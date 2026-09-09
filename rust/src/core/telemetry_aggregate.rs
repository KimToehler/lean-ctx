//! Privacy-safe daily telemetry aggregation.

use sha2::Digest;

use super::installation_id;
use super::telemetry_v2::{
    Architecture, ClientFamily, DistributionChannel, HeartbeatMetrics, OperatingSystem,
    SCHEMA_VERSION, TelemetryBatchV2, TelemetryEnvelopeV2, TelemetryEventV2,
};

pub fn pending_daily_batch() -> Result<TelemetryBatchV2, String> {
    let (installation_id, deletion_token) = installation_id::get_or_create_identity()
        .map_err(|error| format!("installation ID unavailable: {error}"))?;
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    build_daily_heartbeat(
        installation_id,
        hex::encode(sha2::Sha256::digest(deletion_token.as_bytes())),
        date,
        distribution_channel(),
        client_family(),
    )
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
        for forbidden in ["prompt", "source_code", "file_path", "command", "secret"] {
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
}
