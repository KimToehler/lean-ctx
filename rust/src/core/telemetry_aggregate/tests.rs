// SPDX-License-Identifier: Apache-2.0

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
            TelemetryEventV2::IntegrationDetected(metrics) if name == "integration_detected" => {
                Some(metrics.count)
            }
            TelemetryEventV2::CheckoutStarted(metrics) if name == "checkout_started" => {
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

fn error_count(batch: &TelemetryBatchV2, category: ErrorCategory) -> Option<u64> {
    batch
        .events
        .iter()
        .find_map(|envelope| match &envelope.event {
            TelemetryEventV2::ErrorCategoryAggregate(metrics) if metrics.category == category => {
                Some(metrics.count)
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
    // A fresh state has no baseline, so the first batch carries every failure
    // earlier tests left in this process's global counter.
    let prior_failures = crate::core::telemetry::global_metrics()
        .daily_telemetry_snapshot()
        .tool_failures;
    crate::core::telemetry::global_metrics().record_tool_call(2_000, true);

    let preview = preview_daily_batch().expect("preview");
    assert!(!state_path().expect("state path").exists());
    assert_eq!(tool_counts(&preview).1, prior_failures);

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
#[serial_test::serial]
fn preview_fails_fast_during_send_then_preserves_concurrent_counts() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    record_sync_result(false).expect("record included failure");
    let lease = begin_daily_send().expect("begin send");
    let error = preview_daily_batch().expect_err("preview must not race a send");
    assert!(error.contains("send is in progress"));
    record_sync_result(true).expect("record concurrent success");
    lease.commit().expect("commit included failure");
    let preview = preview_daily_batch().expect("preview after send");
    assert_eq!(sync_counts(&preview), Some((1, 1, 0)));
}

#[test]
#[serial_test::serial]
fn preview_fails_fast_during_sidecar_write_and_recovers() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    record_sync_result(true).expect("record result");
    {
        let path = one_shot_path().expect("sidecar path");
        let lock = open_sidecar_lock(&path).expect("open lock");
        lock.lock_exclusive().expect("hold writer lock");
        let error = preview_daily_batch().expect_err("preview must not wait for writer");
        assert!(error.contains("cannot lock one-shot state"));
    }
    assert_eq!(
        sync_counts(&preview_daily_batch().expect("preview after writer exits")),
        Some((1, 1, 0))
    );
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

/// Two fixed buckets. Admission is decided by these values, not by the clock,
/// so none of the tests below can shift meaning across a UTC midnight.
const BUCKET: &str = "2026-03-01";
const NEXT_BUCKET: &str = "2026-03-02";

/// Take a lease for `bucket` and acknowledge it, reporting whether this sender
/// was admitted.
fn admit_and_commit(bucket: &str) -> bool {
    match begin_daily_send_in_bucket(Some(bucket)) {
        Ok(lease) => {
            lease.commit().expect("commit admitted batch");
            true
        }
        Err(error) => {
            assert!(error.contains("already sent"), "{error}");
            false
        }
    }
}

#[test]
#[serial_test::serial]
fn a_second_batch_for_an_acknowledged_bucket_is_refused_under_the_lock() {
    let _iso = crate::core::data_dir::isolated_data_dir();

    let lease = begin_daily_send_in_bucket(Some(BUCKET)).expect("first lease");
    // The bucket that decided admission is the bucket that stamps the payload.
    assert_eq!(lease.batch().events[0].timestamp_bucket, BUCKET);
    lease.commit().expect("commit first send");
    assert_eq!(last_sent_bucket().as_deref(), Some(BUCKET));

    // A caller that evaluated "not sent yet" before the commit above still
    // arrives here. The refusal has to happen under the lock, because outside
    // it there is no point at which the answer stays true.
    let error = begin_daily_send_in_bucket(Some(BUCKET))
        .err()
        .expect("no second batch");
    assert!(error.contains(BUCKET), "{error}");
    assert!(error.contains("already sent"), "{error}");

    // The refusal is inert: no payload was frozen, the recorded bucket is
    // untouched, so there is nothing to unwind and no watermark was consumed.
    assert_eq!(last_sent_bucket().as_deref(), Some(BUCKET));
    assert!(load_state().expect("state").pending.is_none());
}

#[test]
#[serial_test::serial]
fn the_next_bucket_is_still_admitted() {
    let _iso = crate::core::data_dir::isolated_data_dir();

    begin_daily_send_in_bucket(Some(BUCKET))
        .expect("first lease")
        .commit()
        .expect("commit first send");

    // Proves the guard is equality on the bucket, not a blanket "send once".
    let lease = begin_daily_send_in_bucket(Some(NEXT_BUCKET)).expect("next bucket is admitted");
    assert_eq!(lease.batch().events[0].timestamp_bucket, NEXT_BUCKET);
}

#[test]
#[serial_test::serial]
fn an_acknowledged_bucket_still_retries_its_frozen_pending_batch() {
    let _iso = crate::core::data_dir::isolated_data_dir();

    // Freeze a payload and abandon it the way a crash between the network send
    // and the acknowledgement does: the lease drops, `pending` survives.
    let pending = begin_daily_send_in_bucket(Some(BUCKET))
        .expect("first lease")
        .batch()
        .clone();

    let path = state_path().expect("state path");
    let mut state = load_state().expect("load state");
    state.last_sent_bucket = Some(BUCKET.to_string());
    write_state(&path, &state).expect("seed acknowledged bucket");

    // Only a *new* batch is refused. The frozen payload is handed back verbatim
    // even though its bucket is already marked sent -- at-least-once delivery of
    // the exact checkpointed batch is deliberate, not an oversight.
    let lease = begin_daily_send_in_bucket(Some(BUCKET)).expect("pending must still retry");
    assert_eq!(lease.batch(), &pending);
}

#[test]
#[serial_test::serial]
fn a_pending_retry_keeps_its_own_bucket_across_a_day_boundary() {
    let _iso = crate::core::data_dir::isolated_data_dir();

    let pending = begin_daily_send_in_bucket(Some(BUCKET))
        .expect("first lease")
        .batch()
        .clone();
    assert_eq!(pending.events[0].timestamp_bucket, BUCKET);

    // The day rolls over before the retry. The frozen payload is returned
    // unchanged -- it is not re-stamped, and it is not required to match the
    // bucket the caller asked for.
    let lease = begin_daily_send_in_bucket(Some(NEXT_BUCKET)).expect("pending retry");
    assert_eq!(lease.batch(), &pending);
    assert_eq!(lease.batch().events[0].timestamp_bucket, BUCKET);
}

#[test]
#[serial_test::serial]
fn two_callers_contending_over_one_bucket_admit_exactly_one_batch() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    assert!(last_sent_bucket().is_none(), "bucket must start unsent");

    // The barrier makes the interleaving deterministic instead of hoping the
    // scheduler produces it: both senders are past any caller-side precheck
    // before either one reaches the lock, and both name the same bucket.
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let contender_barrier = std::sync::Arc::clone(&barrier);
    let contender = std::thread::spawn(move || {
        contender_barrier.wait();
        admit_and_commit(BUCKET)
    });
    barrier.wait();
    let here = admit_and_commit(BUCKET);
    let there = contender.join().expect("contending sender");

    assert_eq!(
        usize::from(here) + usize::from(there),
        1,
        "exactly one sender may acknowledge a given bucket"
    );
    assert_eq!(last_sent_bucket().as_deref(), Some(BUCKET));
    assert!(load_state().expect("state").pending.is_none());
}

#[test]
#[serial_test::serial]
fn send_aborts_on_a_bounded_wait_for_the_aggregate_lock() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    let path = state_path().expect("state path");
    ensure_parent(&path).expect("state dir");

    // A contending operation must not wait for the entire network request.
    let blocker = open_state_lock(&path).expect("open state lock");
    blocker.lock_exclusive().expect("hold aggregate lock");

    // Monotonic elapsed time, not wall-clock date: unaffected by any rollover.
    let started = std::time::Instant::now();
    let error = begin_daily_send_in_bucket(Some(BUCKET))
        .err()
        .expect("must not wait forever");
    let waited = started.elapsed();
    assert!(error.contains("aggregate"), "{error}");
    assert!(error.contains("timed out"), "{error}");
    assert!(
        waited >= SEND_LOCK_TIMEOUT,
        "returned before the bound: {waited:?}"
    );
    // Generous headroom: a loaded machine must not fail this, while an
    // unbounded wait never finishes at all.
    assert!(
        waited < SEND_LOCK_TIMEOUT * 10,
        "wait was not bounded: {waited:?}"
    );

    drop(blocker);
    begin_daily_send_in_bucket(Some(BUCKET)).expect("lock is usable once the holder exits");
}

#[test]
#[serial_test::serial]
fn send_aborts_on_a_bounded_wait_for_the_nested_one_shot_lock() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    let sidecar = one_shot_path().expect("sidecar path");
    ensure_parent(&sidecar).expect("sidecar dir");

    // The sidecar lock is taken while the aggregate lock is already held, so an
    // unbounded wait here pins both locks rather than one.
    let blocker = open_sidecar_lock(&sidecar).expect("open sidecar lock");
    blocker.lock_exclusive().expect("hold one-shot lock");

    let started = std::time::Instant::now();
    let error = begin_daily_send_in_bucket(Some(BUCKET))
        .err()
        .expect("nested wait is bounded too");
    let waited = started.elapsed();
    assert!(error.contains("one-shot"), "{error}");
    assert!(error.contains("timed out"), "{error}");
    assert!(
        waited < SEND_LOCK_TIMEOUT * 10,
        "wait was not bounded: {waited:?}"
    );

    // The aggregate lock was released along with the failed attempt, so the
    // next sender is not left locked out by the abort itself.
    drop(blocker);
    begin_daily_send_in_bucket(Some(BUCKET)).expect("both locks free again");
}

#[test]
#[serial_test::serial]
fn contended_purge_and_rotation_leave_state_and_callbacks_untouched() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    let lease = begin_daily_send_in_bucket(Some(BUCKET)).expect("lease");
    let path = state_path().expect("path");
    let original = std::fs::read(&path).expect("state");
    let called = std::cell::Cell::new(false);
    let operation = || {
        called.set(true);
        Ok(())
    };
    let started = std::time::Instant::now();
    assert!(
        purge_local_state_then(operation)
            .unwrap_err()
            .contains("timed out")
    );
    assert!(
        rotate_identity_state_then(operation)
            .unwrap_err()
            .contains("timed out")
    );
    assert!(started.elapsed() < SEND_LOCK_TIMEOUT * 10);
    assert!(!called.get());
    assert_eq!(std::fs::read(path).expect("preserved state"), original);
    drop(lease);
    purge_local_state_then(operation).expect("purge after release");
    assert!(called.get());
}

