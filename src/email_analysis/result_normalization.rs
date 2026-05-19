use anyhow::Result;
use serde_json::Value;

use crate::ai::AnalysisResult;
#[cfg(test)]
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{Database, UpdateAiFieldsInput};

/// DB CHECK values. Normalize AI output to match.
const SPAM_STATUS_ALLOWED: &[&str] = &["spam", "not-spam"];
const PHISHING_STATUS_ALLOWED: &[&str] = &["phishing", "not-phishing"];
const MARKETING_STATUS_ALLOWED: &[&str] = &["marketing", "not-marketing"];
const OTP_STATUS_ALLOWED: &[&str] = &["otp", "magic_link", "password_reset", "not_otp"];
const THREAT_LEVEL_ALLOWED: &[&str] = &["none", "low", "medium", "high", "critical"];

/// Allowed category values (DB CHECK). Normalize AI output to lowercase.
pub(super) const CATEGORY_ALLOWED: &[&str] = &[
    "personal",
    "work",
    "volunteering",
    "financial",
    "shopping",
    "social",
    "travel",
    "health",
    "education",
];
/// Allowed email_type values (DB CHECK). Normalize AI output to lowercase.
pub(super) const EMAIL_TYPE_ALLOWED: &[&str] = &[
    "newsletter",
    "announcement",
    "notification",
    "actionable",
    "conversation",
    "transactional",
    "receipt",
    "reference",
];

pub(super) fn normalize_spam_status(s: Option<&str>) -> Option<&'static str> {
    let s = s?.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let s = if s == "not_spam" {
        "not-spam"
    } else {
        s.as_str()
    };
    SPAM_STATUS_ALLOWED.iter().find(|v| *v == &s).copied()
}

pub(super) fn normalize_phishing_status(s: Option<&str>) -> Option<&'static str> {
    let s = s?.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let s = if s == "not_phishing" {
        "not-phishing"
    } else {
        s.as_str()
    };
    PHISHING_STATUS_ALLOWED.iter().find(|v| *v == &s).copied()
}

pub(super) fn normalize_marketing_status(s: Option<&str>) -> Option<&'static str> {
    let s = s?.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    let s = if s == "not_marketing" {
        "not-marketing"
    } else {
        s.as_str()
    };
    MARKETING_STATUS_ALLOWED.iter().find(|v| *v == &s).copied()
}

/// DB expects not_otp (underscore). AI often returns "not-otp" (hyphen).
pub(super) fn normalize_otp_status(s: Option<&str>) -> Option<&'static str> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    let n = s.to_lowercase().replace('-', "_");
    match n.as_str() {
        "none" | "null" | "na" | "n/a" | "not_applicable" | "not applicable" | "no otp"
        | "not otp" | "non_otp" | "no_otp" => Some("not_otp"),
        _ => OTP_STATUS_ALLOWED.iter().find(|v| *v == &n).copied(),
    }
}

pub(super) fn normalize_threat_level(s: Option<&str>) -> Option<&'static str> {
    let s = s?.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    THREAT_LEVEL_ALLOWED.iter().find(|v| *v == &s).copied()
}

pub(super) fn normalize_category(s: Option<&str>) -> Option<&'static str> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    let normalized = normalize_taxonomy_label(s)?;
    CATEGORY_ALLOWED.iter().find(|v| **v == normalized).copied()
}

pub(super) fn normalize_taxonomy_label(value: &str) -> Option<String> {
    let mut normalized = String::new();
    let mut previous_was_separator = false;
    for ch in value.trim().chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_alphanumeric() {
            normalized.push(ch);
            previous_was_separator = false;
        } else if !previous_was_separator {
            normalized.push('_');
            previous_was_separator = true;
        }
    }

    let normalized = normalized.trim_matches('_').to_string();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

pub(super) fn normalize_email_type(s: Option<&str>) -> Option<String> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    let normalized = normalize_taxonomy_label(s)?;
    EMAIL_TYPE_ALLOWED
        .iter()
        .find(|v| **v == normalized)
        .map(|v| (*v).to_string())
}

pub(super) fn infer_category_from_result(r: &AnalysisResult) -> Option<&'static str> {
    let candidates = [
        r.category.as_deref(),
        r.subcategory.as_deref(),
        r.topic.as_deref(),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(c) = normalize_category(Some(candidate)) {
            return Some(c);
        }
    }
    None
}

pub(super) fn needs_taxonomy_alignment_review(r: &AnalysisResult) -> bool {
    let category = normalize_category(r.category.as_deref());
    if category.is_none() || category == Some("personal") {
        return true;
    }

    normalize_category(r.subcategory.as_deref())
        .is_some_and(|subcategory_category| Some(subcategory_category) != category)
}

pub(super) fn needs_email_type_alignment_review(r: &AnalysisResult) -> bool {
    let Some(email_type) = normalize_email_type(r.email_type.as_deref()) else {
        return true;
    };

    // These values are broad/catch-all enough that valid output can still be
    // semantically wrong. Recheck them with the configured provider instead of
    // guessing from Rust keyword lists.
    matches!(
        email_type.as_str(),
        "newsletter" | "notification" | "transactional"
    )
}

