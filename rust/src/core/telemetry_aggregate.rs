//! Privacy-safe daily telemetry aggregation.

use sha2::Digest;
use std::path::PathBuf;
use std::sync::OnceLock;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::installation_id;
use super::telemetry_v2::{
    Architecture, ClientFamily, DistributionChannel, HeartbeatMetrics, Histogram, MAX_COUNT,
    OperatingSystem, SCHEMA_VERSION, SessionMetrics, TelemetryBatchV2, TelemetryEnvelopeV2,
    TelemetryEventV2, ToolUsageMetrics,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CounterCheckpoint {
    tool_calls: u64,
    tool_failures: u64,
    tool_latency_buckets: [u64; crate::core::telemetry::TOOL_LATENCY_BUCKET_UPPER_MS.len()],
    session_uptime_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AggregateState {
    process_nonce: String,
    acknowledged: CounterCheckpoint,
    pending: Option<PendingBatch>,
    #[serde(default)]
    last_sent_bucket: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingBatch {
    batch: TelemetryBatchV2,
    observed: CounterCheckpoint,
    process_nonce: String,
}

pub struct DailySendLease {
    batch: TelemetryBatchV2,
    state_path: PathBuf,
    _lock: std::fs::File,
}

impl DailySendLease {
    pub fn batch(&self) -> &TelemetryBatchV2 {
        &self.batch
    }

    pub fn commit(self) -> Result<(), String> {
        let state = acknowledge_state(load_state_at(&self.state_path)?, &self.batch)?;
        write_state(&self.state_path, &state)
    }
}

pub fn pending_daily_batch() -> Result<TelemetryBatchV2, String> {
    preview_daily_batch()
}

/// Build the exact currently eligible payload without advancing durable state.
pub fn preview_daily_batch() -> Result<TelemetryBatchV2, String> {
    let state = load_state()?;
    if let Some(pending) = state.pending {
        return Ok(pending.batch);
    }
    let observed = current_checkpoint();
    build_from_current_counters(&state, &observed)
}

/// Freeze one payload until the sender explicitly acknowledges success.
#[cfg(test)]
fn prepare_daily_batch() -> Result<TelemetryBatchV2, String> {
    with_locked_state(|mut state| {
        if let Some(pending) = &state.pending {
            return Ok((state.clone(), pending.batch.clone()));
        }
        let observed = current_checkpoint();
        let batch = build_from_current_counters(&state, &observed)?;
        state.pending = Some(PendingBatch {
            batch: batch.clone(),
            observed,
            process_nonce: process_nonce().to_string(),
        });
        Ok((state, batch))
    })
}

/// Hold the cross-process state lease until network, ledger, and acknowledgement finish.
pub fn begin_daily_send() -> Result<DailySendLease, String> {
    let path = state_path()?;
    ensure_parent(&path)?;
    let lock = open_state_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry aggregate state: {error}"))?;
    let mut state = load_state()?;
    let batch = if let Some(pending) = &state.pending {
        pending.batch.clone()
    } else {
        let observed = current_checkpoint();
        let batch = build_from_current_counters(&state, &observed)?;
        state.pending = Some(PendingBatch {
            batch: batch.clone(),
            observed,
            process_nonce: process_nonce().to_string(),
        });
        write_state(&path, &state)?;
        batch
    };
    Ok(DailySendLease {
        batch,
        state_path: path,
        _lock: lock,
    })
}

/// Advance counters only when the exact frozen payload was accepted remotely.
#[cfg(test)]
fn acknowledge_daily_batch(batch: &TelemetryBatchV2) -> Result<(), String> {
    with_locked_state(|state| Ok((acknowledge_state(state, batch)?, ())))
}

fn acknowledge_state(
    mut state: AggregateState,
    batch: &TelemetryBatchV2,
) -> Result<AggregateState, String> {
    let pending = state
        .pending
        .take()
        .ok_or_else(|| "no prepared telemetry batch to acknowledge".to_string())?;
    if pending.batch != *batch {
        return Err("telemetry acknowledgement does not match pending batch".to_string());
    }
    state.process_nonce = pending.process_nonce;
    state.acknowledged = pending.observed;
    state.last_sent_bucket = batch
        .events
        .first()
        .map(|event| event.timestamp_bucket.clone());
    Ok(state)
}

pub fn last_sent_bucket() -> Option<String> {
    load_state().ok().and_then(|state| state.last_sent_bucket)
}

fn build_from_current_counters(
    state: &AggregateState,
    observed: &CounterCheckpoint,
) -> Result<TelemetryBatchV2, String> {
    let (installation_id, deletion_token) = installation_id::get_or_create_identity()
        .map_err(|error| format!("installation ID unavailable: {error}"))?;
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    build_daily_aggregate(
        installation_id,
        hex::encode(sha2::Sha256::digest(deletion_token.as_bytes())),
        date,
        distribution_channel(),
        client_family(),
        state,
        observed,
    )
}

fn build_daily_aggregate(
    installation_id: String,
    deletion_token_hash: String,
    timestamp_bucket: String,
    distribution_channel: DistributionChannel,
    client_family: ClientFamily,
    state: &AggregateState,
    observed: &CounterCheckpoint,
) -> Result<TelemetryBatchV2, String> {
    let empty = CounterCheckpoint::default();
    let baseline = if state.process_nonce == process_nonce() {
        &state.acknowledged
    } else {
        &empty
    };
    let latency_counts = bounded_histogram_delta(
        &observed.tool_latency_buckets,
        &baseline.tool_latency_buckets,
    );
    let calls = latency_counts.iter().sum();
    debug_assert_eq!(
        calls,
        observed
            .tool_calls
            .saturating_sub(baseline.tool_calls)
            .min(MAX_COUNT)
    );
    let failures = observed
        .tool_failures
        .saturating_sub(baseline.tool_failures)
        .min(calls);
    let duration = observed
        .session_uptime_secs
        .saturating_sub(baseline.session_uptime_secs)
        .min(MAX_COUNT);
    let mut batch = build_daily_heartbeat(
        installation_id,
        deletion_token_hash,
        timestamp_bucket,
        distribution_channel,
        client_family,
    )?;
    let common = batch.events[0].clone();
    batch.events.push(envelope_like(
        &common,
        TelemetryEventV2::SessionAggregate(SessionMetrics {
            sessions: 1,
            duration_seconds: single_observation_histogram(
                duration,
                &[
                    60,
                    300,
                    900,
                    3_600,
                    14_400,
                    86_400,
                    604_800,
                    31_536_000,
                    i64::MAX as u64,
                ],
                1,
            ),
        }),
    ));
    batch.events.push(envelope_like(
        &common,
        TelemetryEventV2::ToolUsageAggregate(ToolUsageMetrics {
            calls,
            failures,
            latency_milliseconds: Histogram {
                upper_bounds: crate::core::telemetry::TOOL_LATENCY_BUCKET_UPPER_MS.to_vec(),
                counts: latency_counts.to_vec(),
            },
        }),
    ));
    batch
        .validate()
        .map_err(|error| format!("invalid telemetry batch: {error:?}"))?;
    Ok(batch)
}

fn bounded_histogram_delta<const N: usize>(observed: &[u64; N], baseline: &[u64; N]) -> [u64; N] {
    let mut remaining = MAX_COUNT;
    std::array::from_fn(|index| {
        let count = observed[index]
            .saturating_sub(baseline[index])
            .min(remaining);
        remaining -= count;
        count
    })
}

fn envelope_like(template: &TelemetryEnvelopeV2, event: TelemetryEventV2) -> TelemetryEnvelopeV2 {
    TelemetryEnvelopeV2 {
        schema_version: template.schema_version,
        timestamp_bucket: template.timestamp_bucket.clone(),
        installation_id: template.installation_id.clone(),
        account_id: template.account_id.clone(),
        organization_id: template.organization_id.clone(),
        app_version: template.app_version.clone(),
        event,
    }
}

fn single_observation_histogram(value: u64, bounds: &[u64], count: u64) -> Histogram {
    let mut counts = vec![0; bounds.len()];
    let index = bounds
        .iter()
        .position(|bound| value <= *bound)
        .unwrap_or(bounds.len() - 1);
    counts[index] = count.min(MAX_COUNT);
    Histogram {
        upper_bounds: bounds.to_vec(),
        counts,
    }
}

fn current_checkpoint() -> CounterCheckpoint {
    let snapshot = crate::core::telemetry::global_metrics().daily_telemetry_snapshot();
    CounterCheckpoint {
        tool_calls: snapshot.tool_calls,
        tool_failures: snapshot.tool_failures,
        tool_latency_buckets: snapshot.tool_latency_buckets,
        session_uptime_secs: snapshot.session_uptime_secs,
    }
}

fn process_nonce() -> &'static str {
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

fn state_path() -> Result<PathBuf, String> {
    crate::core::paths::state_dir().map(|dir| dir.join("telemetry_v2_aggregate.json"))
}

pub fn purge_local_state() -> Result<(), String> {
    purge_local_state_then(|| Ok(()))
}

pub fn purge_local_state_then<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let path = state_path()?;
    ensure_parent(&path)?;
    let lock = open_state_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry aggregate state: {error}"))?;
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot purge telemetry aggregate state: {error}")),
    }
    operation()
}

fn load_state() -> Result<AggregateState, String> {
    let path = state_path()?;
    load_state_at(&path)
}

fn load_state_at(path: &std::path::Path) -> Result<AggregateState, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid telemetry aggregate state: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(AggregateState::default()),
        Err(error) => Err(format!("cannot read telemetry aggregate state: {error}")),
    }
}

