//! Run-level observability bootstrap and correlation IDs.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Once, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::metrics;
use log::{LevelFilter, Log, Metadata, Record};

static OBS_INIT: Once = Once::new();
static RUN_SEQ: AtomicU64 = AtomicU64::new(1);
static RUN_CONTEXT: OnceLock<RunContext> = OnceLock::new();
const DEFAULT_FILE_FILTER: &str = "info,rig=warn,sqlx=warn";
const DEFAULT_CONSOLE_FILTER: &str = "warn";

#[derive(Debug, Clone)]
pub struct RunContext {
    pub run_id: String,
    pub command: String,
}

fn json_logs_enabled() -> bool {
    if let Ok(format) = std::env::var("LOG_FORMAT") {
        if format.eq_ignore_ascii_case("json") {
            return true;
        }
    }
    std::env::var("LOG_JSON")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn build_run_id() -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("run-{}-{}-{}", std::process::id(), now_ms, seq)
}

fn current_context_fields() -> (&'static str, &'static str) {
    if let Some(ctx) = RUN_CONTEXT.get() {
        (ctx.run_id.as_str(), ctx.command.as_str())
    } else {
        ("-", "-")
    }
}

fn build_json_log_line(
    ts: &str,
    level: &str,
    target: &str,
    run_id: &str,
    command: &str,
    msg: &str,
) -> String {
    serde_json::json!({
        "ts": ts,
        "level": level,
        "target": target,
        "run_id": run_id,
        "command": command,
        "msg": msg,
    })
    .to_string()
}

fn logging_filter(env_key: &str, fallback_env_key: Option<&str>, default: &str) -> String {
    std::env::var(env_key)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            fallback_env_key.and_then(|key| {
                std::env::var(key)
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
        })
        .unwrap_or_else(|| default.to_string())
}

fn default_log_file_path(command: &str) -> PathBuf {
    let log_dir = std::env::var("LOG_DIR").unwrap_or_else(|_| "logs".to_string());
    PathBuf::from(log_dir).join(format!("{}.log", command))
}

fn log_file_path(command: &str) -> Option<PathBuf> {
    match std::env::var("LOG_FILE_PATH") {
        Ok(value) if value.eq_ignore_ascii_case("off") || value.eq_ignore_ascii_case("false") => {
            None
        }
        Ok(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => Some(default_log_file_path(command)),
    }
}

fn configure_logger(
    filter: &str,
    json_logs: bool,
    target: env_logger::Target,
) -> env_logger::Logger {
    let mut builder = env_logger::Builder::new();
    builder.parse_filters(filter).target(target);
    builder.format(move |buf, record| {
        let ts = chrono::Utc::now().to_rfc3339();
        let (run_id, command) = current_context_fields();
        let msg = record.args().to_string();
        if json_logs {
            let line = build_json_log_line(
                &ts,
                &record.level().as_str().to_lowercase(),
                record.target(),
                run_id,
                command,
                &msg,
            );
            writeln!(buf, "{}", line)
        } else {
            writeln!(
                buf,
                "{} [{}] [{}] run_id={} command={} {}",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                record.target(),
                run_id,
                command,
                msg
            )
        }
    });
    builder.build()
}

struct SplitLogger {
    console: env_logger::Logger,
    file: Option<env_logger::Logger>,
}

impl SplitLogger {
    fn max_filter(&self) -> LevelFilter {
        let file_filter = self
            .file
            .as_ref()
            .map(|logger| logger.filter())
            .unwrap_or(LevelFilter::Off);
        self.console.filter().max(file_filter)
    }
}

impl Log for SplitLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.console.enabled(metadata)
            || self
                .file
                .as_ref()
                .map(|logger| logger.enabled(metadata))
                .unwrap_or(false)
    }

    fn log(&self, record: &Record<'_>) {
        self.console.log(record);
        if let Some(file) = self.file.as_ref() {
            file.log(record);
        }
    }

    fn flush(&self) {
        self.console.flush();
        if let Some(file) = self.file.as_ref() {
            file.flush();
        }
    }
}

pub fn init_observability(command: &str) -> RunContext {
    let context = RunContext {
        run_id: build_run_id(),
        command: command.to_string(),
    };
    let _ = RUN_CONTEXT.set(context.clone());

    OBS_INIT.call_once(|| {
        let json_logs = json_logs_enabled();
        let console_filter = logging_filter("LOG_CONSOLE_FILTER", None, DEFAULT_CONSOLE_FILTER);
        let file_filter = logging_filter("LOG_FILE_FILTER", Some("RUST_LOG"), DEFAULT_FILE_FILTER);
        let console = configure_logger(&console_filter, json_logs, env_logger::Target::Stderr);
        let file = log_file_path(command).and_then(|path| {
            if let Some(parent) = path.parent() {
                if let Err(error) = fs::create_dir_all(parent) {
                    eprintln!(
                        "failed to create log directory {}: {}",
                        parent.display(),
                        error
                    );
                    return None;
                }
            }
            match OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => Some(configure_logger(
                    &file_filter,
                    json_logs,
                    env_logger::Target::Pipe(Box::new(file)),
                )),
                Err(error) => {
                    eprintln!("failed to open log file {}: {}", path.display(), error);
                    None
                }
            }
        });
        let logger = SplitLogger { console, file };
        let max_filter = logger.max_filter();
        log::set_boxed_logger(Box::new(logger)).expect("initialize logger");
        log::set_max_level(max_filter);
        metrics::init_default_metrics();
    });

    log::info!("command_start");
    metrics::counter("command_runs_total", 1, &[("command", command)]);

    context
}

pub fn finish_run(context: &RunContext, result: &anyhow::Result<()>, elapsed: Duration) {
    let status = if result.is_ok() { "ok" } else { "error" };
    metrics::counter(
        "command_runs_finished_total",
        1,
        &[("command", context.command.as_str()), ("status", status)],
    );
    metrics::histogram(
        "command_duration_seconds",
        elapsed.as_secs_f64(),
        &[("command", context.command.as_str()), ("status", status)],
    );
    match result {
        Ok(()) => log::info!("command_finish duration_secs={:.3}", elapsed.as_secs_f64()),
        Err(err) => log::error!(
            "command_finish_error duration_secs={:.3} error={}",
            elapsed.as_secs_f64(),
            err
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_log_line_includes_run_and_command() {
        let line = build_json_log_line(
            "2026-01-01T00:00:00Z",
            "info",
            "test",
            "run-123",
            "daemon",
            "hello",
        );
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(v.get("run_id").and_then(|x| x.as_str()), Some("run-123"));
        assert_eq!(v.get("command").and_then(|x| x.as_str()), Some("daemon"));
        assert_eq!(v.get("msg").and_then(|x| x.as_str()), Some("hello"));
    }
}