#[test]
#[serial_test::serial]
fn contended_record_and_ack_preserve_counters_and_exact_pending_retry() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    record_sync_result(true).expect("initial event");
    let lease = begin_daily_send_in_bucket(Some(BUCKET)).expect("lease");
    let pending = lease.batch().clone();
    let path = one_shot_path().expect("path");
    let original = std::fs::read(&path).expect("sidecar");
    let blocker = open_sidecar_lock(&path).expect("lock");
    blocker.lock_exclusive().expect("hold sidecar");
    let started = std::time::Instant::now();
    assert!(record_sync_result(false).unwrap_err().contains("timed out"));
    assert!(lease.commit().unwrap_err().contains("timed out"));
    assert!(started.elapsed() < SEND_LOCK_TIMEOUT * 10);
    assert_eq!(std::fs::read(path).expect("preserved sidecar"), original);
    drop(blocker);
    let retry = begin_daily_send_in_bucket(Some(NEXT_BUCKET)).expect("retry");
    assert_eq!(retry.batch(), &pending);
    retry.commit().expect("ack after release");
    assert_eq!(last_sent_bucket().as_deref(), Some(BUCKET));
    assert_eq!(sync_counts(&preview_daily_batch().expect("preview")), None);
}

#[test]
#[serial_test::serial]
fn a_busy_ledger_does_not_erase_the_pending_version_upgrade() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    let record = crate::core::telemetry_ledger::HeartbeatRecord {
        timestamp: "2026-03-01T00:00:00Z".into(),
        installation_id: String::new(),
        version: "3.10.1".into(),
        os: String::new(),
        arch: String::new(),
        schema_version: 2,
        event_names: vec![],
        payload_hash: "a".repeat(64),
        endpoint: String::new(),
        status: "success".into(),
    };
    crate::core::telemetry_ledger::append(&record).unwrap();
    let ledger = crate::core::paths::state_dir()
        .unwrap()
        .join("telemetry_heartbeats.jsonl");
    let blocker = open_state_lock(&ledger).unwrap();
    blocker.lock_exclusive().unwrap();
    assert!(
        record_current_version_value("4.0.0")
            .unwrap_err()
            .contains("timed out")
    );
    assert!(
        !one_shot_path().unwrap().exists(),
        "failed observation must not persist"
    );
    drop(blocker);
    record_current_version_value("4.0.0").unwrap();
    assert_eq!(
        version_transition(&preview_daily_batch().unwrap()),
        Some((3, 4))
    );
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
    record_checkout_started().expect("opted-out checkout recording is a no-op");
    record_error_category(ErrorCategory::Internal).expect("opted-out error recording is a no-op");
    assert!(!one_shot_path().expect("one-shot path").exists());
    let preview = preview_daily_batch().expect("preview");
    assert_eq!(occurrence_count(&preview, "setup_completed"), None);
    assert_eq!(occurrence_count(&preview, "integration_detected"), None);
    assert_eq!(sync_counts(&preview), None);
    assert_eq!(autopilot_counts(&preview), None);
    assert_eq!(autopilot_fallback_counts(&preview), None);
    assert_eq!(occurrence_count(&preview, "checkout_started"), None);
    assert_eq!(error_count(&preview, ErrorCategory::Internal), None);
}

