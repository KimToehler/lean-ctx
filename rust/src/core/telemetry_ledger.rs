//! Local append-only ledger of sent telemetry heartbeats.
//!
//! Every successful heartbeat is recorded as a JSON line in
//! `<state_dir>/telemetry_heartbeats.jsonl`. This gives the user full
//! transparency over what was sent and when — visible in the dashboard
//! and via `lean-ctx telemetry history`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HeartbeatRecord {
    pub timestamp: String,
    pub installation_id: String,
    pub version: String,
    pub os: String,
    pub arch: String,
}

fn ledger_path() -> Result<PathBuf, String> {
    crate::core::paths::state_dir().map(|d| d.join("telemetry_heartbeats.jsonl"))
}

pub(crate) fn append(record: &HeartbeatRecord) -> Result<(), String> {
    let path = ledger_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create state dir: {e}"))?;
    }
    let mut line =
        serde_json::to_string(record).map_err(|e| format!("JSON serialization error: {e}"))?;
    line.push('\n');

    use std::io::Write;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("Cannot open telemetry ledger: {e}"))?;
    let mut writer = std::io::BufWriter::new(file);
    writer
        .write_all(line.as_bytes())
        .map_err(|e| format!("Cannot write to telemetry ledger: {e}"))?;
    Ok(())
}

pub(crate) fn read_all() -> Vec<HeartbeatRecord> {
    let Ok(path) = ledger_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub(crate) fn purge_local() -> Result<(), String> {
    let path = ledger_path()?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Cannot purge telemetry ledger: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_read_roundtrip() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        let record = HeartbeatRecord {
            timestamp: "2026-07-30T23:00:00Z".to_string(),
            installation_id: "test-uuid-1234".to_string(),
            version: "3.9.13".to_string(),
            os: "macos".to_string(),
            arch: "aarch64".to_string(),
        };
        append(&record).unwrap();
        append(&record).unwrap();
        let all = read_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].installation_id, "test-uuid-1234");
        assert_eq!(all[1].version, "3.9.13");
        purge_local().expect("purge ledger");
        assert!(read_all().is_empty());
    }
}
