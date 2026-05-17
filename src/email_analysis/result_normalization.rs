use anyhow::Result;
use serde_json::Value;

use crate::ai::AnalysisResult;
#[cfg(test)]
use crate::config::DEFAULT_ACCOUNT_ID;
use crate::db::{Database, EmailRecord, UpdateAiFieldsInput};

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
    if contains_any(
        &n,
        &[
            "none",
            "null",
            "na",
            "n/a",
            "not_applicable",
            "not applicable",
            "no otp",
            "not otp",
            "non_otp",
        ],
    ) {
        return Some("not_otp");
    }
    OTP_STATUS_ALLOWED.iter().find(|v| *v == &n).copied()
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

pub(super) fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
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
    let candidates = [
        r.human_summary.as_deref(),
        r.topic.as_deref(),
        r.subcategory.as_deref(),
        r.email_type.as_deref(),
        r.organization.as_deref(),
    ];
    for candidate in candidates.into_iter().flatten() {
        let lower = candidate.to_lowercase();
        if contains_any(
            &lower,
            &[
                "password reset",
                "reset your password",
                "reset password",
                "reset code",
                "recover password",
            ],
        ) {
            return "password_reset";
        }
        if contains_any(
            &lower,
            &[
                "magic link",
                "sign-in link",
                "signin link",
                "login link",
                "secure link",
            ],
        ) {
            return "magic_link";
        }
        if contains_any(
            &lower,
            &[
                "otp",
                "one-time",
                "one time",
                "verification code",
                "passcode",
                "auth code",
                "authentication code",
                "2fa code",
                "mfa code",
            ],
        ) {
            return "otp";
        }
    }
    if let Some(s) = normalize_otp_status(r.otp_status.as_deref()) {
        return s;
    }
    "not_otp"
}

/// True if the analysis indicates scam/fraud (so we may treat as phishing when model said not-phishing).
pub(super) fn indicates_scam_or_fraud(r: &AnalysisResult) -> bool {
    let lower = |s: Option<&String>| s.map(|s| s.to_lowercase());
    let cat = lower(r.category.as_ref()).unwrap_or_default();
    let typ = lower(r.email_type.as_ref()).unwrap_or_default();
    let topic = lower(r.topic.as_ref()).unwrap_or_default();
    let sub = lower(r.subcategory.as_ref()).unwrap_or_default();
    cat.contains("scam")
        || cat.contains("fraud")
        || typ.contains("scam")
        || typ.contains("fraud")
        || topic.contains("scam")
        || topic.contains("fraud")
        || sub.contains("scam")
        || sub.contains("fraud")
}