#[test]
#[serial_test::serial]
fn error_categories_are_typed_durable_and_ack_only_the_pending_snapshot() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    for category in ERROR_CATEGORIES {
        record_error_category(category).expect("record error category");
    }
    let first = prepare_daily_batch().expect("prepare first");
    for category in ERROR_CATEGORIES {
        assert_eq!(error_count(&first, category), Some(1));
    }

    record_error_category(ErrorCategory::Timeout).expect("record concurrent timeout");
    assert_eq!(prepare_daily_batch().expect("retry"), first);
    acknowledge_daily_batch(&first).expect("ack first");

    let residual = preview_daily_batch().expect("residual preview");
    assert_eq!(error_count(&residual, ErrorCategory::Timeout), Some(1));
    for category in ERROR_CATEGORIES {
        if category != ErrorCategory::Timeout {
            assert_eq!(error_count(&residual, category), None);
        }
    }
    let json = serde_json::to_string(&residual).expect("serialize telemetry");
    assert!(!json.contains("error message"));
    assert!(!json.contains("stack"));
}

#[test]
#[serial_test::serial]
fn error_category_counter_saturates_at_schema_bound() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    with_locked_one_shots(|mut state| {
        state.queued.error_categories[7] = MAX_COUNT;
        Ok((state, ()))
    })
    .expect("seed saturated counter");
    record_error_category(ErrorCategory::Internal).expect("saturated recorder is a no-op");
    assert_eq!(
        error_count(
            &preview_daily_batch().expect("preview"),
            ErrorCategory::Internal
        ),
        Some(MAX_COUNT)
    );
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
fn checkout_starts_are_durable_and_ack_only_the_pending_snapshot() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    record_checkout_started().expect("record checkout");
    let first = prepare_daily_batch().expect("prepare first");
    assert_eq!(occurrence_count(&first, "checkout_started"), Some(1));

    record_checkout_started().expect("record concurrent checkout");
    assert_eq!(prepare_daily_batch().expect("retry"), first);
    acknowledge_daily_batch(&first).expect("ack first");
    assert_eq!(
        occurrence_count(
            &preview_daily_batch().expect("residual preview"),
            "checkout_started"
        ),
        Some(1)
    );
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
        state.queued.checkout_started = MAX_COUNT;
        Ok((state, ()))
    })
    .expect("seed saturated autopilot counts");

    record_autopilot_decisions(1, 1).expect("saturated decision recorder is a no-op");
    record_autopilot_fallback().expect("saturated fallback recorder is a no-op");
    record_checkout_started().expect("saturated checkout recorder is a no-op");
    let preview = preview_daily_batch().expect("preview");
    assert_eq!(
        autopilot_counts(&preview),
        Some((MAX_COUNT, MAX_COUNT, MAX_COUNT))
    );
    assert_eq!(autopilot_fallback_counts(&preview), Some((0, 0, MAX_COUNT)));
    assert_eq!(
        occurrence_count(&preview, "checkout_started"),
        Some(MAX_COUNT)
    );
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
    record_checkout_started().expect("record old-identity checkout");
    record_error_category(ErrorCategory::Internal).expect("record old-identity error");

    rotate_identity_state_then(|| Ok(())).expect("rotate state");
    let replay = preview_daily_batch().expect("preview replay");
    assert_eq!(occurrence_count(&replay, "setup_completed"), Some(1));
    assert_eq!(occurrence_count(&replay, "integration_detected"), Some(2));
    assert_eq!(sync_counts(&replay), None);
    assert_eq!(autopilot_counts(&replay), None);
    assert_eq!(autopilot_fallback_counts(&replay), None);
    assert_eq!(occurrence_count(&replay, "checkout_started"), None);
    assert_eq!(error_count(&replay, ErrorCategory::Internal), None);
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
    record_checkout_started().expect("record old-identity checkout");
    record_error_category(ErrorCategory::Internal).expect("record old-identity error");

    installation_id::reset().expect("simulate successful reset before sidecar cleanup");
    let current = preview_daily_batch().expect("preview rebound sidecar");
    assert_eq!(occurrence_count(&current, "setup_completed"), Some(1));
    assert_eq!(occurrence_count(&current, "integration_detected"), Some(1));
    assert_eq!(sync_counts(&current), None);
    assert_eq!(autopilot_counts(&current), None);
    assert_eq!(autopilot_fallback_counts(&current), None);
    assert_eq!(occurrence_count(&current, "checkout_started"), None);
    assert_eq!(error_count(&current, ErrorCategory::Internal), None);
}