pub(super) fn needs_schema_alignment_review(r: &AnalysisResult) -> bool {
    needs_taxonomy_alignment_review(r) || needs_email_type_alignment_review(r)
}

pub(super) fn needs_classification_reflection(r: &AnalysisResult) -> bool {
    let spam_status = normalize_spam_status(r.spam_status.as_deref());
    let phishing_status = normalize_phishing_status(r.phishing_status.as_deref());
    let marketing_status = normalize_marketing_status(r.marketing_status.as_deref());
    let otp_status = normalize_otp_status(r.otp_status.as_deref());
    let threat_level = normalize_threat_level(r.threat_level.as_deref());

    if spam_status.is_none()
        || phishing_status.is_none()
        || marketing_status.is_none()
        || otp_status.is_none()
        || threat_level.is_none()
    {
        return true;
    }

    if r.confidence.is_none_or(|confidence| confidence < 0.90) {
        return true;
    }

    spam_status == Some("spam")
        || phishing_status == Some("phishing")
        || marketing_status == Some("marketing")
        || otp_status.is_some_and(|status| status != "not_otp")
        || matches!(threat_level, Some("high") | Some("critical"))
}

pub(super) fn apply_schema_alignment_result(
    r: &mut AnalysisResult,
    alignment: &AnalysisResult,
) -> bool {
    let original_category = r.category.clone();
    let original_subcategory = r.subcategory.clone();
    let original_email_type = r.email_type.clone();
    let original_unaligned_category = r
        .category
        .as_deref()
        .filter(|category| normalize_category(Some(category)).is_none())
        .and_then(normalize_taxonomy_label);

    if let Some(category) = normalize_category(alignment.category.as_deref()) {
        r.category = Some(category.to_string());
    }

    if let Some(subcategory) = alignment
        .subcategory
        .as_deref()
        .and_then(normalize_taxonomy_label)
        .filter(|subcategory| !CATEGORY_ALLOWED.contains(&subcategory.as_str()))
    {
        r.subcategory = Some(subcategory);
    } else if let Some(label) = original_unaligned_category {
        let current_is_empty = r
            .subcategory
            .as_deref()
            .map(str::trim)
            .filter(|subcategory| !subcategory.is_empty())
            .is_none();
        let current_is_misplaced_category = normalize_category(r.subcategory.as_deref()).is_some();
        if current_is_empty || current_is_misplaced_category {
            r.subcategory = Some(label);
        }
    }

    if let Some(email_type) = normalize_email_type(alignment.email_type.as_deref()) {
        r.email_type = Some(email_type);
    }

    r.category != original_category
        || r.subcategory != original_subcategory
        || r.email_type != original_email_type
}

pub(super) fn preserve_unaligned_category_as_subcategory(r: &mut AnalysisResult) {
    let Some(raw_category) = r.category.as_deref() else {
        return;
    };
    if normalize_category(Some(raw_category)).is_some() {
        return;
    }
    let Some(label) = normalize_taxonomy_label(raw_category) else {
        return;
    };
    if CATEGORY_ALLOWED.iter().any(|allowed| *allowed == label) {
        return;
    }
    let subcategory_is_empty = r
        .subcategory
        .as_deref()
        .map(str::trim)
        .filter(|subcategory| !subcategory.is_empty())
        .is_none();
    let subcategory_is_misplaced_category = normalize_category(r.subcategory.as_deref()).is_some();
    if subcategory_is_empty || subcategory_is_misplaced_category {
        r.subcategory = Some(label);
    }
}

pub(super) fn infer_email_type_from_result(r: &AnalysisResult) -> Option<String> {
    let candidates = [
        r.email_type.as_deref(),
        r.subcategory.as_deref(),
        r.topic.as_deref(),
        r.organization.as_deref(),
        r.human_summary.as_deref(),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Some(t) = normalize_email_type(Some(candidate)) {
            return Some(t);
        }
    }
    None
}

pub(super) fn infer_otp_status_from_result(r: &AnalysisResult) -> &'static str {
    normalize_otp_status(r.otp_status.as_deref()).unwrap_or("not_otp")
}

