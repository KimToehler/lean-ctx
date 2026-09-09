// SPDX-License-Identifier: Apache-2.0

//! Privacy-bounded, versioned telemetry event contract.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SCHEMA_VERSION: u16 = 2;
pub const MAX_COUNT: u64 = 1_000_000_000;
pub const MAX_HISTOGRAM_BUCKETS: usize = 32;
pub const MAX_BATCH_EVENTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryBatchV2 {
    pub schema_version: u16,
    pub deletion_token_hash: String,
    pub events: Vec<TelemetryEnvelopeV2>,
}

impl TelemetryBatchV2 {
    pub fn validate(&self) -> Result<(), TelemetryValidationError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(TelemetryValidationError::SchemaVersion);
        }
        if !valid_digest(&self.deletion_token_hash) {
            return Err(TelemetryValidationError::DeletionTokenHash);
        }
        if self.events.is_empty() || self.events.len() > MAX_BATCH_EVENTS {
            return Err(TelemetryValidationError::BatchSize);
        }
        self.events
            .iter()
            .try_for_each(TelemetryEnvelopeV2::validate)
    }
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryEnvelopeV2 {
    pub schema_version: u16,
    pub timestamp_bucket: String,
    pub installation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<PseudonymousId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<PseudonymousId>,
    pub app_version: String,
    pub event: TelemetryEventV2,
}

impl TelemetryEnvelopeV2 {
    pub fn validate(&self) -> Result<(), TelemetryValidationError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(TelemetryValidationError::SchemaVersion);
        }
        NaiveDate::parse_from_str(&self.timestamp_bucket, "%Y-%m-%d")
            .map_err(|_| TelemetryValidationError::TimestampBucket)?;
        Uuid::parse_str(&self.installation_id)
            .map_err(|_| TelemetryValidationError::InstallationId)?;
        if self.app_version.is_empty()
            || self.app_version.len() > 64
            || !self
                .app_version
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        {
            return Err(TelemetryValidationError::AppVersion);
        }
        if self.account_id.as_ref().is_some_and(|id| !id.is_valid())
            || self
                .organization_id
                .as_ref()
                .is_some_and(|id| !id.is_valid())
        {
            return Err(TelemetryValidationError::PseudonymousId);
        }
        self.event.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PseudonymousId(String);

impl PseudonymousId {
    pub fn new(value: impl Into<String>) -> Result<Self, TelemetryValidationError> {
        let value = Self(value.into());
        if value.is_valid() {
            Ok(value)
        } else {
            Err(TelemetryValidationError::PseudonymousId)
        }
    }

