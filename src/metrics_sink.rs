//! Lightweight metrics facade with pluggable sinks.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

pub trait MetricsSink: Send + Sync {
    fn counter(&self, name: &str, value: u64, labels: &[(&str, &str)]);
    fn gauge(&self, name: &str, value: f64, labels: &[(&str, &str)]);
    fn histogram(&self, name: &str, value: f64, labels: &[(&str, &str)]);
}

#[derive(Debug, Default)]
pub struct NoopMetricsSink;

impl MetricsSink for NoopMetricsSink {
    fn counter(&self, _name: &str, _value: u64, _labels: &[(&str, &str)]) {}

    fn gauge(&self, _name: &str, _value: f64, _labels: &[(&str, &str)]) {}

    fn histogram(&self, _name: &str, _value: f64, _labels: &[(&str, &str)]) {}
}

#[derive(Debug, Default)]
pub struct LogMetricsSink;

impl MetricsSink for LogMetricsSink {
    fn counter(&self, name: &str, value: u64, labels: &[(&str, &str)]) {
        let labels_json = serde_json::to_string(&labels).unwrap_or_else(|_| "[]".to_string());
        log::info!(
            target: "metrics",
            "metric type=counter name={} value={} labels={}",
            name,
            value,
            labels_json
        );
    }

    fn gauge(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        let labels_json = serde_json::to_string(&labels).unwrap_or_else(|_| "[]".to_string());
        log::info!(
            target: "metrics",
            "metric type=gauge name={} value={} labels={}",
            name,
            value,
            labels_json
        );
    }

    fn histogram(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        let labels_json = serde_json::to_string(&labels).unwrap_or_else(|_| "[]".to_string());
        log::info!(
            target: "metrics",
            "metric type=histogram name={} value={} labels={}",
            name,
            value,
            labels_json
        );
    }
}

#[derive(Debug)]
pub struct FileMetricsSink {
    path: PathBuf,
    file: Mutex<File>,
}

impl FileMetricsSink {
    pub fn new(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    fn write_event(&self, kind: &str, name: &str, value: f64, labels: &[(&str, &str)]) {
        let labels_obj: serde_json::Map<String, serde_json::Value> = labels
            .iter()
            .map(|(k, v)| {
                (
                    (*k).to_string(),
                    serde_json::Value::String((*v).to_string()),
                )
            })
            .collect();
        let event = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "type": kind,
            "name": name,
            "value": value,
            "labels": labels_obj,
        });
        let mut file = match self.file.lock() {
            Ok(lock) => lock,
            Err(err) => {
                log::error!(
                    target: "metrics",
                    "metric_file_sink_lock_error path={} error={}",
                    self.path.display(),
                    err
                );
                return;
            }
        };
        if writeln!(file, "{}", event).is_err() || file.flush().is_err() {
            log::error!(
                target: "metrics",
                "metric_file_sink_write_error path={} event={}",
                self.path.display(),
                event
            );
        }
    }
}

impl MetricsSink for FileMetricsSink {
    fn counter(&self, name: &str, value: u64, labels: &[(&str, &str)]) {
        self.write_event("counter", name, value as f64, labels);
    }

    fn gauge(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        self.write_event("gauge", name, value, labels);
    }

    fn histogram(&self, name: &str, value: f64, labels: &[(&str, &str)]) {
        self.write_event("histogram", name, value, labels);
    }
}

static METRICS_SINK: OnceLock<Arc<dyn MetricsSink>> = OnceLock::new();

pub fn init_default_metrics() {
    let sink_name = std::env::var("METRICS_SINK").unwrap_or_else(|_| "none".to_string());
    let sink: Arc<dyn MetricsSink> = if sink_name.eq_ignore_ascii_case("file") {
        let path = std::env::var("METRICS_FILE_PATH").unwrap_or_else(|_| "metrics.jsonl".into());
        match FileMetricsSink::new(path.clone()) {
            Ok(file_sink) => Arc::new(file_sink),
            Err(err) => {
                log::error!(
                    target: "metrics",
                    "metric_file_sink_init_error path={} error={}",
                    path,
                    err
                );
                Arc::new(LogMetricsSink)
            }
        }
    } else if sink_name.eq_ignore_ascii_case("log") {
        Arc::new(LogMetricsSink)
    } else {
        Arc::new(NoopMetricsSink)
    };
    let _ = METRICS_SINK.set(sink);
}

pub fn counter(name: &str, value: u64, labels: &[(&str, &str)]) {
    if let Some(sink) = METRICS_SINK.get() {
        sink.counter(name, value, labels);
    }
}

pub fn gauge(name: &str, value: f64, labels: &[(&str, &str)]) {
    if let Some(sink) = METRICS_SINK.get() {
        sink.gauge(name, value, labels);
    }
}

pub fn histogram(name: &str, value: f64, labels: &[(&str, &str)]) {
    if let Some(sink) = METRICS_SINK.get() {
        sink.histogram(name, value, labels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_metrics_sink_writes_jsonl() {
        let path = std::env::temp_dir().join(format!(
            "mailsubsystem-metrics-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let sink = FileMetricsSink::new(path.clone()).expect("create file sink");
        sink.counter("test_counter", 2, &[("step", "analyze")]);
        sink.gauge("test_gauge", 4.5, &[("status", "ok")]);
        sink.histogram("test_hist", 1.25, &[("phase", "sync")]);

        let body = std::fs::read_to_string(&path).expect("read metrics file");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        for line in lines {
            let value: serde_json::Value =
                serde_json::from_str(line).expect("line should be valid JSON");
            assert!(value.get("ts").and_then(|v| v.as_str()).is_some());
            assert!(value.get("type").and_then(|v| v.as_str()).is_some());
            assert!(value.get("name").and_then(|v| v.as_str()).is_some());
            assert!(value.get("value").is_some());
            assert!(value.get("labels").and_then(|v| v.as_object()).is_some());
        }

        let _ = std::fs::remove_file(path);
    }
}
