// SPDX-License-Identifier: Apache-2.0

//! Privacy-safe daily telemetry aggregation.

use sha2::Digest;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::OnceLock;

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use super::installation_id;
use super::telemetry_v2::{
    Architecture, ClientFamily, DecisionMetrics, DistributionChannel, HeartbeatMetrics, Histogram,
    MAX_COUNT, OccurrenceMetrics, OperatingSystem, SCHEMA_VERSION, SessionMetrics, SyncMetrics,
    TelemetryBatchV2, TelemetryEnvelopeV2, TelemetryEventV2, ToolUsageMetrics,
    VersionUpgradeMetrics,
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
    #[serde(default)]
    installation_id: String,
    process_nonce: String,
    acknowledged: CounterCheckpoint,
    pending: Option<PendingBatch>,
    #[serde(default)]
    last_sent_bucket: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OneShotState {
    #[serde(default)]
    installation_id: String,
    setup_recorded: bool,
    configured_integrations: BTreeSet<String>,
    observed_major: Option<u16>,
    #[serde(default)]
    last_acknowledged_batch: Option<String>,
    queued: QueuedOneShots,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueuedOneShots {
    setup_completed: bool,
    integrations_detected: u64,
    version_upgrade: Option<VersionTransition>,
    #[serde(default)]
    sync: SyncMetrics,
    #[serde(default)]
    autopilot: DecisionMetrics,
    #[serde(default)]
    autopilot_fallback: DecisionMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTransition {
    from_major: u16,
    to_major: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingBatch {
    batch: TelemetryBatchV2,
    #[serde(default)]
    acknowledgement_id: String,
    observed: CounterCheckpoint,
    process_nonce: String,
    #[serde(default)]
    included_one_shots: QueuedOneShots,
}

pub struct DailySendLease {
    batch: TelemetryBatchV2,
    state_path: PathBuf,
    one_shot_path: PathBuf,
    _lock: std::fs::File,
}

impl DailySendLease {
    pub fn batch(&self) -> &TelemetryBatchV2 {
        &self.batch
    }

    pub fn commit(self) -> Result<(), String> {
        let current_state = load_state_at(&self.state_path)?;
        let pending = current_state
            .pending
            .as_ref()
            .ok_or_else(|| "no prepared telemetry batch to acknowledge".to_string())?;
        let included = pending.included_one_shots.clone();
        let acknowledgement_id = pending_acknowledgement_id(pending)?;
        let state = acknowledge_state(current_state, &self.batch)?;
        acknowledge_one_shots_at(&self.one_shot_path, &included, &acknowledgement_id)?;
        write_state(&self.state_path, &state)?;
        Ok(())
    }
}

pub fn pending_daily_batch() -> Result<TelemetryBatchV2, String> {
    preview_daily_batch()
}

/// Build the exact currently eligible payload without advancing durable state.
pub fn preview_daily_batch() -> Result<TelemetryBatchV2, String> {
    let state = state_for_current_identity(load_state()?)?;
    if let Some(pending) = state.pending {
        return Ok(pending.batch);
    }
    let observed = current_checkpoint();
    let one_shots = load_one_shots()?;
    build_from_current_counters(&state, &observed, &one_shots.queued)
}

/// Freeze one payload until the sender explicitly acknowledges success.
#[cfg(test)]
fn prepare_daily_batch() -> Result<TelemetryBatchV2, String> {
    with_locked_state(|mut state| {
        if let Some(pending) = &state.pending {
            return Ok((state.clone(), pending.batch.clone()));
        }
        let observed = current_checkpoint();
        let one_shots = load_one_shots()?;
        let batch = build_from_current_counters(&state, &observed, &one_shots.queued)?;
        state.installation_id = batch_installation_id(&batch).to_string();
        state.pending = Some(PendingBatch {
            batch: batch.clone(),
            acknowledgement_id: uuid::Uuid::new_v4().to_string(),
            observed,
            process_nonce: process_nonce().to_string(),
            included_one_shots: one_shots.queued,
        });
        Ok((state, batch))
    })
}

/// Hold the cross-process state lease until network, ledger, and acknowledgement finish.
pub fn begin_daily_send() -> Result<DailySendLease, String> {
    let path = state_path()?;
    let one_shot_path = one_shot_path()?;
    ensure_parent(&path)?;
    let lock = open_state_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry aggregate state: {error}"))?;
    let mut state = state_for_current_identity(load_state()?)?;
    let batch = if let Some(pending) = &state.pending {
        pending.batch.clone()
    } else {
        ensure_parent(&one_shot_path)?;
        let one_shot_lock = open_sidecar_lock(&one_shot_path)?;
        one_shot_lock
            .lock_exclusive()
            .map_err(|error| format!("cannot lock telemetry one-shot state: {error}"))?;
        let one_shots = one_shots_for_current_identity(load_one_shots_at(&one_shot_path)?)?;
        let observed = current_checkpoint();
        let batch = build_from_current_counters(&state, &observed, &one_shots.queued)?;
        state.installation_id = batch_installation_id(&batch).to_string();
        state.pending = Some(PendingBatch {
            batch: batch.clone(),
            acknowledgement_id: uuid::Uuid::new_v4().to_string(),
            observed,
            process_nonce: process_nonce().to_string(),
            included_one_shots: one_shots.queued,
        });
        write_state(&path, &state)?;
        batch
    };
    Ok(DailySendLease {
        batch,
        state_path: path,
        one_shot_path,
        _lock: lock,
    })
}

/// Advance counters only when the exact frozen payload was accepted remotely.
#[cfg(test)]
fn acknowledge_daily_batch(batch: &TelemetryBatchV2) -> Result<(), String> {
    let path = state_path()?;
    ensure_parent(&path)?;
    let lock = open_state_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry aggregate state: {error}"))?;
    let state = load_state_at(&path)?;
    let pending = state
        .pending
        .as_ref()
        .ok_or_else(|| "no prepared telemetry batch to acknowledge".to_string())?;
    let included = pending.included_one_shots.clone();
    let acknowledgement_id = pending_acknowledgement_id(pending)?;
    let state = acknowledge_state(state, batch)?;
    acknowledge_one_shots_at(&one_shot_path()?, &included, &acknowledgement_id)?;
    write_state(&path, &state)?;
    Ok(())
}

fn pending_acknowledgement_id(pending: &PendingBatch) -> Result<String, String> {
    if !pending.acknowledgement_id.is_empty() {
        return Ok(pending.acknowledgement_id.clone());
    }
    let encoded = serde_json::to_vec(&pending.batch)
        .map_err(|error| format!("cannot encode telemetry acknowledgement: {error}"))?;
    Ok(hex::encode(sha2::Sha256::digest(encoded)))
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
    load_state()
        .and_then(state_for_current_identity)
        .ok()
        .and_then(|state| state.last_sent_bucket)
}

fn state_for_current_identity(mut state: AggregateState) -> Result<AggregateState, String> {
    let (current, _) = installation_id::get_or_create_identity()
        .map_err(|error| format!("installation ID unavailable: {error}"))?;
    let pending_mismatch = state.pending.as_ref().is_some_and(|pending| {
        let pending_id = batch_installation_id(&pending.batch);
        pending_id.is_empty() || pending_id != current
    });
    if pending_mismatch || (!state.installation_id.is_empty() && state.installation_id != current) {
        state = AggregateState {
            installation_id: current,
            ..AggregateState::default()
        };
    } else if state.installation_id.is_empty() {
        state.installation_id = current;
    }
    Ok(state)
}

fn batch_installation_id(batch: &TelemetryBatchV2) -> &str {
    batch
        .events
        .first()
        .map_or("", |event| event.installation_id.as_str())
}

fn build_from_current_counters(
    state: &AggregateState,
    observed: &CounterCheckpoint,
    queued: &QueuedOneShots,
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
        queued,
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
    queued: &QueuedOneShots,
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
    if queued.setup_completed {
        batch.events.push(envelope_like(
            &common,
            TelemetryEventV2::SetupCompleted(OccurrenceMetrics { count: 1 }),
        ));
    }
    if queued.integrations_detected > 0 {
        batch.events.push(envelope_like(
            &common,
            TelemetryEventV2::IntegrationDetected(OccurrenceMetrics {
                count: queued.integrations_detected.min(MAX_COUNT),
            }),
        ));
    }
    if let Some(transition) = queued.version_upgrade {
        batch.events.push(envelope_like(
            &common,
            TelemetryEventV2::VersionUpgrade(VersionUpgradeMetrics {
                from_major: transition.from_major,
                to_major: transition.to_major,
            }),
        ));
    }
    if queued.sync.attempts > 0 {
        batch.events.push(envelope_like(
            &common,
            TelemetryEventV2::SyncAggregate(queued.sync.clone()),
        ));
    }
    if queued.autopilot.admitted > 0 || queued.autopilot.denied > 0 || queued.autopilot.fallback > 0
    {
        batch.events.push(envelope_like(
            &common,
            TelemetryEventV2::AutopilotAggregate(queued.autopilot.clone()),
        ));
    }
    if queued.autopilot_fallback.fallback > 0 {
        batch.events.push(envelope_like(
            &common,
            TelemetryEventV2::AutopilotFallbackAggregate(queued.autopilot_fallback.clone()),
        ));
    }
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

fn one_shot_path() -> Result<PathBuf, String> {
    crate::core::paths::state_dir().map(|dir| dir.join("telemetry_v2_one_shots.json"))
}

pub fn record_setup_completion(
    integration_ids: impl IntoIterator<Item = String>,
) -> Result<(), String> {
    if !telemetry_collection_eligible() {
        return Ok(());
    }
    with_locked_one_shots(|mut state| {
        if !state.setup_recorded {
            state.setup_recorded = true;
            state.queued.setup_completed = true;
        }
        for integration_id in integration_ids {
            if state.configured_integrations.len() >= 64 {
                break;
            }
            if integration_id.is_empty() || integration_id.len() > 128 {
                continue;
            }
            let stable_id = hex::encode(sha2::Sha256::digest(integration_id.as_bytes()));
            if state.configured_integrations.insert(stable_id) {
                state.queued.integrations_detected = state
                    .queued
                    .integrations_detected
                    .saturating_add(1)
                    .min(MAX_COUNT);
            }
        }
        Ok((state, ()))
    })
}

pub fn record_sync_result(success: bool) -> Result<(), String> {
    if !telemetry_collection_eligible() {
        return Ok(());
    }
    with_locked_one_shots(|mut state| {
        if state.queued.sync.attempts >= MAX_COUNT {
            return Ok((state, ()));
        }
        state.queued.sync.attempts += 1;
        if success {
            state.queued.sync.successes = state
                .queued
                .sync
                .successes
                .saturating_add(1)
                .min(state.queued.sync.attempts);
        } else {
            state.queued.sync.failures = state.queued.sync.failures.saturating_add(1).min(
                state
                    .queued
                    .sync
                    .attempts
                    .saturating_sub(state.queued.sync.successes),
            );
        }
        Ok((state, ()))
    })
}

pub fn record_autopilot_decisions(admitted: u64, denied: u64) -> Result<(), String> {
    if !telemetry_collection_eligible() {
        return Ok(());
    }
    with_locked_one_shots(|mut state| {
        state.queued.autopilot.admitted = state
            .queued
            .autopilot
            .admitted
            .saturating_add(admitted)
            .min(MAX_COUNT);
        state.queued.autopilot.denied = state
            .queued
            .autopilot
            .denied
            .saturating_add(denied)
            .min(MAX_COUNT);
        Ok((state, ()))
    })
}

pub fn record_autopilot_fallback() -> Result<(), String> {
    if !telemetry_collection_eligible() {
        return Ok(());
    }
    with_locked_one_shots(|mut state| {
        state.queued.autopilot.fallback = state
            .queued
            .autopilot
            .fallback
            .saturating_add(1)
            .min(MAX_COUNT);
        state.queued.autopilot_fallback.fallback = state
            .queued
            .autopilot_fallback
            .fallback
            .saturating_add(1)
            .min(MAX_COUNT);
        Ok((state, ()))
    })
}

fn telemetry_collection_eligible() -> bool {
    let config = crate::core::config::Config::load_global();
    let do_not_track = std::env::var("DO_NOT_TRACK").ok();
    let telemetry_override = std::env::var("LEAN_CTX_TELEMETRY").ok();
    !config.telemetry.explicitly_disabled()
        && !crate::core::config::TelemetryConfig::environment_disables(
            do_not_track.as_deref(),
            telemetry_override.as_deref(),
        )
}

pub fn record_current_version() -> Result<(), String> {
    record_current_version_value(env!("CARGO_PKG_VERSION"))
}

fn record_current_version_value(version: &str) -> Result<(), String> {
    let current = parse_major(version)
        .ok_or_else(|| "current app version has no valid major component".to_string())?;
    with_locked_one_shots(|mut state| {
        let previous = state.observed_major.or_else(|| {
            crate::core::telemetry_ledger::latest_valid_version()
                .as_deref()
                .and_then(parse_major)
        });
        match previous {
            Some(previous) if current > previous => {
                let from_major = state
                    .queued
                    .version_upgrade
                    .map_or(previous, |transition| transition.from_major);
                state.queued.version_upgrade = Some(VersionTransition {
                    from_major,
                    to_major: current,
                });
                state.observed_major = Some(current);
            }
            Some(previous) => state.observed_major = Some(previous.max(current)),
            None => state.observed_major = Some(current),
        }
        Ok((state, ()))
    })
}

fn parse_major(version: &str) -> Option<u16> {
    version.split('.').next()?.parse().ok()
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
    let one_shot_path = one_shot_path()?;
    ensure_parent(&one_shot_path)?;
    let one_shot_lock = open_sidecar_lock(&one_shot_path)?;
    one_shot_lock
        .lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry one-shot state: {error}"))?;
    remove_state_file(&path, "aggregate")?;
    remove_state_file(&one_shot_path, "one-shot")?;
    operation()
}

pub fn rotate_identity_state_then<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let path = state_path()?;
    let one_shot_path = one_shot_path()?;
    ensure_parent(&path)?;
    let lock = open_state_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry aggregate state: {error}"))?;
    let one_shot_lock = open_sidecar_lock(&one_shot_path)?;
    one_shot_lock
        .lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry one-shot state: {error}"))?;
    let mut one_shots = one_shots_for_current_identity(load_one_shots_at(&one_shot_path)?)?;
    write_one_shots(&one_shot_path, &one_shots)?;
    let value = operation()?;
    one_shots.installation_id = installation_id::get_or_create()
        .map_err(|error| format!("installation ID unavailable after rotation: {error}"))?;
    one_shots.queued.setup_completed |= one_shots.setup_recorded;
    one_shots.queued.integrations_detected = one_shots
        .configured_integrations
        .len()
        .try_into()
        .unwrap_or(MAX_COUNT)
        .min(MAX_COUNT);
    one_shots.queued.sync = SyncMetrics::default();
    one_shots.queued.autopilot = DecisionMetrics::default();
    one_shots.queued.autopilot_fallback = DecisionMetrics::default();
    one_shots.last_acknowledged_batch = None;
    remove_state_file(&path, "aggregate")?;
    write_one_shots(&one_shot_path, &one_shots)?;
    Ok(value)
}

fn remove_state_file(path: &std::path::Path, kind: &str) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot purge telemetry {kind} state: {error}")),
    }
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

fn load_one_shots() -> Result<OneShotState, String> {
    let path = one_shot_path()?;
    ensure_parent(&path)?;
    let lock = open_sidecar_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry one-shot state: {error}"))?;
    one_shots_for_current_identity(load_one_shots_at(&path)?)
}

fn load_one_shots_at(path: &std::path::Path) -> Result<OneShotState, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid telemetry one-shot state: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(OneShotState::default()),
        Err(error) => Err(format!("cannot read telemetry one-shot state: {error}")),
    }
}

fn one_shots_for_current_identity(mut state: OneShotState) -> Result<OneShotState, String> {
    let current = installation_id::get_or_create()
        .map_err(|error| format!("installation ID unavailable: {error}"))?;
    if state.installation_id.is_empty() {
        state.installation_id = current;
    } else if state.installation_id != current {
        state.installation_id = current;
        state.queued.setup_completed |= state.setup_recorded;
        state.queued.integrations_detected = state
            .configured_integrations
            .len()
            .try_into()
            .unwrap_or(MAX_COUNT)
            .min(MAX_COUNT);
        state.queued.sync = SyncMetrics::default();
        state.queued.autopilot = DecisionMetrics::default();
        state.queued.autopilot_fallback = DecisionMetrics::default();
        state.last_acknowledged_batch = None;
    }
    Ok(state)
}

fn with_locked_one_shots<T>(
    operation: impl FnOnce(OneShotState) -> Result<(OneShotState, T), String>,
) -> Result<T, String> {
    let path = one_shot_path()?;
    ensure_parent(&path)?;
    let lock = open_sidecar_lock(&path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry one-shot state: {error}"))?;
    let state = one_shots_for_current_identity(load_one_shots_at(&path)?)?;
    let (state, value) = operation(state)?;
    write_one_shots(&path, &state)?;
    Ok(value)
}

fn acknowledge_one_shots_at(
    path: &std::path::Path,
    included: &QueuedOneShots,
    acknowledgement_id: &str,
) -> Result<(), String> {
    ensure_parent(path)?;
    let lock = open_sidecar_lock(path)?;
    lock.lock_exclusive()
        .map_err(|error| format!("cannot lock telemetry one-shot state: {error}"))?;
    let mut state = one_shots_for_current_identity(load_one_shots_at(path)?)?;
    if state.last_acknowledged_batch.as_deref() == Some(acknowledgement_id) {
        return Ok(());
    }
    if included.setup_completed {
        state.queued.setup_completed = false;
    }
    state.queued.integrations_detected = state
        .queued
        .integrations_detected
        .saturating_sub(included.integrations_detected);
    state.queued.sync.attempts = state
        .queued
        .sync
        .attempts
        .saturating_sub(included.sync.attempts);
    state.queued.sync.successes = state
        .queued
        .sync
        .successes
        .saturating_sub(included.sync.successes);
    state.queued.sync.failures = state
        .queued
        .sync
        .failures
        .saturating_sub(included.sync.failures);
    subtract_decisions(&mut state.queued.autopilot, &included.autopilot);
    subtract_decisions(
        &mut state.queued.autopilot_fallback,
        &included.autopilot_fallback,
    );
    if let Some(sent) = included.version_upgrade {
        state.queued.version_upgrade = match state.queued.version_upgrade {
            Some(current) if current == sent => None,
            Some(current)
                if current.from_major == sent.from_major && current.to_major > sent.to_major =>
            {
                Some(VersionTransition {
                    from_major: sent.to_major,
                    to_major: current.to_major,
                })
            }
            current => current,
        };
    }
    state.last_acknowledged_batch = Some(acknowledgement_id.to_string());
    write_one_shots(path, &state)
}

fn subtract_decisions(current: &mut DecisionMetrics, included: &DecisionMetrics) {
    current.admitted = current.admitted.saturating_sub(included.admitted);
    current.denied = current.denied.saturating_sub(included.denied);
    current.fallback = current.fallback.saturating_sub(included.fallback);
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

fn write_one_shots(path: &std::path::Path, state: &OneShotState) -> Result<(), String> {
    let bytes = serde_json::to_vec(state)
        .map_err(|error| format!("cannot serialize telemetry one-shot state: {error}"))?;
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        Some(std::fs::Permissions::from_mode(0o600))
    };
    #[cfg(not(unix))]
    let permissions: Option<std::fs::Permissions> = None;
    crate::core::atomic_fs::try_atomic_write(path, &bytes, permissions.as_ref())
        .map_err(|error| format!("cannot persist telemetry one-shot state: {error}"))
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

fn open_sidecar_lock(path: &std::path::Path) -> Result<std::fs::File, String> {
    open_state_lock(path)
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

    struct TelemetryEnvGuard(Option<std::ffi::OsString>);

    impl TelemetryEnvGuard {
        fn disable() -> Self {
            let previous = std::env::var_os("LEAN_CTX_TELEMETRY");
            crate::test_env::set_var("LEAN_CTX_TELEMETRY", "off");
            Self(previous)
        }
    }

    impl Drop for TelemetryEnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => crate::test_env::set_var("LEAN_CTX_TELEMETRY", value),
                None => crate::test_env::remove_var("LEAN_CTX_TELEMETRY"),
            }
        }
    }

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

    fn occurrence_count(batch: &TelemetryBatchV2, name: &str) -> Option<u64> {
        batch
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                TelemetryEventV2::SetupCompleted(metrics) if name == "setup_completed" => {
                    Some(metrics.count)
                }
                TelemetryEventV2::IntegrationDetected(metrics)
                    if name == "integration_detected" =>
                {
                    Some(metrics.count)
                }
                _ => None,
            })
    }

    fn version_transition(batch: &TelemetryBatchV2) -> Option<(u16, u16)> {
        batch
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                TelemetryEventV2::VersionUpgrade(metrics) => {
                    Some((metrics.from_major, metrics.to_major))
                }
                _ => None,
            })
    }

    fn sync_counts(batch: &TelemetryBatchV2) -> Option<(u64, u64, u64)> {
        batch
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                TelemetryEventV2::SyncAggregate(metrics) => {
                    Some((metrics.attempts, metrics.successes, metrics.failures))
                }
                _ => None,
            })
    }

    fn autopilot_counts(batch: &TelemetryBatchV2) -> Option<(u64, u64, u64)> {
        batch
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                TelemetryEventV2::AutopilotAggregate(metrics) => {
                    Some((metrics.admitted, metrics.denied, metrics.fallback))
                }
                _ => None,
            })
    }

    fn autopilot_fallback_counts(batch: &TelemetryBatchV2) -> Option<(u64, u64, u64)> {
        batch
            .events
            .iter()
            .find_map(|envelope| match &envelope.event {
                TelemetryEventV2::AutopilotFallbackAggregate(metrics) => {
                    Some((metrics.admitted, metrics.denied, metrics.fallback))
                }
                _ => None,
            })
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

    #[test]
    #[serial_test::serial]
    fn one_shots_are_deduplicated_and_ack_only_included_watermarks() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_current_version_value("3.9.20").expect("seed major");
        assert_eq!(
            version_transition(&preview_daily_batch().expect("preview")),
            None
        );

        record_setup_completion(vec!["Claude Code".into(), "Claude Code".into()])
            .expect("record setup");
        let first = prepare_daily_batch().expect("prepare first");
        assert_eq!(occurrence_count(&first, "setup_completed"), Some(1));
        assert_eq!(occurrence_count(&first, "integration_detected"), Some(1));

        record_setup_completion(vec!["Claude Code".into(), "Codex".into()])
            .expect("record later integration");
        record_current_version_value("4.0.0").expect("record upgrade");
        assert_eq!(prepare_daily_batch().expect("retry exact"), first);
        acknowledge_daily_batch(&first).expect("ack first");

        let second = prepare_daily_batch().expect("prepare second");
        assert_eq!(occurrence_count(&second, "setup_completed"), None);
        assert_eq!(occurrence_count(&second, "integration_detected"), Some(1));
        assert_eq!(version_transition(&second), Some((3, 4)));
        record_current_version_value("5.0.0").expect("record next upgrade while pending");
        acknowledge_daily_batch(&second).expect("ack second");

        let third = prepare_daily_batch().expect("prepare residual upgrade");
        assert_eq!(version_transition(&third), Some((4, 5)));
        acknowledge_daily_batch(&third).expect("ack residual upgrade");

        record_current_version_value("2.0.0").expect("ignore downgrade");
        let final_preview = preview_daily_batch().expect("final preview");
        assert_eq!(occurrence_count(&final_preview, "setup_completed"), None);
        assert_eq!(
            occurrence_count(&final_preview, "integration_detected"),
            None
        );
        assert_eq!(version_transition(&final_preview), None);
    }

    #[test]
    #[serial_test::serial]
    fn setup_recording_does_not_wait_for_network_send_lease() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        let lease = begin_daily_send().expect("begin send");
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(record_setup_completion(vec!["codex".into()]))
                .expect("report setup recording");
        });
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("setup recording must not wait for network lease")
            .expect("setup recording succeeded");
        drop(lease);
        worker.join().expect("setup worker");
    }

    #[test]
    #[serial_test::serial]
    fn setup_recording_respects_environment_opt_out() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        let _telemetry = TelemetryEnvGuard::disable();
        record_setup_completion(vec!["codex".into()]).expect("opted-out recording is a no-op");
        record_sync_result(true).expect("opted-out sync recording is a no-op");
        record_autopilot_decisions(1, 1).expect("opted-out decision recording is a no-op");
        record_autopilot_fallback().expect("opted-out fallback recording is a no-op");
        assert!(!one_shot_path().expect("one-shot path").exists());
        let preview = preview_daily_batch().expect("preview");
        assert_eq!(occurrence_count(&preview, "setup_completed"), None);
        assert_eq!(occurrence_count(&preview, "integration_detected"), None);
        assert_eq!(sync_counts(&preview), None);
        assert_eq!(autopilot_counts(&preview), None);
        assert_eq!(autopilot_fallback_counts(&preview), None);
    }

    #[test]
    #[serial_test::serial]
    fn autopilot_results_are_durable_and_ack_only_the_pending_snapshot() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_autopilot_decisions(2, 1).expect("record decisions");
        let first = prepare_daily_batch().expect("prepare first");
        assert_eq!(autopilot_counts(&first), Some((2, 1, 0)));

        record_autopilot_decisions(1, 2).expect("record concurrent decisions");
        record_autopilot_fallback().expect("record concurrent fallback");
        assert_eq!(prepare_daily_batch().expect("retry"), first);
        acknowledge_daily_batch(&first).expect("ack first");

        let residual = preview_daily_batch().expect("residual preview");
        assert_eq!(autopilot_counts(&residual), Some((1, 2, 1)));
        assert_eq!(autopilot_fallback_counts(&residual), Some((0, 0, 1)));
    }

    #[test]
    #[serial_test::serial]
    fn saturated_autopilot_counters_remain_bounded() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        with_locked_one_shots(|mut state| {
            state.queued.autopilot = DecisionMetrics {
                admitted: MAX_COUNT,
                denied: MAX_COUNT,
                fallback: MAX_COUNT,
            };
            state.queued.autopilot_fallback.fallback = MAX_COUNT;
            Ok((state, ()))
        })
        .expect("seed saturated autopilot counts");

        record_autopilot_decisions(1, 1).expect("saturated decision recorder is a no-op");
        record_autopilot_fallback().expect("saturated fallback recorder is a no-op");
        let preview = preview_daily_batch().expect("preview");
        assert_eq!(
            autopilot_counts(&preview),
            Some((MAX_COUNT, MAX_COUNT, MAX_COUNT))
        );
        assert_eq!(autopilot_fallback_counts(&preview), Some((0, 0, MAX_COUNT)));
    }

    #[test]
    #[serial_test::serial]
    fn sync_results_are_durable_and_ack_only_the_pending_snapshot() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_sync_result(true).expect("record success");
        record_sync_result(false).expect("record failure");
        let first = prepare_daily_batch().expect("prepare first");
        assert_eq!(sync_counts(&first), Some((2, 1, 1)));

        record_sync_result(true).expect("record concurrent success");
        assert_eq!(prepare_daily_batch().expect("retry"), first);
        acknowledge_daily_batch(&first).expect("ack first");
        assert_eq!(
            sync_counts(&preview_daily_batch().expect("residual preview")),
            Some((1, 1, 0))
        );
    }

    #[test]
    #[serial_test::serial]
    fn retry_after_sidecar_ack_crash_does_not_double_subtract_one_shots() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_sync_result(true).expect("record included sync result");
        record_autopilot_decisions(1, 0).expect("record included decision");
        let pending = prepare_daily_batch().expect("prepare pending batch");
        let aggregate_path = state_path().expect("aggregate path");
        let state = load_state_at(&aggregate_path).expect("load pending state");
        let pending_state = state.pending.expect("pending batch");
        let acknowledgement_id =
            pending_acknowledgement_id(&pending_state).expect("compute acknowledgement id");
        let included = pending_state.included_one_shots;

        // Simulate a crash after the sidecar subtraction was persisted but before
        // the aggregate pending marker was cleared.
        acknowledge_one_shots_at(
            &one_shot_path().expect("one-shot path"),
            &included,
            &acknowledgement_id,
        )
        .expect("persist sidecar acknowledgement");
        record_sync_result(false).expect("record result after interrupted commit");
        record_autopilot_decisions(0, 1).expect("record decision after interrupted commit");

        acknowledge_daily_batch(&pending).expect("retry acknowledgement");
        let residual = preview_daily_batch().expect("residual preview");
        assert_eq!(sync_counts(&residual), Some((1, 0, 1)));
        assert_eq!(autopilot_counts(&residual), Some((0, 1, 0)));
    }

    #[test]
    #[serial_test::serial]
    fn distinct_pending_instances_with_identical_metrics_are_each_acknowledged() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_sync_result(true).expect("record first result");
        let first = prepare_daily_batch().expect("prepare first batch");
        let first_state = load_state().expect("load first state");
        let first_pending = first_state.pending.expect("first pending");
        let first_id = pending_acknowledgement_id(&first_pending).expect("first id");
        acknowledge_daily_batch(&first).expect("ack first batch");

        record_sync_result(true).expect("record identical second result");
        let second = prepare_daily_batch().expect("prepare second batch");
        let second_state = load_state().expect("load second state");
        let second_pending = second_state.pending.expect("second pending");
        let second_id = pending_acknowledgement_id(&second_pending).expect("second id");
        assert_ne!(first_id, second_id);
        acknowledge_daily_batch(&second).expect("ack second batch");
        assert_eq!(
            sync_counts(&preview_daily_batch().expect("final preview")),
            None
        );
    }

    #[test]
    #[serial_test::serial]
    fn saturated_sync_counter_remains_cross_field_consistent() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        with_locked_one_shots(|mut state| {
            state.queued.sync = SyncMetrics {
                attempts: MAX_COUNT,
                successes: MAX_COUNT,
                failures: 0,
            };
            Ok((state, ()))
        })
        .expect("seed saturated sync counts");

        record_sync_result(false).expect("saturated recorder is a no-op");
        assert_eq!(
            sync_counts(&preview_daily_batch().expect("preview")),
            Some((MAX_COUNT, MAX_COUNT, 0))
        );
    }

    #[test]
    #[serial_test::serial]
    fn identity_rotation_requeues_installation_scoped_facts() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_setup_completion(vec!["codex".into(), "claude".into()]).expect("record setup");
        let first = prepare_daily_batch().expect("prepare first");
        acknowledge_daily_batch(&first).expect("ack first");
        record_sync_result(true).expect("record old-identity sync");
        record_autopilot_decisions(1, 1).expect("record old-identity decisions");
        record_autopilot_fallback().expect("record old-identity fallback");

        rotate_identity_state_then(|| Ok(())).expect("rotate state");
        let replay = preview_daily_batch().expect("preview replay");
        assert_eq!(occurrence_count(&replay, "setup_completed"), Some(1));
        assert_eq!(occurrence_count(&replay, "integration_detected"), Some(2));
        assert_eq!(sync_counts(&replay), None);
        assert_eq!(autopilot_counts(&replay), None);
        assert_eq!(autopilot_fallback_counts(&replay), None);
    }

    #[test]
    #[serial_test::serial]
    fn failed_identity_rotation_does_not_requeue_old_identity_facts() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_setup_completion(vec!["codex".into()]).expect("record setup");
        let first = prepare_daily_batch().expect("prepare first");
        acknowledge_daily_batch(&first).expect("ack first");

        assert!(rotate_identity_state_then::<()>(|| Err("reset failed".into())).is_err());
        let unchanged = preview_daily_batch().expect("preview unchanged state");
        assert_eq!(occurrence_count(&unchanged, "setup_completed"), None);
        assert_eq!(occurrence_count(&unchanged, "integration_detected"), None);
    }

    #[test]
    #[serial_test::serial]
    fn legacy_pending_batch_is_discarded_after_identity_change() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        let stale = prepare_daily_batch().expect("prepare old identity batch");
        let stale_id = batch_installation_id(&stale).to_string();
        let path = state_path().expect("state path");
        let legacy_state = load_state_at(&path).expect("load state");
        let mut legacy_json = serde_json::to_value(legacy_state).expect("serialize legacy state");
        legacy_json
            .as_object_mut()
            .expect("aggregate state object")
            .remove("installation_id");
        let bytes = serde_json::to_vec(&legacy_json).expect("encode legacy JSON");
        crate::core::atomic_fs::try_atomic_write(&path, &bytes, None)
            .expect("write legacy state without identity field");

        let current_id = installation_id::reset().expect("rotate identity directly");
        assert_ne!(current_id, stale_id);
        let current = preview_daily_batch().expect("preview current identity");
        assert_ne!(current, stale);
        assert!(
            current
                .events
                .iter()
                .all(|event| event.installation_id == current_id)
        );
    }

    #[test]
    #[serial_test::serial]
    fn stale_sidecar_discards_sync_but_requeues_setup_after_identity_change() {
        let _iso = crate::core::data_dir::isolated_data_dir();
        record_setup_completion(vec!["codex".into()]).expect("record setup");
        let first = prepare_daily_batch().expect("prepare first");
        acknowledge_daily_batch(&first).expect("ack setup");
        record_sync_result(true).expect("record old-identity sync");
        record_autopilot_decisions(1, 1).expect("record old-identity decisions");
        record_autopilot_fallback().expect("record old-identity fallback");

        installation_id::reset().expect("simulate successful reset before sidecar cleanup");
        let current = preview_daily_batch().expect("preview rebound sidecar");
        assert_eq!(occurrence_count(&current, "setup_completed"), Some(1));
        assert_eq!(occurrence_count(&current, "integration_detected"), Some(1));
        assert_eq!(sync_counts(&current), None);
        assert_eq!(autopilot_counts(&current), None);
        assert_eq!(autopilot_fallback_counts(&current), None);
    }
}
