//! Confidence calibration: per-worker accuracy tracking and threshold auto-adjustment.
//!
//! Computes optimal escalation thresholds based on observed confidence distributions
//! and writes the result to the agent scratchpad for the harness to read at runtime.

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;

use super::state::AgentState;
use crate::db::Database;

/// Minimum escalation threshold — never lower than this regardless of calibration.
const MIN_THRESHOLD: f32 = 0.50;
/// Maximum escalation threshold — never higher than this.
const MAX_THRESHOLD: f32 = 0.95;
/// Minimum number of completed runs required before calibration produces a result.
const MIN_SAMPLES: i64 = 20;
/// Default number of recent runs to consider for percentile computation.
pub const DEFAULT_SAMPLE_LIMIT: i64 = 200;
/// If escalation rate exceeds this, lower the threshold (too many escalations).
const TARGET_ESCALATION_RATE_HIGH: f64 = 0.20;
/// If escalation rate is below this, raise the threshold (too few escalations).
const TARGET_ESCALATION_RATE_LOW: f64 = 0.05;
/// Exponential smoothing factor: weight given to the newly computed value.
const SMOOTHING_FACTOR: f32 = 0.7;

#[derive(Debug, Clone)]
pub struct CalibrationResult {
    pub agent_name: String,
    pub account_id: String,
    pub sample_count: i64,
    pub previous_threshold: Option<f32>,
    pub new_threshold: f32,
    pub p50: f64,
    pub p75: f64,
    pub p95: f64,
    pub escalation_rate: f64,
    pub skipped: bool,
    pub skip_reason: Option<String>,
}

/// Run confidence calibration for a single agent in a single account.
///
/// Reads recent confidence statistics from `agent_runs`, computes an optimal
/// escalation threshold, and writes it to the scratchpad key `calibration_{agent_name}`.
///
/// Returns `CalibrationResult` with `skipped = true` if insufficient data.
pub async fn calibrate_agent(
    db: &Database,
    account_id: &str,
    agent_name: &str,
    spec_threshold: f32,
    sample_limit: Option<i64>,
) -> Result<CalibrationResult> {
    let limit = sample_limit.unwrap_or(DEFAULT_SAMPLE_LIMIT);

    let stats = db
        .get_confidence_stats_for_account(account_id, agent_name, limit)
        .await
        .context("load confidence stats for calibration")?;

    // Not enough data — skip calibration, keep spec default
    let Some(stats) = stats else {
        return Ok(CalibrationResult {
            agent_name: agent_name.to_string(),
            account_id: account_id.to_string(),
            sample_count: 0,
            previous_threshold: None,
            new_threshold: spec_threshold,
            p50: 0.0,
            p75: 0.0,
            p95: 0.0,
            escalation_rate: 0.0,
            skipped: true,
            skip_reason: Some("no completed runs with confidence data".to_string()),
        });
    };

    if stats.sample_count < MIN_SAMPLES {
        return Ok(CalibrationResult {
            agent_name: agent_name.to_string(),
            account_id: account_id.to_string(),
            sample_count: stats.sample_count,
            previous_threshold: None,
            new_threshold: spec_threshold,
            p50: stats.p50,
            p75: stats.p75,
            p95: stats.p95,
            escalation_rate: stats.escalation_rate,
            skipped: true,
            skip_reason: Some(format!(
                "insufficient samples ({}, need {})",
                stats.sample_count, MIN_SAMPLES
            )),
        });
    }

    // Read current calibrated threshold from scratchpad (if any previous calibration exists)
    let state = AgentState::new(Arc::new(db.clone()), account_id, agent_name);
    let calibration_key = format!("calibration_{}", agent_name);
    let previous = state
        .read_scratchpad(&calibration_key)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.get("threshold").and_then(|t| t.as_f64()))
        .map(|t| t as f32);
    let current = previous.unwrap_or(spec_threshold);

    // Compute target threshold based on escalation rate
    let computed = if stats.escalation_rate > TARGET_ESCALATION_RATE_HIGH {
        // Too many escalations — lower threshold toward p50 (accept more results)
        stats.p50 as f32
    } else if stats.escalation_rate < TARGET_ESCALATION_RATE_LOW {
        // Too few escalations — raise threshold toward p75 (be stricter)
        stats.p75 as f32
    } else {
        // In target band (5-20%) — keep current threshold
        current
    };

    // Apply exponential smoothing to avoid oscillation
    let smoothed = SMOOTHING_FACTOR * computed + (1.0 - SMOOTHING_FACTOR) * current;
    let clamped = smoothed.clamp(MIN_THRESHOLD, MAX_THRESHOLD);

    // Write calibration result to scratchpad
    let value = json!({
        "threshold": clamped,
        "computed_at": Utc::now().to_rfc3339(),
        "sample_count": stats.sample_count,
        "p50": stats.p50,
        "p75": stats.p75,
        "p95": stats.p95,
        "escalation_rate": stats.escalation_rate,
        "spec_default": spec_threshold,
    });
    state
        .write_scratchpad(&calibration_key, value, None)
        .await
        .context("write calibration to scratchpad")?;

    Ok(CalibrationResult {
        agent_name: agent_name.to_string(),
        account_id: account_id.to_string(),
        sample_count: stats.sample_count,
        previous_threshold: previous,
        new_threshold: clamped,
        p50: stats.p50,
        p75: stats.p75,
        p95: stats.p95,
        escalation_rate: stats.escalation_rate,
        skipped: false,
        skip_reason: None,
    })
}

