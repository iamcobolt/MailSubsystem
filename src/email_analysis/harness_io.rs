use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::ai::{AnalysisResult, TokenUsage};
use crate::attachments::AttachmentProcessor;
use crate::db::EmailRecord;
use crate::harness::RunResult;

#[cfg(test)]
pub(super) fn email_to_harness_input(email: &EmailRecord) -> Value {
    email_to_harness_input_with_worker_instruction(email, None)
}

pub(super) fn email_to_harness_input_with_worker_instruction(
    email: &EmailRecord,
    worker_instruction: Option<&str>,
) -> Value {
    let attachment_summaries = email_attachment_summaries(email);
    let mut input = json!({
        "message_id": email.message_id.as_str(),
        "subject": email.subject.as_deref(),
        "sender": email.sender.as_deref(),
        "received_date": email.received_date.as_ref().map(|date| date.to_rfc3339()),
        "body_text": email
            .body_text
            .as_deref()
            .map(|body| truncate_str(body, 12_000).to_string()),
        "message_size": email.message_size,
        "attachment_count": attachment_summaries.len(),
        "attachments": attachment_summaries,
        "thread_ids": &email.related_message_ids,
        "is_read": email.is_read,
        "list_id": extract_header_value(email.raw_email_content.as_deref(), "list-id"),
        "list_unsubscribe": extract_header_value(
            email.raw_email_content.as_deref(),
            "list-unsubscribe",
        ),
        "x_priority": extract_header_value(
            email.raw_email_content.as_deref(),
            "x-priority",
        ),
        "reply_to": extract_header_value(email.raw_email_content.as_deref(), "reply-to"),
    });
    if let Some(instruction) = worker_instruction
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if let Some(object) = input.as_object_mut() {
            object.insert(
                "orchestrator_context".to_string(),
                Value::String(instruction.to_string()),
            );
        }
    }
    input
}

fn extract_header_value(raw: Option<&str>, header_name: &str) -> Option<String> {
    let raw = raw?;
    let needle = header_name.to_ascii_lowercase();
    let mut current_name: Option<String> = None;
    let mut current_value = String::new();
    let flush = |name: &Option<String>, value: &str| -> Option<String> {
        if name
            .as_ref()
            .map(|n| n.eq_ignore_ascii_case(&needle))
            .unwrap_or(false)
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        None
    };

    for line in raw.lines().take(400) {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            if let Some(v) = flush(&current_name, &current_value) {
                return Some(v);
            }
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if current_name.is_some() {
                if !current_value.is_empty() {
                    current_value.push(' ');
                }
                current_value.push_str(line.trim());
            }
            continue;
        }
        if let Some(v) = flush(&current_name, &current_value) {
            return Some(v);
        }
        current_name = None;
        current_value.clear();
        if let Some((name, value)) = line.split_once(':') {
            current_name = Some(name.trim().to_ascii_lowercase());
            current_value.push_str(value.trim());
        }
    }
    flush(&current_name, &current_value)
}

fn email_attachment_summaries(email: &EmailRecord) -> Vec<Value> {
    let Some(raw) = email.raw_email_content.as_deref() else {
        return Vec::new();
    };
    let processor = AttachmentProcessor::default();
    match processor.extract_attachments(raw.as_bytes()) {
        Ok(attachments) => attachments
            .iter()
            .map(|attachment| processor.summarize_attachment(attachment).to_json_value())
            .collect(),
        Err(error) => {
            log::debug!(
                "Failed to parse attachments for {}: {}",
                email.message_id,
                error
            );
            Vec::new()
        }
    }
}

pub(super) fn truncate_str(input: &str, max_chars: usize) -> &str {
    let end = input
        .char_indices()
        .nth(max_chars)
        .map(|(index, _)| index)
        .unwrap_or(input.len());
    &input[..end]
}

pub(super) fn harness_output_to_analysis_result(
    result: RunResult,
    email: &EmailRecord,
) -> Result<AnalysisResult> {
    let confidence = result
        .output
        .get("confidence")
        .and_then(|value| value.as_f64())
        .map(|value| value as f32);
    let otp_expires = result
        .output
        .get("otp_expires_minutes")
        .and_then(|value| value.as_i64())
        .and_then(|minutes| {
            email
                .received_date
                .as_ref()
                .map(|received| *received + chrono::Duration::minutes(minutes.max(0)))
        });

    let mut analysis: AnalysisResult = serde_json::from_value(result.output.clone())
        .context("deserialize harness output to AnalysisResult")?;
    if analysis.ai_summary.is_none() {
        analysis.ai_summary = Some(result.output.clone());
    }
    analysis.analyzed_by = Some(format!("harness:{}", result.run_id));
    analysis.token_usage = Some(TokenUsage {
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
    });
    analysis.confidence = confidence;
    if otp_expires.is_some() {
        analysis.otp_expires = otp_expires;
    }
    Ok(analysis)
}

pub(super) fn orchestrator_output_to_analysis_result(
    result: RunResult,
    email: &EmailRecord,
) -> Result<AnalysisResult> {
    let output = result
        .output
        .get("result")
        .cloned()
        .context("orchestrator result missing")?;
    let confidence = output
        .get("confidence")
        .and_then(|value| value.as_f64())
        .map(|value| value as f32);
    let otp_expires = output
        .get("otp_expires_minutes")
        .and_then(|value| value.as_i64())
        .and_then(|minutes| {
            email
                .received_date
                .as_ref()
                .map(|received| *received + chrono::Duration::minutes(minutes.max(0)))
        });

    let mut analysis: AnalysisResult =
        serde_json::from_value(output.clone()).context("deserialize orchestrator output")?;
    if analysis.ai_summary.is_none() {
        analysis.ai_summary = Some(output);
    }
    analysis.analyzed_by = Some(format!("orchestrator:{}", result.run_id));
    analysis.token_usage = Some(TokenUsage {
        input_tokens: result.input_tokens,
        output_tokens: result.output_tokens,
    });
    analysis.confidence = confidence;
    if otp_expires.is_some() {
        analysis.otp_expires = otp_expires;
    }
    Ok(analysis)
}
