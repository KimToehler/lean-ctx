//! Typed error-category telemetry for MCP dispatch (#telemetry-v2).
//!
//! Only the category derived from a typed outcome or protocol code is
//! aggregated — never the error message or tool output.

use crate::core::telemetry_v2::ErrorCategory;
use crate::server::tool_trait::ShellOutcome;

pub(super) fn record_shell_error_category(outcome: Option<&ShellOutcome>, output: &str) {
    let Some(category) = shell_error_category(outcome, output) else {
        return;
    };
    if let Some(ShellOutcome::Background(background)) = outcome {
        if let Err(error) = crate::server::background_shell::record_error_telemetry_once(
            &background.job_id,
            category,
        ) {
            tracing::debug!(%error, "error telemetry aggregation failed");
        }
    } else {
        record_error_category(category);
    }
}

fn shell_error_category(outcome: Option<&ShellOutcome>, output: &str) -> Option<ErrorCategory> {
    let outcome = outcome.filter(|outcome| super::outcome::is_shell_error(outcome, output))?;
    match outcome {
        ShellOutcome::Blocked => Some(ErrorCategory::Authorization),
        ShellOutcome::Exit(124) => Some(ErrorCategory::Timeout),
        ShellOutcome::Background(background) if background.exit_code == Some(124) => {
            Some(ErrorCategory::Timeout)
        }
        ShellOutcome::Exit(_) | ShellOutcome::Background(_) => Some(ErrorCategory::Internal),
        ShellOutcome::BackgroundLookupError(_) => None,
    }
}

pub(super) fn record_mcp_error(code: rmcp::model::ErrorCode) {
    record_error_category(mcp_error_category(code));
}

fn record_error_category(category: ErrorCategory) {
    if let Err(error) = crate::core::telemetry_aggregate::record_error_category(category) {
        tracing::debug!(%error, "error telemetry aggregation failed");
    }
}

fn mcp_error_category(code: rmcp::model::ErrorCode) -> ErrorCategory {
    if code == rmcp::model::ErrorCode::INVALID_PARAMS {
        ErrorCategory::Validation
    } else {
        ErrorCategory::Internal
    }
}

#[cfg(test)]
mod tests {
    use super::{mcp_error_category, shell_error_category};
    use crate::core::telemetry_v2::ErrorCategory;
    use crate::server::tool_trait::{BackgroundJobState, BackgroundShellOutcome, ShellOutcome};

    #[test]
    fn shell_error_categories_use_typed_outcomes_not_output_strings() {
        assert_eq!(shell_error_category(None, "secret error"), None);
        assert_eq!(
            shell_error_category(Some(&ShellOutcome::Exit(0)), "error"),
            None
        );
        assert_eq!(
            shell_error_category(Some(&ShellOutcome::Blocked), "arbitrary details"),
            Some(ErrorCategory::Authorization)
        );
        assert_eq!(
            shell_error_category(Some(&ShellOutcome::Exit(124)), "[exit:124]"),
            Some(ErrorCategory::Timeout)
        );
        assert_eq!(
            shell_error_category(Some(&ShellOutcome::Exit(2)), "provider timeout"),
            Some(ErrorCategory::Internal)
        );
        assert_eq!(
            shell_error_category(Some(&ShellOutcome::Exit(1)), "grep found no matches"),
            None
        );
        let failed_background = ShellOutcome::Background(BackgroundShellOutcome {
            state: BackgroundJobState::Failed,
            exit_code: Some(124),
            job_id: "opaque-job".into(),
            archive_id: None,
            archive_truncated: None,
            captured_chars: None,
            archived_chars: None,
            summary: "typed summary".into(),
            is_error: true,
            display: None,
        });
        assert_eq!(
            shell_error_category(Some(&failed_background), "arbitrary details"),
            Some(ErrorCategory::Timeout)
        );
    }

    #[test]
    fn mcp_error_categories_use_protocol_codes() {
        assert_eq!(
            mcp_error_category(rmcp::model::ErrorCode::INVALID_PARAMS),
            ErrorCategory::Validation
        );
        assert_eq!(
            mcp_error_category(rmcp::model::ErrorCode::INTERNAL_ERROR),
            ErrorCategory::Internal
        );
    }
}
