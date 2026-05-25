use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

use crate::ai::AnalysisResult;

use super::result_normalization::{
    normalize_category, normalize_email_type, normalize_marketing_status, normalize_otp_status,
    normalize_phishing_status, normalize_spam_status, normalize_taxonomy_label,
    normalize_threat_level,
};

const DEFAULT_CORPUS_LABEL: &str = "src/email_analysis/fixtures/classification_eval.json";
const DEFAULT_CORPUS: &str = include_str!("fixtures/classification_eval.json");

#[derive(Debug, Deserialize)]
struct ClassificationEvalSuite {
    version: u32,
    minimum_accuracy: f32,
    cases: Vec<ClassificationEvalCase>,
}

#[derive(Debug, Deserialize)]
struct ClassificationEvalCase {
    id: String,
    title: String,
    email: EvalEmail,
    expected: ExpectedClassification,
    analysis: AnalysisResult,
}

#[derive(Debug, Deserialize)]
struct EvalEmail {
    message_id: String,
    subject: Option<String>,
    sender: Option<String>,
    body_text: Option<String>,
    raw_email_content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExpectedClassification {
    spam_status: String,
    phishing_status: String,
    marketing_status: String,
    otp_status: String,
    threat_level: String,
    category: String,
    subcategory: Option<String>,
    email_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClassificationEvalMismatch {
    pub case_id: String,
    pub case_title: String,
    pub field: &'static str,
    pub expected: String,
    pub actual: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClassificationEvalSummary {
    pub corpus_label: String,
    pub cases: usize,
    pub checks: usize,
    pub passed: usize,
    pub minimum_accuracy: f32,
    pub mismatches: Vec<ClassificationEvalMismatch>,
}

impl ClassificationEvalSummary {
    pub(crate) fn accuracy(&self) -> f32 {
        if self.checks == 0 {
            return 0.0;
        }
        self.passed as f32 / self.checks as f32
    }

    pub(crate) fn is_passing(&self) -> bool {
        self.accuracy() + f32::EPSILON >= self.minimum_accuracy
    }
}

#[derive(Debug)]
struct NormalizedExpectedClassification {
    spam_status: String,
    phishing_status: String,
    marketing_status: String,
    otp_status: String,
    threat_level: String,
    category: String,
    subcategory: Option<String>,
    email_type: String,
}

pub(crate) fn run_classification_eval(corpus_path: Option<&Path>) -> Result<()> {
    let summary = match corpus_path {
        Some(path) => {
            let raw = fs::read_to_string(path)
                .with_context(|| format!("read classification eval corpus {}", path.display()))?;
            evaluate_classification_eval_corpus(path.display().to_string(), &raw)?
        }
        None => {
            evaluate_classification_eval_corpus(DEFAULT_CORPUS_LABEL.to_string(), DEFAULT_CORPUS)?
        }
    };

    println!(
        "classification eval: {}/{} checks passed across {} cases ({:.1}% accuracy, minimum {:.1}%)",
        summary.passed,
        summary.checks,
        summary.cases,
        summary.accuracy() * 100.0,
        summary.minimum_accuracy * 100.0
    );
    println!("corpus: {}", summary.corpus_label);

    if summary.mismatches.is_empty() {
        println!("mismatches: none");
    } else {
        println!("mismatches:");
        for mismatch in &summary.mismatches {
            println!(
                "  {} ({}) {}: expected {}, got {}",
                mismatch.case_id,
                mismatch.case_title,
                mismatch.field,
                mismatch.expected,
                mismatch.actual
            );
        }
    }

    if !summary.is_passing() {
        bail!(
            "classification eval failed: {:.1}% accuracy is below {:.1}%",
            summary.accuracy() * 100.0,
            summary.minimum_accuracy * 100.0
        );
    }

    Ok(())
}

pub(crate) fn evaluate_classification_eval_corpus(
    corpus_label: String,
    raw: &str,
) -> Result<ClassificationEvalSummary> {
    let suite: ClassificationEvalSuite = serde_json::from_str(raw)
        .with_context(|| format!("parse classification eval corpus {corpus_label}"))?;
    suite.evaluate(corpus_label)
}

impl ClassificationEvalSuite {
    fn evaluate(&self, corpus_label: String) -> Result<ClassificationEvalSummary> {
        self.validate()?;

        let mut checks = 0usize;
        let mut mismatches = Vec::new();

        for case in &self.cases {
            let expected = case.expected.normalized(&case.id)?;
            case.score(&expected, &mut checks, &mut mismatches);
        }

        let passed = checks.saturating_sub(mismatches.len());
        Ok(ClassificationEvalSummary {
            corpus_label,
            cases: self.cases.len(),
            checks,
            passed,
            minimum_accuracy: self.minimum_accuracy,
            mismatches,
        })
    }

    fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!(
                "unsupported classification eval corpus version {}; expected 1",
                self.version
            );
        }
        if !(0.0..=1.0).contains(&self.minimum_accuracy) {
            bail!(
                "classification eval minimum_accuracy must be between 0.0 and 1.0, got {}",
                self.minimum_accuracy
            );
        }
        if self.cases.is_empty() {
            bail!("classification eval corpus must contain at least one case");
        }
        for case in &self.cases {
            case.validate()?;
        }
        Ok(())
    }
}

impl ClassificationEvalCase {
    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("classification eval case id must not be empty");
        }
        if self.title.trim().is_empty() {
            bail!(
                "classification eval case {} title must not be empty",
                self.id
            );
        }
        if self.email.message_id.trim().is_empty() {
            bail!(
                "classification eval case {} email.message_id must not be empty",
                self.id
            );
        }