fn tool_call_counts(batch: &TelemetryBatchV2) -> Vec<(String, u64, u64)> {
    batch
        .events
        .iter()
        .find_map(|envelope| match &envelope.event {
            TelemetryEventV2::ToolCallAggregate(metrics) => Some(
                metrics
                    .tools
                    .iter()
                    .map(|entry| (entry.tool.clone(), entry.calls, entry.failures))
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn counters(entries: &[(&str, u64, u64)]) -> BTreeMap<String, ToolCounterCheckpoint> {
    entries
        .iter()
        .map(|(tool, calls, failures)| {
            (
                (*tool).to_string(),
                ToolCounterCheckpoint {
                    calls: *calls,
                    failures: *failures,
                },
            )
        })
        .collect()
}

#[test]
fn tool_call_deltas_subtract_the_baseline_and_drop_invalid_names() {
    let observed = counters(&[
        ("ctx_read", 10, 3),
        ("ctx_shell", 4, 0),
        ("ctx_tree", 2, 0),
        ("Bad Name", 9, 0),
    ]);
    let baseline = counters(&[("ctx_read", 7, 1), ("ctx_tree", 2, 0)]);
    let deltas: Vec<_> = tool_call_deltas(&observed, &baseline)
        .into_iter()
        .map(|entry| (entry.tool, entry.calls, entry.failures))
        .collect();
    assert_eq!(
        deltas,
        vec![
            ("ctx_read".to_string(), 3, 2),
            ("ctx_shell".to_string(), 4, 0)
        ]
    );
}

#[test]
fn tool_call_deltas_keep_the_most_called_tools_past_the_entry_cap() {
    let observed: BTreeMap<_, _> = (0..MAX_TOOL_ENTRIES + 5)
        .map(|index| {
            (
                format!("tool_{index:04}"),
                ToolCounterCheckpoint {
                    calls: index as u64 + 1,
                    failures: 0,
                },
            )
        })
        .collect();
    let deltas = tool_call_deltas(&observed, &BTreeMap::new());
    assert_eq!(deltas.len(), MAX_TOOL_ENTRIES);
    assert!(deltas.iter().all(|entry| entry.calls > 5));
    ToolCallMetrics { tools: deltas }
        .validate()
        .expect("capped deltas satisfy the contract");
}

#[test]
fn checkpoint_written_before_per_tool_counting_still_loads() {
    let legacy = r#"{"tool_calls":3,"tool_failures":1,"tool_latency_buckets":[1,1,1,0,0,0,0,0,0],"session_uptime_secs":60}"#;
    let checkpoint: CounterCheckpoint = serde_json::from_str(legacy).expect("legacy checkpoint");
    assert!(checkpoint.tools.is_empty());
    assert_eq!(checkpoint.tool_calls, 3);
}

#[test]
#[serial_test::serial]
fn daily_batch_carries_one_setup_profile() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    let batch = preview_daily_batch().expect("preview");
    let profiles: Vec<_> = batch
        .events
        .iter()
        .filter_map(|envelope| match &envelope.event {
            TelemetryEventV2::SetupProfile(metrics) => Some(*metrics),
            _ => None,
        })
        .collect();
    assert_eq!(profiles, vec![setup_profile()]);
    batch.validate().expect("valid batch");
}

#[test]
#[serial_test::serial]
fn acknowledged_batch_resets_the_per_tool_baseline() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    let metrics = crate::core::telemetry::global_metrics();
    metrics.record_named_tool_call("telemetry_probe_tool", 1_000, true);
    let first = prepare_daily_batch().expect("prepare");
    assert!(
        tool_call_counts(&first)
            .iter()
            .any(|(tool, _, _)| tool == "telemetry_probe_tool")
    );
    acknowledge_daily_batch(&first).expect("acknowledge");

    metrics.record_named_tool_call("telemetry_probe_tool", 1_000, false);
    metrics.record_named_tool_call("telemetry_probe_tool", 1_000, true);
    let next = preview_daily_batch().expect("next preview");
    let probe: Vec<_> = tool_call_counts(&next)
        .into_iter()
        .filter(|(tool, _, _)| tool == "telemetry_probe_tool")
        .collect();
    assert_eq!(probe, vec![("telemetry_probe_tool".to_string(), 2, 1)]);
    next.validate().expect("valid batch");
}

fn probe_counts(batch: &TelemetryBatchV2, tool: &str) -> Option<(u64, u64)> {
    tool_call_counts(batch)
        .into_iter()
        .find(|(name, _, _)| name == tool)
        .map(|(_, calls, failures)| (calls, failures))
}

fn queued_counters() -> CounterCheckpoint {
    load_one_shots_at(&one_shot_path().expect("sidecar path"))
        .expect("sidecar")
        .queued
        .counters
}

#[test]
#[serial_test::serial]
fn persisted_counters_are_sent_once_and_later_calls_reach_the_next_batch() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    let metrics = crate::core::telemetry::global_metrics();
    metrics.record_named_tool_call("telemetry_fold_probe", 1_000, true);
    metrics.record_named_tool_call("telemetry_fold_probe", 1_000, false);
    persist_process_counters().expect("persist");
    assert_eq!(
        queued_counters().tools.get("telemetry_fold_probe").copied(),
        Some(ToolCounterCheckpoint {
            calls: 2,
            failures: 1
        })
    );
    // A second fold with no new calls must not count them again.
    persist_process_counters().expect("idempotent persist");

    let lease = begin_daily_send_in_bucket(Some(BUCKET)).expect("send");
    assert_eq!(
        probe_counts(lease.batch(), "telemetry_fold_probe"),
        Some((2, 1))
    );
    lease.commit().expect("commit");
    assert!(!queued_counters().tools.contains_key("telemetry_fold_probe"));

    // Calls after the daily send are kept for the next bucket, not dropped.
    metrics.record_named_tool_call("telemetry_fold_probe", 1_000, true);
    persist_process_counters().expect("persist after send");
    assert!(begin_daily_send_in_bucket(Some(BUCKET)).is_err());
    let next = begin_daily_send_in_bucket(Some(NEXT_BUCKET)).expect("next send");
    assert_eq!(
        probe_counts(next.batch(), "telemetry_fold_probe"),
        Some((1, 0))
    );
    next.batch().validate().expect("valid batch");
}

#[test]
#[serial_test::serial]
fn counters_persisted_by_an_exited_process_are_included() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    // Absorb this process's counters so only the other process's remain.
    persist_process_counters().expect("baseline");
    let path = one_shot_path().expect("sidecar path");
    let mut sidecar = load_one_shots_at(&path).expect("sidecar");
    sidecar.queued.counters = CounterCheckpoint::default();
    let mut buckets = [0; crate::core::telemetry::TOOL_LATENCY_BUCKET_UPPER_MS.len()];
    buckets[0] = 3;
    add_counters(
        &mut sidecar.queued.counters,
        &CounterCheckpoint {
            tool_calls: 3,
            tool_failures: 1,
            tool_latency_buckets: buckets,
            session_uptime_secs: 30,
            tools: counters(&[("telemetry_exited_probe", 3, 1)]),
        },
    );
    write_one_shots(&path, &sidecar).expect("seed exited process counters");

    let preview = preview_daily_batch().expect("preview");
    assert_eq!(
        probe_counts(&preview, "telemetry_exited_probe"),
        Some((3, 1))
    );
    assert!(tool_counts(&preview).0 >= 3);
    // The preview folded in memory only.
    assert_eq!(queued_counters().tool_calls, 3);
    let lease = begin_daily_send_in_bucket(Some(BUCKET)).expect("send");
    assert_eq!(
        probe_counts(lease.batch(), "telemetry_exited_probe"),
        Some((3, 1))
    );
    lease.commit().expect("commit");
    assert!(
        !queued_counters()
            .tools
            .contains_key("telemetry_exited_probe")
    );
}

