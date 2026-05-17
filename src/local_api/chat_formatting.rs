use anyhow::{Context, Result};
use serde_json::Value;

use crate::agent_router::ExecutionPlan;
use crate::harness::DB_COMPLETENESS_PENDING_STATUS;

pub(super) fn format_chat_agent_response(
    execution_plan: &ExecutionPlan,
    output: &Value,
) -> Result<String> {
    if is_db_prerequisites_pending_output(output) {
        return format_agent_response(output);
    }

    if output.get("response_markdown").is_some() {
        return format_agent_response(output);
    }

    if execution_plan.visible_agent_name == execution_plan.execution_agent_name {
        return format_agent_response(output);
    }

    match execution_plan.execution_agent_name.as_str() {
        "email-analyzer" => Ok(format_email_analysis_response(output, None)),
        "location-agent" => Ok(format_location_response(output)),
        "folder-consolidator" => Ok(format_folder_consolidation_response(output)),
        "orchestrator" => format_orchestrator_chat_response(output),
        _ => format_agent_response(output),
    }
}

pub(super) fn format_agent_response(output: &Value) -> Result<String> {
    if let Some(markdown) = output
        .get("response_markdown")
        .and_then(|value| value.as_str())
    {
        return Ok(markdown.to_string());
    }
    if let Some(markdown) = output
        .get("digest_markdown")
        .and_then(|value| value.as_str())
    {
        return Ok(markdown.to_string());
    }
    serde_json::to_string_pretty(output).context("serialize agent response")
}

pub(super) fn is_db_prerequisites_pending_output(output: &Value) -> bool {
    output.get("status").and_then(|value| value.as_str()) == Some(DB_COMPLETENESS_PENDING_STATUS)
}