        let has_subject = self
            .email
            .subject
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_sender = self
            .email
            .sender
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_body = self
            .email
            .body_text
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            || self
                .email
                .raw_email_content
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty());

        if !has_subject || !has_sender || !has_body {
            bail!(
                "classification eval case {} must include subject, sender, and body/raw evidence",
                self.id
            );
        }

        Ok(())
    }

    fn score(
        &self,
        expected: &NormalizedExpectedClassification,
        checks: &mut usize,
        mismatches: &mut Vec<ClassificationEvalMismatch>,
    ) {
        self.check_label(
            "spam_status",
            &expected.spam_status,
            normalize_spam_status(self.analysis.spam_status.as_deref()).map(str::to_string),
            checks,
            mismatches,
        );
        self.check_label(
            "phishing_status",
            &expected.phishing_status,
            normalize_phishing_status(self.analysis.phishing_status.as_deref()).map(str::to_string),
            checks,
            mismatches,
        );
        self.check_label(
            "marketing_status",
            &expected.marketing_status,
            normalize_marketing_status(self.analysis.marketing_status.as_deref())
                .map(str::to_string),
            checks,
            mismatches,
        );
        self.check_label(
            "otp_status",
            &expected.otp_status,
            normalize_otp_status(self.analysis.otp_status.as_deref()).map(str::to_string),
            checks,
            mismatches,
        );
        self.check_label(
            "threat_level",
            &expected.threat_level,
            normalize_threat_level(self.analysis.threat_level.as_deref()).map(str::to_string),
            checks,
            mismatches,
        );
        self.check_label(
            "category",
            &expected.category,
            normalize_category(self.analysis.category.as_deref()).map(str::to_string),
            checks,
            mismatches,
        );
        self.check_label(
            "email_type",
            &expected.email_type,
            normalize_email_type(self.analysis.email_type.as_deref()),
            checks,
            mismatches,
        );

        if let Some(expected_subcategory) = expected.subcategory.as_deref() {
            self.check_label(
                "subcategory",
                expected_subcategory,
                self.analysis
                    .subcategory
                    .as_deref()
                    .and_then(normalize_taxonomy_label),
                checks,
                mismatches,
            );
        }
    }

    fn check_label(
        &self,
        field: &'static str,
        expected: &str,
        actual: Option<String>,
        checks: &mut usize,
        mismatches: &mut Vec<ClassificationEvalMismatch>,
    ) {
        *checks += 1;
        if actual.as_deref() == Some(expected) {
            return;
        }

        mismatches.push(ClassificationEvalMismatch {
            case_id: self.id.clone(),
            case_title: self.title.clone(),
            field,
            expected: expected.to_string(),
            actual: actual.unwrap_or_else(|| "missing-or-invalid".to_string()),
        });
    }
}

