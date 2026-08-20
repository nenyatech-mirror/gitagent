//! Lightweight telemetry — appends structured run events to
//! `<dir>/.gitagent/telemetry.jsonl` when `GITAGENT_TELEMETRY=1`. This is a
//! self-contained JSONL sink, not an OTLP exporter; the same call sites can be
//! pointed at OpenTelemetry later without changing the loop.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub struct Telemetry {
    path: PathBuf,
}

impl Telemetry {
    /// Enabled only when `GITAGENT_TELEMETRY=1`; otherwise None (zero overhead).
    pub fn new(dir: &Path) -> Option<Telemetry> {
        if std::env::var("GITAGENT_TELEMETRY").ok().as_deref() != Some("1") {
            return None;
        }
        Some(Telemetry { path: dir.join(".gitagent").join("telemetry.jsonl") })
    }

    pub fn record(&self, event: &str, data: Value) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{}", json!({ "event": event, "data": data }));
        }
    }
}