pub(super) fn apply_classification_reflection_result(
    r: &mut AnalysisResult,
    reflection: &AnalysisResult,
) -> bool {
    let original = r.clone();

    if let Some(status) = normalize_spam_status(reflection.spam_status.as_deref()) {
        r.spam_status = Some(status.to_string());
    }
    if let Some(status) = normalize_phishing_status(reflection.phishing_status.as_deref()) {
        r.phishing_status = Some(status.to_string());
    }
    if let Some(status) = normalize_marketing_status(reflection.marketing_status.as_deref()) {
        r.marketing_status = Some(status.to_string());
    }
    if let Some(status) = normalize_otp_status(reflection.otp_status.as_deref()) {
        r.otp_status = Some(status.to_string());
        if status == "not_otp" {
            r.otp_code = None;
            r.otp_expires = None;
        }
    }
    if let Some(level) = normalize_threat_level(reflection.threat_level.as_deref()) {
        r.threat_level = Some(level.to_string());
    }

    apply_schema_alignment_result(r, reflection);

    if let Some(threat_indicators) = reflection.threat_indicators.as_ref() {
        r.threat_indicators = Some(threat_indicators.clone());
    }
    if let Some(ai_summary) = reflection.ai_summary.as_ref() {
        r.ai_summary = Some(ai_summary.clone());
    }
    if let Some(human_summary) = reflection
        .human_summary
        .as_deref()
        .map(str::trim)
        .filter(|summary| !summary.is_empty())
    {
        r.human_summary = Some(human_summary.to_string());
    }
    if let Some(organization) = reflection
        .organization
        .as_deref()
        .map(str::trim)
        .filter(|organization| !organization.is_empty())
    {
        r.organization = Some(organization.to_string());
    }
    if let Some(topic) = reflection
        .topic
        .as_deref()
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
    {
        r.topic = Some(topic.to_string());
    }
    if reflection.confidence.is_some() {
        r.confidence = reflection.confidence;
    }
    if let Some(analyzed_by) = r.analyzed_by.as_deref() {
        if !analyzed_by.contains("+classification_reflection") {
            r.analyzed_by = Some(format!("{analyzed_by}+classification_reflection"));
        }
    } else {
        r.analyzed_by = Some("classification_reflection".to_string());
    }

    r.spam_status != original.spam_status
        || r.phishing_status != original.phishing_status
        || r.marketing_status != original.marketing_status
        || r.otp_status != original.otp_status
        || r.otp_code != original.otp_code
        || r.otp_expires != original.otp_expires
        || r.threat_level != original.threat_level
        || r.threat_indicators != original.threat_indicators
        || r.ai_summary != original.ai_summary
        || r.human_summary != original.human_summary
        || r.category != original.category
        || r.subcategory != original.subcategory
        || r.organization != original.organization
        || r.topic != original.topic
        || r.email_type != original.email_type
        || r.confidence != original.confidence
}

/// Update database with analysis result. Mutates `r` only for schema normalization.
#[cfg(test)]
pub async fn apply_analysis_result(
    db: &Database,
    message_id: &str,
    r: &mut AnalysisResult,
) -> Result<u64> {
    apply_analysis_result_for_account(db, DEFAULT_ACCOUNT_ID, message_id, r).await
}

pub async fn apply_analysis_result_for_account(
    db: &Database,
    account_id: &str,
    message_id: &str,
    r: &mut AnalysisResult,
) -> Result<u64> {
    let category = infer_category_from_result(r);
    preserve_unaligned_category_as_subcategory(r);
    let email_type = infer_email_type_from_result(r);
    let otp_status = infer_otp_status_from_result(r);
    r.category = category.map(str::to_string);
    if let Some(ref e) = email_type {
        r.email_type = Some(e.clone());
    }
    r.otp_status = Some(otp_status.to_string());
    if otp_status == "not_otp" {
        // Keep expiry reserved for real auth flows.
        r.otp_expires = None;
    }
    let spam_status = normalize_spam_status(r.spam_status.as_deref());
    let phishing_status = normalize_phishing_status(r.phishing_status.as_deref());
    let marketing_status = normalize_marketing_status(r.marketing_status.as_deref());
    let threat_level = normalize_threat_level(r.threat_level.as_deref());
    if let Some(level) = threat_level {
        r.threat_level = Some(level.to_string());
    }
    let threat_indicators_json = r
        .threat_indicators
        .as_ref()
        .map(|items| Value::Array(items.iter().cloned().map(Value::String).collect()));
    // location_recommendation is not set by this step; it is filled by a separate location analysis (folder path).
    let update_fields = UpdateAiFieldsInput {
        message_id,
        spam_status,
        phishing_status,
        marketing_status,
        otp_status: Some(otp_status),
        otp_code: r.otp_code.as_deref(),
        otp_expires: r.otp_expires,
        threat_level,
        threat_indicators: threat_indicators_json.as_ref(),
        ai_summary: r.ai_summary.as_ref(),
        human_summary: r.human_summary.as_deref(),
        category,
        subcategory: r.subcategory.as_deref(),
        organization: r.organization.as_deref(),
        topic: r.topic.as_deref(),
        email_type: email_type.as_deref(),
        location_recommendation: None, // folder path from location analysis, not classification
        location_create_if_missing: None, // from location analysis, not classification
        offer_expires: r.offer_expires,
    };
    let updated = db
        .update_ai_fields_for_account(account_id, &update_fields)
        .await?;
    let _ = db
        .set_analyzed_by_for_account(account_id, message_id, r.analyzed_by.as_deref())
        .await?;
    Ok(updated)
}