impl ExpectedClassification {
    fn normalized(&self, case_id: &str) -> Result<NormalizedExpectedClassification> {
        Ok(NormalizedExpectedClassification {
            spam_status: normalize_expected_status(
                case_id,
                "spam_status",
                &self.spam_status,
                normalize_spam_status,
            )?,
            phishing_status: normalize_expected_status(
                case_id,
                "phishing_status",
                &self.phishing_status,
                normalize_phishing_status,
            )?,
            marketing_status: normalize_expected_status(
                case_id,
                "marketing_status",
                &self.marketing_status,
                normalize_marketing_status,
            )?,
            otp_status: normalize_expected_status(
                case_id,
                "otp_status",
                &self.otp_status,
                normalize_otp_status,
            )?,
            threat_level: normalize_expected_status(
                case_id,
                "threat_level",
                &self.threat_level,
                normalize_threat_level,
            )?,
            category: normalize_category(Some(&self.category))
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow!(
                        "classification eval case {} expected.category is invalid: {}",
                        case_id,
                        self.category
                    )
                })?,
            subcategory: self
                .subcategory
                .as_deref()
                .map(|value| {
                    normalize_taxonomy_label(value).ok_or_else(|| {
                        anyhow!(
                            "classification eval case {} expected.subcategory is invalid: {}",
                            case_id,
                            value
                        )
                    })
                })
                .transpose()?,
            email_type: normalize_email_type(Some(&self.email_type)).ok_or_else(|| {
                anyhow!(
                    "classification eval case {} expected.email_type is invalid: {}",
                    case_id,
                    self.email_type
                )
            })?,
        })
    }
}

fn normalize_expected_status(
    case_id: &str,
    field: &'static str,
    value: &str,
    normalize: fn(Option<&str>) -> Option<&'static str>,
) -> Result<String> {
    normalize(Some(value)).map(str::to_string).ok_or_else(|| {
        anyhow!(
            "classification eval case {} expected.{} is invalid: {}",
            case_id,
            field,
            value
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_classification_eval_corpus_passes() {
        let summary =
            evaluate_classification_eval_corpus("default".to_string(), DEFAULT_CORPUS).unwrap();

        assert!(summary.is_passing());
        assert_eq!(summary.minimum_accuracy, 1.0);
        assert!(summary.cases >= 4);
        assert_eq!(summary.mismatches, Vec::new());
    }

    #[test]
    fn classification_eval_reports_label_mismatches() {
        let raw = r#"{
          "version": 1,
          "minimum_accuracy": 1.0,
          "cases": [
            {
              "id": "mismatch-example",
              "title": "Mismatch example",
              "email": {
                "message_id": "mismatch@example",
                "subject": "Security newsletter",
                "sender": "newsletter@example.com",
                "body_text": "A legitimate fraud education newsletter."
              },
              "expected": {
                "spam_status": "not-spam",
                "phishing_status": "not-phishing",
                "marketing_status": "not-marketing",
                "otp_status": "not_otp",
                "threat_level": "none",
                "category": "financial",
                "email_type": "newsletter"
              },
              "analysis": {
                "spam_status": "not-spam",
                "phishing_status": "phishing",
                "marketing_status": "not-marketing",
                "otp_status": "not_otp",
                "threat_level": "high",
                "category": "financial",
                "email_type": "newsletter"
              }
            }
          ]
        }"#;

        let summary = evaluate_classification_eval_corpus("inline".to_string(), raw).unwrap();

        assert!(!summary.is_passing());
        assert_eq!(summary.checks, 7);
        assert_eq!(summary.passed, 5);
        assert_eq!(summary.mismatches.len(), 2);
        assert_eq!(summary.mismatches[0].field, "phishing_status");
        assert_eq!(summary.mismatches[1].field, "threat_level");
    }
}