    fn is_valid(&self) -> bool {
        self.0.strip_prefix("hmac-sha256:").is_some_and(|tail| {
            tail.len() == 64
                && tail
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name", content = "metrics", rename_all = "snake_case")]
pub enum TelemetryEventV2 {
    Heartbeat(HeartbeatMetrics),
    SetupCompleted(OccurrenceMetrics),
    IntegrationDetected(OccurrenceMetrics),
    SessionAggregate(SessionMetrics),
    ToolUsageAggregate(ToolUsageMetrics),
    AutopilotAggregate(DecisionMetrics),
    AutopilotFallbackAggregate(DecisionMetrics),
    SyncAggregate(SyncMetrics),
    TrialStarted(OccurrenceMetrics),
    TrialEnded(OutcomeMetrics),
    UpgradeViewed(OccurrenceMetrics),
    CheckoutStarted(OccurrenceMetrics),
    SubscriptionActivated(OccurrenceMetrics),
    SubscriptionCancelled(OccurrenceMetrics),
    TeamCreated(OccurrenceMetrics),
    TeamMemberInvited(OccurrenceMetrics),
    TeamContextPromoted(OutcomeMetrics),
    ErrorCategoryAggregate(ErrorMetrics),
    VersionUpgrade(VersionUpgradeMetrics),
    OrchestrationAggregate(Box<OrchestrationMetrics>),
}

impl TelemetryEventV2 {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Heartbeat(_) => "heartbeat",
            Self::SetupCompleted(_) => "setup_completed",
            Self::IntegrationDetected(_) => "integration_detected",
            Self::SessionAggregate(_) => "session_aggregate",
            Self::ToolUsageAggregate(_) => "tool_usage_aggregate",
            Self::AutopilotAggregate(_) => "autopilot_aggregate",
            Self::AutopilotFallbackAggregate(_) => "autopilot_fallback_aggregate",
            Self::SyncAggregate(_) => "sync_aggregate",
            Self::TrialStarted(_) => "trial_started",
            Self::TrialEnded(_) => "trial_ended",
            Self::UpgradeViewed(_) => "upgrade_viewed",
            Self::CheckoutStarted(_) => "checkout_started",
            Self::SubscriptionActivated(_) => "subscription_activated",
            Self::SubscriptionCancelled(_) => "subscription_cancelled",
            Self::TeamCreated(_) => "team_created",
            Self::TeamMemberInvited(_) => "team_member_invited",
            Self::TeamContextPromoted(_) => "team_context_promoted",
            Self::ErrorCategoryAggregate(_) => "error_category_aggregate",
            Self::VersionUpgrade(_) => "version_upgrade",
            Self::OrchestrationAggregate(_) => "orchestration_aggregate",
        }
    }

    fn validate(&self) -> Result<(), TelemetryValidationError> {
        match self {
            Self::Heartbeat(metrics) => metrics.validate(),
            Self::SetupCompleted(metrics)
            | Self::IntegrationDetected(metrics)
            | Self::TrialStarted(metrics)
            | Self::UpgradeViewed(metrics)
            | Self::CheckoutStarted(metrics)
            | Self::SubscriptionActivated(metrics)
            | Self::SubscriptionCancelled(metrics)
            | Self::TeamCreated(metrics)
            | Self::TeamMemberInvited(metrics) => metrics.validate(),
            Self::SessionAggregate(metrics) => metrics.validate(),
            Self::ToolUsageAggregate(metrics) => metrics.validate(),
            Self::AutopilotAggregate(metrics) | Self::AutopilotFallbackAggregate(metrics) => {
                metrics.validate()
            }
            Self::SyncAggregate(metrics) => metrics.validate(),
            Self::TrialEnded(metrics) | Self::TeamContextPromoted(metrics) => metrics.validate(),
            Self::ErrorCategoryAggregate(metrics) => metrics.validate(),
            Self::VersionUpgrade(metrics) => metrics.validate(),
            Self::OrchestrationAggregate(metrics) => metrics.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatMetrics {
    pub distribution_channel: DistributionChannel,
    pub client_family: ClientFamily,
    pub operating_system: OperatingSystem,
    pub architecture: Architecture,
}

impl HeartbeatMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OccurrenceMetrics {
    pub count: u64,
}

impl OccurrenceMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded(self.count)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeMetrics {
    pub accepted: u64,
    pub rejected: u64,
    pub unknown: u64,
}

impl OutcomeMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded_many(&[self.accepted, self.rejected, self.unknown])
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionMetrics {
    pub sessions: u64,
    pub duration_seconds: Histogram,
}

impl SessionMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded(self.sessions)?;
        self.duration_seconds.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolUsageMetrics {
    pub calls: u64,
    pub failures: u64,
    pub latency_milliseconds: Histogram,
}

impl ToolUsageMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded_many(&[self.calls, self.failures])?;
        if self.failures > self.calls {
            return Err(TelemetryValidationError::InconsistentCounts);
        }
        self.latency_milliseconds.validate()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionMetrics {
    pub admitted: u64,
    pub denied: u64,
    pub fallback: u64,
}

impl DecisionMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded_many(&[self.admitted, self.denied, self.fallback])
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncMetrics {
    pub attempts: u64,
    pub successes: u64,
    pub failures: u64,
}

impl SyncMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded_many(&[self.attempts, self.successes, self.failures])?;
        if self.successes.saturating_add(self.failures) > self.attempts {
            return Err(TelemetryValidationError::InconsistentCounts);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorMetrics {
    pub category: ErrorCategory,
    pub count: u64,
}

impl ErrorMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded(self.count)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionUpgradeMetrics {
    pub from_major: u16,
    pub to_major: u16,
}

impl VersionUpgradeMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        if self.to_major < self.from_major {
            return Err(TelemetryValidationError::VersionDirection);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestrationMetrics {
    pub admitted_tasks: u64,
    pub execution_plans: u64,
    pub retries: u64,
    pub fallbacks: u64,
    pub cancellations: u64,
    pub lease_conflicts: u64,
    pub receipts: u64,
    pub outcomes: OutcomeMetrics,
    pub node_count: Histogram,
    pub fan_out: Histogram,
    pub depth: Histogram,
    pub parallel_nodes: Histogram,
}

impl OrchestrationMetrics {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        bounded_many(&[
            self.admitted_tasks,
            self.execution_plans,
            self.retries,
            self.fallbacks,
            self.cancellations,
            self.lease_conflicts,
            self.receipts,
        ])?;
        self.outcomes.validate()?;
        self.node_count.validate()?;
        self.fan_out.validate()?;
        self.depth.validate()?;
        self.parallel_nodes.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Histogram {
    pub upper_bounds: Vec<u64>,
    pub counts: Vec<u64>,
}

impl Histogram {
    fn validate(&self) -> Result<(), TelemetryValidationError> {
        if self.upper_bounds.is_empty()
            || self.upper_bounds.len() > MAX_HISTOGRAM_BUCKETS
            || self.upper_bounds.len() != self.counts.len()
            || !self
                .upper_bounds
                .windows(2)
                .all(|window| window[0] < window[1])
        {
            return Err(TelemetryValidationError::Histogram);
        }
        bounded_many(&self.counts)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DistributionChannel {
    Cargo,
    Homebrew,
    Npm,
    Docker,
    Source,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientFamily {
    Claude,
    Codex,
    Cursor,
    Gemini,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatingSystem {
    Macos,
    Linux,
    Windows,
    Other,
}

impl OperatingSystem {
    #[must_use]
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Self::Macos,
            "linux" => Self::Linux,
            "windows" => Self::Windows,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X86_64,
    Aarch64,
    Other,
}

impl Architecture {
    #[must_use]
    pub fn current() -> Self {
        match std::env::consts::ARCH {
            "x86_64" => Self::X86_64,
            "aarch64" => Self::Aarch64,
            _ => Self::Other,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Authentication,
    Authorization,
    Configuration,
    Network,
    Provider,
    Timeout,
    Validation,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryValidationError {
    SchemaVersion,
    DeletionTokenHash,
    TimestampBucket,
    InstallationId,
    PseudonymousId,
    AppVersion,
    CountBound,
    InconsistentCounts,
    Histogram,
    VersionDirection,
    BatchSize,
}

fn bounded(value: u64) -> Result<(), TelemetryValidationError> {
    if value <= MAX_COUNT {
        Ok(())
    } else {
        Err(TelemetryValidationError::CountBound)
    }
}

fn bounded_many(values: &[u64]) -> Result<(), TelemetryValidationError> {
    values.iter().try_for_each(|value| bounded(*value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(event: TelemetryEventV2) -> TelemetryEnvelopeV2 {
        TelemetryEnvelopeV2 {
            schema_version: SCHEMA_VERSION,
            timestamp_bucket: "2026-09-08".into(),
            installation_id: "550e8400-e29b-41d4-a716-446655440000".into(),
            account_id: None,
            organization_id: None,
            app_version: "4.0.0".into(),
            event,
        }
    }

    #[test]
    fn valid_typed_event_roundtrips() {
        let event = envelope(TelemetryEventV2::ToolUsageAggregate(ToolUsageMetrics {
            calls: 4,
            failures: 1,
            latency_milliseconds: Histogram {
                upper_bounds: vec![10, 100],
                counts: vec![1, 3],
            },
        }));
        event.validate().unwrap();
        let encoded = serde_json::to_vec(&event).unwrap();
        let decoded: TelemetryEnvelopeV2 = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn batch_is_bounded_and_validates_every_event() {
        let event = envelope(TelemetryEventV2::Heartbeat(HeartbeatMetrics {
            distribution_channel: DistributionChannel::Cargo,
            client_family: ClientFamily::Codex,
            operating_system: OperatingSystem::Linux,
            architecture: Architecture::X86_64,
        }));
        let batch = TelemetryBatchV2 {
            schema_version: SCHEMA_VERSION,
            deletion_token_hash: "a".repeat(64),
            events: vec![event],
        };
        assert!(batch.validate().is_ok());
        let mut invalid_credential = batch.clone();
        invalid_credential.deletion_token_hash = "raw-secret".into();
        assert_eq!(
            invalid_credential.validate(),
            Err(TelemetryValidationError::DeletionTokenHash)
        );
        let empty = TelemetryBatchV2 {
            schema_version: SCHEMA_VERSION,
            deletion_token_hash: "a".repeat(64),
            events: Vec::new(),
        };
        assert_eq!(empty.validate(), Err(TelemetryValidationError::BatchSize));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let raw = r#"{"schema_version":2,"timestamp_bucket":"2026-09-08","installation_id":"550e8400-e29b-41d4-a716-446655440000","app_version":"4.0.0","raw_task":"secret","event":{"name":"heartbeat","metrics":{"distribution_channel":"cargo","client_family":"codex","operating_system":"linux","architecture":"x86_64"}}}"#;
        assert!(serde_json::from_str::<TelemetryEnvelopeV2>(raw).is_err());
    }

    #[test]
    fn malformed_identity_date_and_version_are_rejected() {
        let mut event = envelope(TelemetryEventV2::Heartbeat(HeartbeatMetrics {
            distribution_channel: DistributionChannel::Cargo,
            client_family: ClientFamily::Codex,
            operating_system: OperatingSystem::Linux,
            architecture: Architecture::X86_64,
        }));
        event.installation_id = "not-a-uuid".into();
        assert_eq!(
            event.validate(),
            Err(TelemetryValidationError::InstallationId)
        );
        event.installation_id = "550e8400-e29b-41d4-a716-446655440000".into();
        event.timestamp_bucket = "2026-02-30".into();
        assert_eq!(
            event.validate(),
            Err(TelemetryValidationError::TimestampBucket)
        );
        event.timestamp_bucket = "2026-09-08".into();
        event.schema_version = 3;
        assert_eq!(
            event.validate(),
            Err(TelemetryValidationError::SchemaVersion)
        );
    }

    #[test]
    fn bounds_and_cross_field_counts_fail_closed() {
        let event = envelope(TelemetryEventV2::SyncAggregate(SyncMetrics {
            attempts: 1,
            successes: 1,
            failures: 1,
        }));
        assert_eq!(
            event.validate(),
            Err(TelemetryValidationError::InconsistentCounts)
        );
        let event = envelope(TelemetryEventV2::SetupCompleted(OccurrenceMetrics {
            count: MAX_COUNT + 1,
        }));
        assert_eq!(event.validate(), Err(TelemetryValidationError::CountBound));
    }

    #[test]
    fn histogram_shape_and_order_are_bounded() {
        let histogram = Histogram {
            upper_bounds: vec![100, 10],
            counts: vec![1, 1],
        };
        assert_eq!(
            histogram.validate(),
            Err(TelemetryValidationError::Histogram)
        );
    }

    #[test]
    fn pseudonymous_ids_never_accept_raw_identifiers() {
        assert!(PseudonymousId::new("customer@example.com").is_err());
        assert!(PseudonymousId::new(format!("hmac-sha256:{}", "a".repeat(64))).is_ok());
    }
}