/// Pure computation of the calibrated threshold — for unit testing without DB.
pub fn compute_calibrated_threshold(current: f32, p50: f64, p75: f64, escalation_rate: f64) -> f32 {
    let computed = if escalation_rate > TARGET_ESCALATION_RATE_HIGH {
        p50 as f32
    } else if escalation_rate < TARGET_ESCALATION_RATE_LOW {
        p75 as f32
    } else {
        current
    };
    let smoothed = SMOOTHING_FACTOR * computed + (1.0 - SMOOTHING_FACTOR) * current;
    smoothed.clamp(MIN_THRESHOLD, MAX_THRESHOLD)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibration_insufficient_samples_skips() {
        // This tests the pure computation path — the async calibrate_agent
        // handles the skip logic, but we can verify bounds here.
        let result = compute_calibrated_threshold(0.75, 0.80, 0.90, 0.10);
        // In-band (10% escalation rate) — should keep current
        assert!((result - 0.75).abs() < 0.01);
    }

    #[test]
    fn calibration_high_escalation_lowers_threshold() {
        // escalation_rate = 30% (> 20%) — should move toward p50
        let result = compute_calibrated_threshold(0.75, 0.60, 0.85, 0.30);
        // 0.7 * 0.60 + 0.3 * 0.75 = 0.42 + 0.225 = 0.645
        assert!((result - 0.645).abs() < 0.01);
        assert!(result < 0.75); // Must be lower than current
    }

    #[test]
    fn calibration_low_escalation_raises_threshold() {
        // escalation_rate = 2% (< 5%) — should move toward p75
        let result = compute_calibrated_threshold(0.70, 0.65, 0.88, 0.02);
        // 0.7 * 0.88 + 0.3 * 0.70 = 0.616 + 0.21 = 0.826
        assert!((result - 0.826).abs() < 0.01);
        assert!(result > 0.70); // Must be higher than current
    }

    #[test]
    fn calibration_in_band_preserves_threshold() {
        let result = compute_calibrated_threshold(0.75, 0.65, 0.88, 0.10);
        // In-band — computed = current = 0.75
        // 0.7 * 0.75 + 0.3 * 0.75 = 0.75
        assert!((result - 0.75).abs() < 0.01);
    }

    #[test]
    fn calibration_clamps_to_lower_bound() {
        // Very low p50 with high escalation rate
        let result = compute_calibrated_threshold(0.50, 0.30, 0.40, 0.50);
        assert!(result >= MIN_THRESHOLD);
    }

    #[test]
    fn calibration_clamps_to_upper_bound() {
        // Very high p75 with low escalation rate
        let result = compute_calibrated_threshold(0.95, 0.90, 0.99, 0.01);
        assert!(result <= MAX_THRESHOLD);
    }

    #[test]
    fn calibration_smoothing_prevents_large_jumps() {
        // Current at 0.75, computed would be p50=0.50 (high escalation)
        let result = compute_calibrated_threshold(0.75, 0.50, 0.85, 0.30);
        // 0.7 * 0.50 + 0.3 * 0.75 = 0.35 + 0.225 = 0.575
        // Jump from 0.75 to 0.575 — not all the way to 0.50
        assert!(result > 0.55);
        assert!(result < 0.60);
    }
}