#[cfg(test)]
fn with_locked_state<T>(
    operation: impl FnOnce(AggregateState) -> Result<(AggregateState, T), String>,
) -> Result<T, String> {
    let path = state_path()?;
    ensure_parent(&path)?;
    let lock = open_state_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry aggregate state: {error}"))?;
    let (state, value) = operation(load_state_at(&path)?)?;
    write_state(&path, &state)?;
    Ok(value)
}

fn ensure_parent(path: &std::path::Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "telemetry state path has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create telemetry state directory: {error}"))
}

fn write_state(path: &std::path::Path, state: &AggregateState) -> Result<(), String> {
    let bytes = serde_json::to_vec(&state)
        .map_err(|error| format!("cannot serialize telemetry aggregate state: {error}"))?;
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        Some(std::fs::Permissions::from_mode(0o600))
    };
    #[cfg(not(unix))]
    let permissions: Option<std::fs::Permissions> = None;
    crate::core::atomic_fs::try_atomic_write(&path, &bytes, permissions.as_ref())
        .map_err(|error| format!("cannot persist telemetry aggregate state: {error}"))
}

fn open_state_lock(path: &std::path::Path) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))
        .map_err(|error| format!("cannot open telemetry state lock: {error}"))
}

pub fn build_daily_heartbeat(
    installation_id: String,
    deletion_token_hash: String,
    timestamp_bucket: String,
    distribution_channel: DistributionChannel,
    client_family: ClientFamily,
) -> Result<TelemetryBatchV2, String> {
    let batch = TelemetryBatchV2 {
        schema_version: SCHEMA_VERSION,
        deletion_token_hash,
        events: vec![TelemetryEnvelopeV2 {
            schema_version: SCHEMA_VERSION,
            timestamp_bucket,
            installation_id,
            account_id: None,
            organization_id: None,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            event: TelemetryEventV2::Heartbeat(HeartbeatMetrics {
                distribution_channel,
                client_family,
                operating_system: OperatingSystem::current(),
                architecture: Architecture::current(),
            }),
        }],
    };
    batch
        .validate()
        .map_err(|error| format!("invalid telemetry batch: {error:?}"))?;
    Ok(batch)
}