fn format_email_analysis_response(output: &Value, preface: Option<&str>) -> String {
    let summary = output
        .get("human_summary")
        .and_then(|value| value.as_str())
        .or_else(|| output.get("summary").and_then(|value| value.as_str()))
        .unwrap_or("I reviewed the attached email.");

    let phishing_status = output
        .get("phishing_status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let spam_status = output
        .get("spam_status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let marketing_status = output
        .get("marketing_status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let threat_level = output
        .get("threat_level")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let otp_status = output
        .get("otp_status")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let otp_code = output.get("otp_code").and_then(|value| value.as_str());
    let category = output.get("category").and_then(|value| value.as_str());
    let email_type = output.get("email_type").and_then(|value| value.as_str());
    let location_recommendation = output
        .get("location_recommendation")
        .and_then(|value| value.as_str());

    let mut sections = vec!["## Mail Assistant".to_string()];
    if let Some(preface) = preface {
        sections.push(String::new());
        sections.push(preface.to_string());
    }
    sections.push(String::new());
    sections.push(summary.to_string());
    sections.push(String::new());

    let mut bullets = Vec::new();
    if phishing_status == "phishing" {
        bullets.push(format!(
            "- Safety: This looks unsafe and should be treated as phishing (threat level: {}).",
            threat_level
        ));
    } else if matches!(threat_level, "high" | "critical") {
        bullets.push(format!(
            "- Safety: The threat level is `{}`, so this deserves extra caution.",
            threat_level
        ));
    } else if spam_status == "spam" {
        bullets.push(
            "- Safety: This reads like spam rather than a message you need to trust.".to_string(),
        );
    } else {
        bullets.push("- Safety: Nothing in the analysis suggests confirmed phishing.".to_string());
    }

    if let (Some(category), Some(email_type)) = (category, email_type) {
        bullets.push(format!(
            "- Classification: This looks like a `{}` `{}` message.",
            category, email_type
        ));
    }

    if phishing_status == "phishing" || matches!(threat_level, "high" | "critical") {
        bullets.push("- Next step: Do not click links, open attachments, or reply until you verify it through a trusted channel.".to_string());
    } else if otp_status == "otp" {
        if let Some(otp_code) = otp_code {
            bullets.push(format!(
                "- Next step: Use OTP code `{}` only if you initiated the request.",
                otp_code
            ));
        } else {
            bullets.push("- Next step: This appears to be a one-time-code message. Only use it if you initiated the request.".to_string());
        }
    } else if email_type == Some("actionable") {
        bullets.push(
            "- Next step: This looks actionable, so review the requested task or due date."
                .to_string(),
        );
    } else if marketing_status == "marketing" {
        bullets.push("- Next step: No urgent action stands out unless the offer or announcement matters to you.".to_string());
    } else {
        bullets.push("- Next step: No urgent action stands out from the analysis.".to_string());
    }

    if let Some(location_recommendation) = location_recommendation {
        bullets.push(format!(
            "- Filing: A reasonable folder would be `{}`.",
            location_recommendation
        ));
    }

    sections.push(bullets.join("\n"));
    sections.join("\n")
}

fn format_location_response(output: &Value) -> String {
    let folder_path = output
        .get("folder_path")
        .and_then(|value| value.as_str())
        .unwrap_or("a more appropriate folder");
    let create_if_missing = output
        .get("create_if_missing")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let confidence = output
        .get("confidence")
        .and_then(|value| value.as_f64())
        .map(|value| format!("{}%", (value * 100.0).round() as i32))
        .unwrap_or_else(|| "unknown".to_string());
    let reasoning = output
        .get("reasoning")
        .and_then(|value| value.as_str())
        .unwrap_or("The existing folder structure points there.");

    format!(
        "## Mail Assistant\n\nI’d file this under `{}`.\n\n- Confidence: {}\n- Create folder if missing: {}\n- Why: {}",
        folder_path,
        confidence,
        if create_if_missing { "yes" } else { "no" },
        reasoning
    )
}

fn format_folder_consolidation_response(output: &Value) -> String {
    let summary = output
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or("I reviewed the folder structure.");
    let proposals = output
        .get("consolidation_proposals")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let empty_folders = output
        .get("empty_folders")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    let mut lines = vec![
        "## Mail Assistant".to_string(),
        String::new(),
        summary.to_string(),
    ];

    if proposals.is_empty() {
        lines.push(String::new());
        lines.push(
            "I don’t see a strong merge recommendation from the current analysis.".to_string(),
        );
    } else {
        lines.push(String::new());
        lines.push("### Consolidation Ideas".to_string());
        for proposal in proposals.iter().take(5) {
            let source = proposal
                .get("source_folder")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let target = proposal
                .get("target_folder")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");
            let reason = proposal
                .get("reason")
                .and_then(|value| value.as_str())
                .unwrap_or("Similar content profile.");
            let confidence = proposal
                .get("confidence")
                .and_then(|value| value.as_f64())
                .map(|value| format!("{}%", (value * 100.0).round() as i32))
                .unwrap_or_else(|| "unknown confidence".to_string());
            lines.push(format!(
                "- Merge `{}` into `{}` ({}) — {}",
                source, target, confidence, reason
            ));
        }
    }

    if !empty_folders.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "Empty folders: {}",
            empty_folders
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    lines.join("\n")
}

fn format_orchestrator_chat_response(output: &Value) -> Result<String> {
    let Some(result) = output.get("result") else {
        return format_agent_response(output);
    };
    if output.get("task_type").and_then(|value| value.as_str()) != Some("escalation_review") {
        return format_agent_response(output);
    }

    let mut formatted = format_email_analysis_response(
        result,
        Some("I escalated this turn for a higher-judgment review."),
    );
    if let Some(reasoning) = result
        .get("escalation_reasoning")
        .and_then(|value| value.as_str())
    {
        formatted.push_str("\n\n- Review note: ");
        formatted.push_str(reasoning);
    }
    Ok(formatted)
}

pub(super) fn extract_chat_confidence(output: &Value) -> Option<f64> {
    output
        .get("confidence")
        .and_then(|value| value.as_f64())
        .or_else(|| {
            output
                .get("result")
                .and_then(|value| value.get("confidence"))
                .and_then(|value| value.as_f64())
        })
}
