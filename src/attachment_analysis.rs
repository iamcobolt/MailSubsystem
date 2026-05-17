//! Email attachment extraction and lightweight analysis.

use anyhow::{Context, Result};
use mailparse::{parse_mail, ParsedMail};
use serde_json::Value;

pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: usize,
    pub content: Vec<u8>,
}

#[derive(Default)]
pub struct AttachmentAnalysis {
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: usize,
    pub extracted_text: Option<String>,
    pub vision_description: Option<String>,
    pub ocr_text: Option<String>,
    pub analysis: String,
    pub is_executable: bool,
    pub malware_detected: bool,
    pub threat_type: Option<String>,
    pub contains_sensitive_data: bool,
}

impl AttachmentAnalysis {
    pub fn to_json_value(&self) -> Value {
        serde_json::json!({
            "filename": self.filename,
            "mime_type": self.mime_type,
            "size_bytes": self.size_bytes,
            "extracted_text": self.extracted_text,
            "vision_description": self.vision_description,
            "ocr_text": self.ocr_text,
            "analysis": self.analysis,
            "is_executable": self.is_executable,
            "malware_detected": self.malware_detected,
            "threat_type": self.threat_type,
            "contains_sensitive_data": self.contains_sensitive_data,
        })
    }
}

pub struct AttachmentProcessor {
    pub max_attachment_size_bytes: usize,
}

impl Default for AttachmentProcessor {
    fn default() -> Self {
        Self {
            max_attachment_size_bytes: 25 * 1024 * 1024,
        }
    }
}

impl AttachmentProcessor {
    pub fn extract_attachments(&self, raw_email: &[u8]) -> Result<Vec<Attachment>> {
        let parsed = parse_mail(raw_email).context("parse email")?;
        let mut out = Vec::new();
        self.collect_attachments(&parsed, &mut out)?;
        Ok(out)
    }

    fn collect_attachments(&self, part: &ParsedMail, out: &mut Vec<Attachment>) -> Result<()> {
        let mime_type = part
            .headers
            .iter()
            .find(|h: &&mailparse::MailHeader| h.get_key().eq_ignore_ascii_case("Content-Type"))
            .map(|h| {
                h.get_value()
                    .split(';')
                    .next()
                    .unwrap_or("text/plain")
                    .trim()
                    .to_lowercase()
            })
            .unwrap_or_else(|| "text/plain".to_string());
        let disp = part.headers.iter().find(|h: &&mailparse::MailHeader| {
            h.get_key().eq_ignore_ascii_case("Content-Disposition")
        });
        let is_attachment = disp
            .map(|d| d.get_value().to_lowercase().contains("attachment"))
            .unwrap_or(false);
        let filename = disp
            .and_then(|d| {
                d.get_value().split(';').find_map(|s: &str| {
                    let s = s.trim();
                    if s.to_lowercase().starts_with("filename=") {
                        Some(s[9..].trim_matches('"').to_string())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_else(|| format!("attachment_{}", out.len()));
        let body = part.get_body_raw().context("body")?;
        if body.is_empty() {
            for sub in &part.subparts {
                self.collect_attachments(sub, out)?;
            }
            return Ok(());
        }
        let decoded = part.get_body().map(|s| s.into_bytes()).unwrap_or(body);
        if decoded.len() > self.max_attachment_size_bytes {
            return Ok(());
        }
        let is_attach_mime = mime_type.starts_with("image/")
            || mime_type == "application/pdf"
            || mime_type.contains("wordprocessingml")
            || mime_type == "application/x-msdownload"
            || mime_type == "application/zip";
        if is_attachment || is_attach_mime {
            out.push(Attachment {
                filename,
                mime_type: mime_type.clone(),
                size_bytes: decoded.len(),
                content: decoded,
            });
        } else {
            for sub in &part.subparts {
                self.collect_attachments(sub, out)?;
            }
        }
        Ok(())
    }

    pub fn summarize_attachment(&self, attachment: &Attachment) -> AttachmentAnalysis {
        let mime = attachment.mime_type.trim().to_lowercase();
        let malware = (mime.contains("x-msdownload") || mime.contains("x-executable"))
            && attachment.content.len() >= 2
            && attachment.content.starts_with(b"MZ");
        AttachmentAnalysis {
            filename: attachment.filename.clone(),
            mime_type: attachment.mime_type.clone(),
            size_bytes: attachment.size_bytes,
            analysis: if malware {
                "Suspicious executable".to_string()
            } else {
                "Attachment".to_string()
            },
            is_executable: mime.contains("x-msdownload")
                || mime.contains("x-executable")
                || mime == "application/zip",
            malware_detected: malware,
            threat_type: if malware {
                Some("Windows executable".to_string())
            } else {
                None
            },
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_multipart_with_attachment() -> Vec<u8> {
        let raw = b"MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=bound\r\n\
\r\n\
--bound\r\n\
Content-Type: text/plain\r\n\
\r\n\
body here\r\n\
--bound\r\n\
Content-Type: application/octet-stream; name=\"doc.txt\"\r\n\
Content-Disposition: attachment; filename=\"doc.txt\"\r\n\
\r\n\
attachment content\r\n\
--bound--\r\n";
        raw.to_vec()
    }

    #[test]
    fn test_extract_attachments_finds_one() {
        let proc = AttachmentProcessor::default();
        let raw = simple_multipart_with_attachment();
        let list = proc.extract_attachments(&raw).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].filename, "doc.txt");
        assert!(
            list[0].mime_type.contains("octet") || list[0].mime_type == "application/octet-stream"
        );
        let body = String::from_utf8_lossy(&list[0].content);
        assert!(
            body.trim_end().ends_with("attachment content"),
            "body: {:?}",
            body
        );
        assert_eq!(list[0].size_bytes, list[0].content.len());
    }

    #[test]
    fn test_analyze_attachment_executable_mz() {
        let proc = AttachmentProcessor::default();
        let exe = Attachment {
            filename: "bad.exe".to_string(),
            mime_type: "application/x-msdownload".to_string(),
            size_bytes: 10,
            content: b"MZ\x90\x00\x01\x00\x00\x00".to_vec(),
        };
        let out = proc.summarize_attachment(&exe);
        assert!(out.is_executable);
        assert!(out.malware_detected);
        assert!(out.analysis.contains("Suspicious"));
    }

    #[test]
    fn test_analyze_attachment_pdf_not_executable() {
        let proc = AttachmentProcessor::default();
        let pdf = Attachment {
            filename: "doc.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            size_bytes: 4,
            content: b"%PDF".to_vec(),
        };
        let out = proc.summarize_attachment(&pdf);
        assert!(!out.malware_detected);
        assert!(!out.is_executable);
    }

    #[test]
    fn test_attachment_analysis_to_json_value() {
        let a = AttachmentAnalysis {
            filename: "x".to_string(),
            mime_type: "image/png".to_string(),
            size_bytes: 100,
            analysis: "ok".to_string(),
            is_executable: false,
            malware_detected: false,
            ..Default::default()
        };
        let v = a.to_json_value();
        assert_eq!(v["filename"], "x");
        assert_eq!(v["mime_type"], "image/png");
        assert_eq!(v["malware_detected"], false);
    }
}