#[test]
#[serial_test::serial]
fn calls_made_while_telemetry_is_off_are_never_back_filled() {
    let _iso = crate::core::data_dir::isolated_data_dir();
    persist_process_counters().expect("baseline");
    let metrics = crate::core::telemetry::global_metrics();
    {
        let _off = TelemetryEnvGuard::disable();
        metrics.record_named_tool_call("telemetry_optout_probe", 1_000, true);
        persist_process_counters().expect("skip while off");
    }
    persist_process_counters().expect("persist after re-enable");
    assert!(
        !queued_counters()
            .tools
            .contains_key("telemetry_optout_probe")
    );
}

#[test]
fn acknowledgement_keeps_totals_consistent_after_a_queue_reset() {
    let mut sent_buckets = [0; crate::core::telemetry::TOOL_LATENCY_BUCKET_UPPER_MS.len()];
    sent_buckets[0] = 5;
    let included = CounterCheckpoint {
        tool_calls: 5,
        tool_failures: 2,
        tool_latency_buckets: sent_buckets,
        session_uptime_secs: 10,
        tools: counters(&[("ctx_read", 5, 2)]),
    };
    let mut kept_buckets = [0; crate::core::telemetry::TOOL_LATENCY_BUCKET_UPPER_MS.len()];
    kept_buckets[0] = 1;
    kept_buckets[1] = 2;
    let mut total = CounterCheckpoint {
        tool_calls: 3,
        tool_failures: 3,
        tool_latency_buckets: kept_buckets,
        session_uptime_secs: 4,
        tools: counters(&[("ctx_read", 1, 1), ("ctx_tree", 2, 0)]),
    };
    subtract_counters(&mut total, &included);
    assert_eq!(
        total.tool_calls,
        total.tool_latency_buckets.iter().sum::<u64>()
    );
    assert_eq!(total.tool_calls, 2);
    assert!(total.tool_failures <= total.tool_calls);
    assert_eq!(total.tools, counters(&[("ctx_tree", 2, 0)]));
}

#[test]
fn sidecar_written_before_durable_counters_still_loads() {
    let mut legacy = serde_json::to_value(OneShotState::default()).expect("encode");
    legacy["queued"]
        .as_object_mut()
        .expect("queued object")
        .remove("counters");
    let state: OneShotState = serde_json::from_value(legacy).expect("legacy sidecar");
    assert_eq!(state.queued.counters, CounterCheckpoint::default());
}