fn distribution_channel() -> DistributionChannel {
    match option_env!("LEAN_CTX_DISTRIBUTION_CHANNEL") {
        Some("cargo") => DistributionChannel::Cargo,
        Some("homebrew") => DistributionChannel::Homebrew,
        Some("npm") => DistributionChannel::Npm,
        Some("docker") => DistributionChannel::Docker,
        Some("source") => DistributionChannel::Source,
        _ => DistributionChannel::Unknown,
    }
}

fn client_family() -> ClientFamily {
    if std::env::var_os("CLAUDECODE").is_some() {
        ClientFamily::Claude
    } else if std::env::var_os("CODEX_HOME").is_some() {
        ClientFamily::Codex
    } else if std::env::var_os("CURSOR_TRACE_ID").is_some() {
        ClientFamily::Cursor
    } else if std::env::var_os("GEMINI_CLI").is_some() {
        ClientFamily::Gemini
    } else {
        ClientFamily::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_counts(batch: &TelemetryBatchV2) -> (u64, u64) {
        batch
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                TelemetryEventV2::ToolUsageAggregate(metrics) => {
                    Some((metrics.calls, metrics.failures))
                }
                _ => None,
            })
            .expect("tool aggregate")
    }

    #[test]
    fn daily_batch_is_typed_bounded_and_contains_no_runtime_content() {
        let batch = build_daily_heartbeat(
            "550e8400-e29b-41d4-a716-446655440000".into(),
            "a".repeat(64),
            "2026-09-09".into(),
            DistributionChannel::Cargo,
            ClientFamily::Codex,
        )
        .expect("valid batch");
        let json = serde_json::to_string(&batch).expect("serialize batch");
        assert!(batch.validate().is_ok());
        for forbidden in [
            "prompt",
            "source_code",
            "context_content",
            "file_path",
            "filename",
            "command",
            "argument",
            "stdout",
            "stderr",
            "repository_url",
            "task_text",
            "issue_text",
            "error_message",
            "api_key",
        ] {
            assert!(
                !json.contains(forbidden),
                "forbidden key leaked: {forbidden}"
            );
        }
    }

    #[test]
    fn malformed_identity_is_rejected_before_send() {
        assert!(
            build_daily_heartbeat(
                "raw-user-id".into(),
                "a".repeat(64),
                "2026-09-09".into(),
                DistributionChannel::Unknown,
                ClientFamily::Other,
            )
            .is_err()
        );
    }

    #[test]
    #[serial_test::serial]
    fn prepare_is_two_phase_and_preview_does_not_advance_state() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        crate::core::telemetry::global_metrics().record_tool_call(2_000, true);

        let preview = preview_daily_batch().expect("preview");
        assert!(!state_path().expect("state path").exists());
        assert_eq!(tool_counts(&preview).1, 0);

        let pending = prepare_daily_batch().expect("prepare");
        crate::core::telemetry::global_metrics().record_tool_call(4_000, false);
        assert_eq!(prepare_daily_batch().expect("retry"), pending);
        assert_eq!(preview_daily_batch().expect("pending preview"), pending);

        let mut wrong = pending.clone();
        wrong.events[0].app_version.push_str("-different");
        assert!(acknowledge_daily_batch(&wrong).is_err());
        assert_eq!(prepare_daily_batch().expect("still pending"), pending);

        acknowledge_daily_batch(&pending).expect("acknowledge");
        let next = preview_daily_batch().expect("next preview");
        assert_eq!(tool_counts(&next), (1, 1));
    }

    #[test]
    fn histogram_edges_are_bounded_and_deterministic() {
        let histogram = single_observation_histogram(51, &[10, 50, 100], MAX_COUNT + 1);
        assert_eq!(histogram.upper_bounds, vec![10, 50, 100]);
        assert_eq!(histogram.counts, vec![0, 0, MAX_COUNT]);
        let capped = single_observation_histogram(101, &[10, 50, 100], 1);
        assert_eq!(capped.counts, vec![0, 0, 1]);

        let saturated = bounded_histogram_delta(&[MAX_COUNT; 9], &[0; 9]);
        assert_eq!(saturated.iter().sum::<u64>(), MAX_COUNT);
        assert_eq!(saturated[0], MAX_COUNT);
        assert!(saturated[1..].iter().all(|count| *count == 0));
    }

    #[test]
    #[serial_test::serial]
    fn send_lease_blocks_purge_until_send_finishes() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        let lease = begin_daily_send().expect("begin send");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(purge_local_state()).expect("report purge");
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(50))
                .is_err(),
            "purge must wait for in-flight send lease"
        );
        drop(lease);
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("purge completed")
            .expect("purge succeeded");
        worker.join().expect("purge worker");
    }
}