/// True when the model's own structured output says this is useful account/admin
/// guidance rather than disposable promotional noise.
pub(super) fn indicates_recipient_useful_admin_guidance(r: &AnalysisResult) -> bool {
    let category = normalize_category(r.category.as_deref());
    let category_supports_guidance = matches!(
        category,
        Some("personal") | Some("financial") | Some("health") | Some("education")
    );
    if !category_supports_guidance {
        return false;
    }

    let email_type = normalize_email_type(r.email_type.as_deref());
    let type_supports_guidance = matches!(
        email_type.as_deref(),
        Some("newsletter")
            | Some("notification")
            | Some("transactional")
            | Some("reference")
            | Some("actionable")
    );
    if !type_supports_guidance {
        return false;
    }

    let evidence = [
        r.subcategory.as_deref(),
        r.topic.as_deref(),
        r.human_summary.as_deref(),
        r.organization.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();

    let admin_context = contains_any(
        &evidence,
        &[
            "account",
            "admin",
            "administration",
            "property",
            "landlord",
            "tenant",
            "housing",
            "legal",
            "government",
            "tax",
            "financial",
            "health",
            "utility",
            "safety",
            "compliance",
            "regulatory",
            "policy",
        ],
    );
    let useful_guidance = contains_any(
        &evidence,
        &[
            "guidance",
            "guide",
            "explainer",
            "update",
            "deadline",
            "renewal",
            "renew",
            "certificate",
            "check",
            "rights",
            "law",
            "rules",
            "obligation",
            "required",
            "mandatory",
            "statement",
            "notice",
            "status",
        ],
    );

    admin_context && useful_guidance
}

/// True when the structured output describes an alert the recipient configured,
/// rather than a broad promotional campaign.
pub(super) fn indicates_user_configured_alert(r: &AnalysisResult) -> bool {
    let email_type = normalize_email_type(r.email_type.as_deref());
    let type_supports_alert = matches!(
        email_type.as_deref(),
        Some("notification")
            | Some("newsletter")
            | Some("transactional")
            | Some("actionable")
            | Some("reference")
    );
    if !type_supports_alert {
        return false;
    }

    let evidence = [
        r.subcategory.as_deref(),
        r.topic.as_deref(),
        r.human_summary.as_deref(),
        r.organization.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();

    contains_any(
        &evidence,
        &[
            "saved search",
            "saved searches",
            "watch list",
            "watch-list",
            "watchlist",
            "job alert",
            "price alert",
            "calendar alert",
            "account alert",
            "credit alert",
            "matches your preferences",
            "match your preferences",
            "matches your saved",
            "matching your saved",
            "user-configured alert",
            "user configured alert",
        ],
    )
}

pub(super) fn email_evidence_indicates_user_configured_alert(email: &EmailRecord) -> bool {
    let evidence = [
        email.subject.as_deref(),
        email.body_text.as_deref(),
        email.raw_email_content.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();

    contains_any(
        &evidence,
        &[
            "saved search",
            "saved searches",
            "savedsearch",
            "manage search alerts",
            "change email alert",
            "unsave a search",
            "watch list alert",
            "watch-list alert",
            "watchlist alert",
            "watchlist notification",
            "matches your saved",
            "match your saved",
            "matching your saved",
            "your job alert",
            "calendar alerts for",
            "credit alert to review",
        ],
    )
}

pub(super) fn email_evidence_indicates_signature_only_nonmarketing(
    email: &EmailRecord,
    r: &AnalysisResult,
) -> bool {
    if top_level_headers_contain_list_header(email.raw_email_content.as_deref()) {
        return false;
    }

    let source_text = [
        email.subject.as_deref(),
        email.body_text.as_deref(),
        email.raw_email_content.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    let source_lower = source_text.to_lowercase();
    let structured_lower = [
        r.category.as_deref(),
        r.subcategory.as_deref(),
        r.topic.as_deref(),
        r.organization.as_deref(),
        r.email_type.as_deref(),
        r.human_summary.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_lowercase();
    let ai_summary_lower = analysis_result_summary_text(r).to_lowercase();
    let payload_evidence = format!("{structured_lower} {ai_summary_lower}");

    if contains_commercial_payload_signal(&payload_evidence)
        || contains_commercial_payload_signal(email.subject.as_deref().unwrap_or(""))
    {
        return false;
    }

    let email_type = normalize_email_type(r.email_type.as_deref());
    let transactional_payload = matches!(
        email_type.as_deref(),
        Some("transactional") | Some("receipt") | Some("reference")
    ) || contains_transactional_payload_signal(&payload_evidence)
        || contains_transactional_payload_signal(email.subject.as_deref().unwrap_or(""))
        || contains_transactional_payload_signal(&source_lower);

    let forwarded_payload = email
        .subject
        .as_deref()
        .map(|subject| subject.trim_start().to_lowercase().starts_with("fwd:"))
        .unwrap_or(false)
        || source_lower.contains("begin forwarded message:");

    if forwarded_payload && transactional_payload {
        return true;
    }

    sender_looks_individual(email.sender.as_deref())
        && contains_direct_personal_request_signal(&source_lower)
}

pub(super) fn email_evidence_indicates_payload_marketing(
    email: &EmailRecord,
    r: &AnalysisResult,
) -> bool {
    let subject = email.subject.as_deref().unwrap_or("");
    let ai_summary = analysis_result_summary_text(r);
    let payload_evidence = [
        r.category.as_deref(),
        r.subcategory.as_deref(),
        r.topic.as_deref(),
        r.organization.as_deref(),
        r.email_type.as_deref(),
        r.human_summary.as_deref(),
        Some(ai_summary.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");

    contains_commercial_payload_signal(subject)
        || contains_commercial_payload_signal(&payload_evidence)
}

pub(super) fn top_level_headers_contain_list_header(raw: Option<&str>) -> bool {
    let Some(raw) = raw else {
        return false;
    };
    let headers = raw
        .split_once("\r\n\r\n")
        .map(|(headers, _)| headers)
        .or_else(|| raw.split_once("\n\n").map(|(headers, _)| headers))
        .unwrap_or(raw);
    headers.lines().any(|line| {
        let lower = line.trim_start().to_ascii_lowercase();
        lower.starts_with("list-id:") || lower.starts_with("list-unsubscribe:")
    })
}

pub(super) fn analysis_result_summary_text(r: &AnalysisResult) -> String {
    match r.ai_summary.as_ref() {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

pub(super) fn contains_commercial_payload_signal(value: &str) -> bool {
    let lower = value.to_lowercase();
    contains_any(
        &lower,
        &[
            "free session",
            "guest pass",
            "discount",
            "sale",
            "coupon",
            "promo code",
            "loyalty reward",
            "limited time",
            "hours only",
            "book now",
            "book today",
            "offer redemption",
            "paid service",
            "book a consultation",
            "schedule a demo",
            "upgrade now",
            "upgrade today",
            "upgrade to ",
            "partner introduction",
            "partner offer",
            "introducing our new",
            "new partners",
            "preferred partner",
            "sponsored by",
            "sponsored post",
            "lead generation",
            "lead-generation",
            "sales campaign",
        ],
    )
}

pub(super) fn contains_transactional_payload_signal(value: &str) -> bool {
    let lower = value.to_lowercase();
    contains_any(
        &lower,
        &[
            "order delivery",
            "order_delivery",
            "delivery notification",
            "scheduled for delivery",
            "delivery of your order",
            "your order",
            "has arrived",
            "have arrived",
            "appointment confirmation",
            "receipt",
            "invoice",
            "booking confirmation",
            "tracking",
            "shipped",
            "delivered",
        ],
    )
}

pub(super) fn contains_direct_personal_request_signal(value: &str) -> bool {
    contains_any(
        value,
        &[
            "can you ",
            "could you ",
            "would you ",
            "please print",
            "please send",
            "please check",
            "please review",
            "please look",
            "thanks",
            "thank you",
        ],
    ) && contains_any(
        value,
        &[
            " print",
            " pdf",
            " attachment",
            " document",
            " call",
            " send",
            " check",
            " review",
            " look",
        ],
    )
}

pub(super) fn sender_looks_individual(sender: Option<&str>) -> bool {
    let Some(sender) = sender else {
        return false;
    };
    let sender = sender.trim().to_lowercase();
    if contains_any(
        &sender,
        &[
            "noreply",
            "no-reply",
            "newsletter",
            "marketing",
            "promo",
            "offers",
            "support",
            "info@",
            "sales@",
        ],
    ) {
        return false;
    }
    let domain = sender.rsplit('@').next().unwrap_or("");
    contains_any(
        domain,
        &[
            "icloud.com",
            "me.com",
            "gmail.com",
            "googlemail.com",
            "outlook.com",
            "hotmail.com",
            "live.com",
            "yahoo.com",
            "proton.me",
            "protonmail.com",
            "fastmail.com",
        ],
    )
}

pub(super) fn mentions_user_configured_alert_basis(value: &str) -> bool {
    let lower = value.to_lowercase();
    contains_any(
        &lower,
        &[
            "saved search",
            "saved searches",
            "watch list",
            "watch-list",
            "watchlist",
            "job alert",
            "calendar alert",
            "account alert",
            "credit alert",
            "user-configured alert",
            "user configured alert",
            "matching your preferences",
            "matches your preferences",
        ],
    )
}

pub(super) fn append_summary_sentence(summary: &mut Option<String>, sentence: &str) {
    match summary {
        Some(existing) if !existing.trim().is_empty() => {
            let trimmed = existing.trim_end();
            let separator = if trimmed.ends_with(['.', '!', '?']) {
                " "
            } else {
                ". "
            };
            *existing = format!("{trimmed}{separator}{sentence}");
        }
        _ => {
            *summary = Some(sentence.to_string());
        }
    }
}

pub(super) fn json_summary_mentions(value: &Value, predicate: fn(&str) -> bool) -> bool {
    match value {
        Value::String(text) => predicate(text),
        other => predicate(&other.to_string()),
    }
}

pub(super) fn append_json_summary_sentence(summary: &mut Option<Value>, sentence: &str) {
    match summary {
        Some(Value::String(existing)) if !existing.trim().is_empty() => {
            let trimmed = existing.trim_end();
            let separator = if trimmed.ends_with(['.', '!', '?']) {
                " "
            } else {
                ". "
            };
            *existing = format!("{trimmed}{separator}{sentence}");
        }
        Some(existing) => {
            let text = existing.to_string();
            let trimmed = text.trim_end();
            let separator = if trimmed.ends_with(['.', '!', '?']) {
                " "
            } else {
                ". "
            };
            *existing = Value::String(format!("{trimmed}{separator}{sentence}"));
        }
        None => {
            *summary = Some(Value::String(sentence.to_string()));
        }
    }
}

pub(super) fn ensure_user_configured_alert_summary_detail(r: &mut AnalysisResult) {
    const HUMAN_NOTE: &str =
        "This appears tied to a saved search or user-configured alert, not a generic promotion.";
    const AI_NOTE: &str =
        "Source evidence indicates this is tied to a saved search or user-configured alert, which supports not-spam while preserving marketing for commercial listings.";

    if !r
        .human_summary
        .as_deref()
        .map(mentions_user_configured_alert_basis)
        .unwrap_or(false)
    {
        append_summary_sentence(&mut r.human_summary, HUMAN_NOTE);
    }
    if !r
        .ai_summary
        .as_ref()
        .map(|summary| json_summary_mentions(summary, mentions_user_configured_alert_basis))
        .unwrap_or(false)
    {
        append_json_summary_sentence(&mut r.ai_summary, AI_NOTE);
    }
}

/// Update database with analysis result. Mutates `r` so normalized/overridden values match what we write to DB.
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
    let mut email_type = infer_email_type_from_result(r);
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
    let mut spam_status = normalize_spam_status(r.spam_status.as_deref());
    let mut phishing_status = normalize_phishing_status(r.phishing_status.as_deref());
    if spam_status == Some("spam")
        && phishing_status == Some("not-phishing")
        && indicates_scam_or_fraud(r)
    {
        phishing_status = Some("phishing");
        r.phishing_status = Some("phishing".to_string());
    }
    let mut marketing_status = normalize_marketing_status(r.marketing_status.as_deref());
    let threat_level = normalize_threat_level(r.threat_level.as_deref());
    let threat_blocks_non_destructive_repair = phishing_status == Some("phishing")
        || matches!(threat_level, Some("high") | Some("critical"));
    let source_email = if !threat_blocks_non_destructive_repair
        && (spam_status == Some("spam") || marketing_status.is_some())
    {
        db.get_email_by_message_id_for_account(account_id, message_id)
            .await?
    } else {
        None
    };
    let is_user_configured_alert = indicates_user_configured_alert(r);
    let source_indicates_user_configured_alert =
        if spam_status == Some("spam") && !threat_blocks_non_destructive_repair {
            source_email
                .as_ref()
                .map(email_evidence_indicates_user_configured_alert)
                .unwrap_or(false)
        } else {
            false
        };
    let source_indicates_signature_only_nonmarketing =
        if marketing_status == Some("marketing") && !threat_blocks_non_destructive_repair {
            source_email
                .as_ref()
                .map(|email| email_evidence_indicates_signature_only_nonmarketing(email, r))
                .unwrap_or(false)
        } else {
            false
        };
    if spam_status == Some("spam")
        && !threat_blocks_non_destructive_repair
        && (indicates_recipient_useful_admin_guidance(r)
            || is_user_configured_alert
            || source_indicates_user_configured_alert
            || source_indicates_signature_only_nonmarketing)
    {
        spam_status = Some("not-spam");
        r.spam_status = Some("not-spam".to_string());
    }
    if source_indicates_signature_only_nonmarketing {
        marketing_status = Some("not-marketing");
        r.marketing_status = Some("not-marketing".to_string());
    }
    if marketing_status == Some("not-marketing")
        && !source_indicates_signature_only_nonmarketing
        && !threat_blocks_non_destructive_repair
        && source_email
            .as_ref()
            .map(|email| email_evidence_indicates_payload_marketing(email, r))
            .unwrap_or(false)
    {
        marketing_status = Some("marketing");
        r.marketing_status = Some("marketing".to_string());
    }
    if (is_user_configured_alert || source_indicates_user_configured_alert)
        && email_type.as_deref() == Some("newsletter")
    {
        email_type = Some("notification".to_string());
        r.email_type = Some("notification".to_string());
    }
    if is_user_configured_alert || source_indicates_user_configured_alert {
        ensure_user_configured_alert_summary_detail(r);
    }
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
