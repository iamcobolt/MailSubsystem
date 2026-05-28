use std::sync::OnceLock;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use regex::Regex;
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;
pub type ApiJsonResult<T> = ApiResult<Json<T>>;

#[derive(Debug)]
pub struct ApiError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: redact_error_message(&message.into()),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: redact_error_message(&message.into()),
        }
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: redact_error_message(&message.into()),
        }
    }

    pub fn too_many_requests(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: redact_error_message(&message.into()),
        }
    }

    pub fn request_timeout(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::REQUEST_TIMEOUT,
            message: redact_error_message(&message.into()),
        }
    }

    pub fn internal(error: impl std::fmt::Display) -> Self {
        let message = redact_error_message(&format!("{error:#}"));
        log::warn!("local API internal error: {}", message);
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": redact_error_message(&self.message) })),
        )
            .into_response()
    }
}

pub(crate) fn redact_error_message(message: &str) -> String {
    let redacted = email_regex()
        .replace_all(message, "[redacted-email]")
        .into_owned();
    labeled_pii_regex()
        .replace_all(&redacted, "$1: [redacted]")
        .into_owned()
}

fn email_regex() -> &'static Regex {
    static EMAIL_RE: OnceLock<Regex> = OnceLock::new();
    EMAIL_RE.get_or_init(|| {
        Regex::new(r"(?i)\b[a-z0-9._%+\-]+@[a-z0-9.\-]+\.[a-z]{2,}\b")
            .expect("compile email redaction regex")
    })
}

fn labeled_pii_regex() -> &'static Regex {
    static LABELED_PII_RE: OnceLock<Regex> = OnceLock::new();
    LABELED_PII_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(subject|body|snippet)\s*[:=]\s*[^;\n]+")
            .expect("compile labeled PII redaction regex")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_email_subject_and_body_snippets() {
        let redacted = redact_error_message(
            "sender alice@example.com subject: Payroll changes; body=secret contents",
        );

        assert!(!redacted.contains("alice@example.com"));
        assert!(!redacted.contains("Payroll changes"));
        assert!(!redacted.contains("secret contents"));
        assert!(redacted.contains("[redacted-email]"));
        assert!(redacted.contains("subject: [redacted]"));
        assert!(redacted.contains("body: [redacted]"));
    }

    #[test]
    fn internal_error_stores_redacted_message() {
        let error = ApiError::internal(anyhow::anyhow!(
            "failed for bob@example.com subject: Merger details"
        ));

        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!error.message.contains("bob@example.com"));
        assert!(!error.message.contains("Merger details"));
    }
}
