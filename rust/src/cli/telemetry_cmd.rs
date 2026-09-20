//! CLI commands for the anonymous telemetry heartbeat.
//!
//! `lean-ctx telemetry [status|on|off|reset-id|show]`

use crate::core::config;
use crate::core::installation_id;
use std::io::IsTerminal;

const DEFAULT_ON_NOTICE: &str = "LeanCTX anonymous product telemetry is enabled by default.\n\nSent: version, OS/arch, anonymous install ID, coarse feature/health aggregates.\nNever sent: prompts, source code, file contents, filenames, commands, secrets.\n\nInspect:  lean-ctx telemetry show\nDisable:  lean-ctx telemetry off\nHistory:  lean-ctx telemetry history";

pub(crate) fn maybe_show_default_on_notice() {
    let terminal = std::io::stderr().is_terminal();
    let ci = std::env::var_os("CI").is_some();
    let do_not_track = std::env::var("DO_NOT_TRACK").ok();
    let telemetry_override = std::env::var("LEAN_CTX_TELEMETRY").ok();
    let cfg = config::Config::load_global();
    if !should_show_default_on_notice(
        &cfg.telemetry,
        terminal,
        ci,
        do_not_track.as_deref(),
        telemetry_override.as_deref(),
    ) {
        return;
    }

    if config::setter::set_by_key("telemetry.notice_shown", "true").is_ok() {
        eprintln!("{DEFAULT_ON_NOTICE}");
    }
}

fn should_show_default_on_notice(
    telemetry: &config::TelemetryConfig,
    terminal: bool,
    ci: bool,
    do_not_track: Option<&str>,
    env_override: Option<&str>,
) -> bool {
    terminal
        && !ci
        && !telemetry.notice_shown
        && !telemetry.explicitly_disabled()
        && !config::TelemetryConfig::environment_disables(do_not_track, env_override)
}

pub(super) fn cmd_telemetry(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("status");

    match sub {
        "status" => show_status(),
        "on" | "enable" => set_enabled(true),
        "off" | "disable" => set_enabled(false),
        "reset-id" => reset_id(),
        "show" | "pending" => show_payload(),
        "history" | "log" => show_history(),
        "purge-local" => purge_local(),
        "delete-remote" => delete_remote(),
        "--help" | "-h" => print_help(),
        other => {
            eprintln!("telemetry: unknown subcommand '{other}'");
            print_help();
            std::process::exit(1);
        }
    }
}

/// Why the effective send path is inactive despite the persisted preference.
///
/// The verdict itself always comes from [`config::TelemetryConfig::send_eligible`];
/// this only explains a `false` from that same authority, reusing its own public
/// predicates instead of re-interpreting the opt-out rules a second time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendBlocker {
    /// Persisted opt-out: `telemetry off`, or a legacy `enabled = false`.
    Preference,
    /// `DO_NOT_TRACK=1`, or `LEAN_CTX_TELEMETRY=off|false|0|no`.
    Environment,
    /// The one-time default-on notice has not been processed yet.
    Notice,
    /// The authority refuses for a reason this display does not model yet.
    Policy,
}

impl SendBlocker {
    fn describe(self) -> &'static str {
        match self {
            Self::Preference => "off by your saved preference",
            Self::Environment => "blocked by the environment (DO_NOT_TRACK / LEAN_CTX_TELEMETRY)",
            Self::Notice => "waiting for the one-time notice",
            Self::Policy => "blocked by telemetry policy",
        }
    }
}

/// Classifies a non-eligible state. `None` means the authority itself says the
/// installation is send-eligible, so the order below never decides eligibility —
/// it only picks the reason to show, most-persistent cause first.
fn send_blocker(
    telemetry: &config::TelemetryConfig,
    do_not_track: Option<&str>,
    env_override: Option<&str>,
) -> Option<SendBlocker> {
    if telemetry.send_eligible(do_not_track, env_override) {
        return None;
    }
    if telemetry.explicitly_disabled() {
        Some(SendBlocker::Preference)
    } else if config::TelemetryConfig::environment_disables(do_not_track, env_override) {
        Some(SendBlocker::Environment)
    } else if !telemetry.notice_shown {
        Some(SendBlocker::Notice)
    } else {
        Some(SendBlocker::Policy)
    }
}

fn show_status() {
    // Global-only, like every path that actually sends (`show_payload`,
    // `cloud_sync::cloud_background_tasks`): a project-local override would
    // otherwise be shown as if it gated transmission.
    let cfg = config::Config::load_global();
    let enabled = !cfg.telemetry.explicitly_disabled();
    let blocker = send_blocker(
        &cfg.telemetry,
        std::env::var("DO_NOT_TRACK").ok().as_deref(),
        std::env::var("LEAN_CTX_TELEMETRY").ok().as_deref(),
    );
    let last = cfg.telemetry.last_heartbeat.as_deref().unwrap_or("never");

    println!(
        "  Preference: {}",
        if enabled {
            "\x1b[32menabled\x1b[0m"
        } else {
            "\x1b[2mdisabled\x1b[0m"
        }
    );
    match blocker {
        None => println!("  Sending:    \x1b[32mactive\x1b[0m"),
        Some(reason) => println!(
            "  Sending:    \x1b[2minactive — {}\x1b[0m",
            reason.describe()
        ),
    }

    if let Ok(id) = installation_id::get_or_create() {
        println!("  Install ID: {}", installation_id::masked(&id));
    }
    println!("  Last sent:  {last}");
    println!();

    if enabled {
        println!("  \x1b[2mDisable: lean-ctx telemetry off\x1b[0m");
    } else {
        println!("  \x1b[2mEnable:  lean-ctx telemetry on\x1b[0m");
    }
    println!("  \x1b[2mInspect: lean-ctx telemetry show\x1b[0m");
}

fn set_enabled(enabled: bool) {
    let preference = if enabled {
        "explicitly_enabled"
    } else {
        "explicitly_disabled"
    };
    match config::setter::set_many_by_key(&[
        ("telemetry.enabled", if enabled { "true" } else { "false" }),
        ("telemetry.preference", preference),
        ("telemetry.notice_shown", "true"),
        ("cloud.contribute_enabled", "false"),
    ]) {
        Ok(_) => {
            if enabled {
                println!("Telemetry enabled — thank you for helping improve lean-ctx!");
                println!("Sent daily: version, OS, arch, compression patterns, random install ID.");
                println!("No code, no file names, no personal data — ever.");
                println!("\x1b[2mDisable anytime: lean-ctx telemetry off\x1b[0m");
            } else {
                println!("Telemetry disabled. No data will be sent.");
                println!("\x1b[2mRe-enable: lean-ctx telemetry on\x1b[0m");
            }
        }
        Err(e) => {
            eprintln!("Failed to update config: {e}");
            std::process::exit(1);
        }
    }
}

fn reset_id() {
    let (current_id, deletion_token) = match installation_id::get_or_create_identity() {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("Failed to read telemetry identity: {error}");
            std::process::exit(1);
        }
    };
    match crate::cloud_client::delete_remote_telemetry(&current_id, &deletion_token) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "Installation ID was not reset because its remote telemetry could not be deleted."
            );
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("Installation ID was not reset: {error}");
            std::process::exit(1);
        }
    }
    match crate::core::telemetry_aggregate::rotate_identity_state_then(installation_id::reset) {
        Ok(new_id) => {
            println!(
                "Installation ID regenerated: {}",
                installation_id::masked(&new_id)
            );
            println!("\x1b[2mThe old ID is gone — the server cannot correlate old and new.\x1b[0m");
        }
        Err(e) => {
            eprintln!("Failed to reset installation ID: {e}");
            std::process::exit(1);
        }
    }
}

fn show_payload() {
    let cfg = config::Config::load_global();
    let do_not_track = std::env::var("DO_NOT_TRACK").ok();
    let telemetry_override = std::env::var("LEAN_CTX_TELEMETRY").ok();
    if !cfg
        .telemetry
        .send_eligible(do_not_track.as_deref(), telemetry_override.as_deref())
    {
        println!("No telemetry payload is currently eligible for sending.");
        return;
    }
    let payload = match crate::core::telemetry_aggregate::pending_daily_batch() {
        Ok(payload) => payload,
        Err(error) => {
            eprintln!("Unable to build telemetry payload: {error}");
            return;
        }
    };

    println!("This is the exact JSON that would be sent to api.leanctx.com:");
    println!();
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    );
    println!();
    println!(
        "\x1b[2mEndpoint: POST {}/api/telemetry/v2/batch\x1b[0m",
        api_url()
    );
    println!("\x1b[2mFrequency: at most once per day\x1b[0m");
    println!("\x1b[2mAuthentication: none\x1b[0m");
}

fn api_url() -> String {
    std::env::var("LEAN_CTX_API_URL").unwrap_or_else(|_| "https://api.leanctx.com".to_string())
}

fn show_history() {
    let records = crate::core::telemetry_ledger::read_all();
    if records.is_empty() {
        println!("No heartbeats sent yet.");
        println!("\x1b[2mEnable with: lean-ctx telemetry on\x1b[0m");
        return;
    }
    let header = format!(
        "  \x1b[1m{:<28} {:<12} {:<10} {}\x1b[0m",
        "Timestamp", "Version", "OS", "Arch"
    );
    println!("{header}");
    println!("  {}", "\u{2500}".repeat(65));
    for record in records.iter().rev().take(50) {
        println!(
            "  {:<28} {:<12} {:<10} {}",
            record.timestamp, record.version, record.os, record.arch,
        );
        if record.schema_version > 0 {
            println!(
                "    schema={} status={} events={} hash={} endpoint={}",
                record.schema_version,
                record.status,
                record.event_names.join(","),
                record.payload_hash,
                record.endpoint
            );
        }
    }
    println!();
    println!(
        "  \x1b[2m{} total heartbeats recorded\x1b[0m",
        records.len()
    );
}

fn purge_local() {
    match crate::core::telemetry_ledger::purge_local()
        .and_then(|()| crate::core::telemetry_aggregate::purge_local_state())
    {
        Ok(()) => println!("Local telemetry history purged."),
        Err(error) => {
            eprintln!("Failed to purge local telemetry history: {error}");
            std::process::exit(1);
        }
    }
}

fn delete_remote() {
    let (installation_id, deletion_token) = match installation_id::get_or_create_identity() {
        Ok(identity) => identity,
        Err(error) => {
            eprintln!("Failed to read telemetry identity: {error}");
            std::process::exit(1);
        }
    };
    match crate::cloud_client::delete_remote_telemetry(&installation_id, &deletion_token) {
        Ok(true) => {
            match crate::core::telemetry_aggregate::rotate_identity_state_then(
                installation_id::reset,
            ) {
                Ok(_) => {
                    println!("Remote telemetry was deleted and the local identity was rotated.");
                }
                Err(error) => {
                    eprintln!(
                        "Remote telemetry deleted, but local identity rotation failed: {error}"
                    );
                    std::process::exit(1);
                }
            }
        }
        Ok(false) => {
            eprintln!(
                "Remote telemetry was not deleted; send one current v2 batch first to register the deletion credential."
            );
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("Failed to delete remote telemetry: {error}");
            std::process::exit(1);
        }
    }
}

fn print_help() {
    println!("Usage: lean-ctx telemetry [subcommand]");
    println!();
    println!("Manage privacy-safe telemetry (default-on, fully disableable, no PII).");
    println!();
    println!("Subcommands:");
    println!("  status     Show current telemetry status (default)");
    println!("  on         Enable anonymous heartbeat");
    println!("  off        Disable anonymous heartbeat");
    println!("  show       Display the exact payload that would be sent");
    println!("  pending    Display the exact typed batch currently eligible for sending");
    println!("  reset-id   Regenerate the anonymous installation ID");
    println!("  history    Show log of all sent heartbeats");
    println!("  purge-local Delete the local telemetry history");
    println!("  delete-remote Delete server-side telemetry for this installation");
    println!();
    println!("The heartbeat sends: version, OS, architecture, compression patterns,");
    println!("and a random install UUID. No code, filenames, or personal data — ever.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_notice_requires_an_eligible_interactive_run() {
        let cfg = config::TelemetryConfig::default();
        assert!(should_show_default_on_notice(&cfg, true, false, None, None));
        assert!(!should_show_default_on_notice(
            &cfg, false, false, None, None
        ));
        assert!(!should_show_default_on_notice(&cfg, true, true, None, None));
        assert!(!should_show_default_on_notice(
            &cfg,
            true,
            false,
            Some("1"),
            None
        ));
        assert!(!should_show_default_on_notice(
            &cfg,
            true,
            false,
            None,
            Some("off")
        ));
    }

    #[test]
    fn default_notice_never_overrides_persisted_user_state() {
        let shown = config::TelemetryConfig {
            notice_shown: true,
            ..config::TelemetryConfig::default()
        };
        assert!(!should_show_default_on_notice(
            &shown, true, false, None, None
        ));
        let disabled = config::TelemetryConfig {
            enabled: false,
            ..config::TelemetryConfig::default()
        };
        assert!(!should_show_default_on_notice(
            &disabled, true, false, None, None
        ));
    }

    /// Eligible state, and each reason the status line must be able to name.
    #[test]
    fn status_names_every_reason_sending_is_inactive() {
        // Default-on but pre-notice: enabled as a preference, not yet sending.
        let mut cfg = config::TelemetryConfig::default();
        assert_eq!(send_blocker(&cfg, None, None), Some(SendBlocker::Notice));

        cfg.notice_shown = true;
        assert_eq!(send_blocker(&cfg, None, None), None);

        assert_eq!(
            send_blocker(&cfg, Some("1"), None),
            Some(SendBlocker::Environment)
        );
        for value in ["off", "false", "0", "no", " OFF "] {
            assert_eq!(
                send_blocker(&cfg, None, Some(value)),
                Some(SendBlocker::Environment),
                "LEAN_CTX_TELEMETRY={value} must block sending"
            );
        }
        // Only the exact opt-out spellings block; nothing else is invented here.
        assert_eq!(send_blocker(&cfg, Some("0"), None), None);
        assert_eq!(send_blocker(&cfg, None, Some("on")), None);

        let disabled = config::TelemetryConfig {
            preference: config::TelemetryPreference::ExplicitlyDisabled,
            ..cfg.clone()
        };
        assert_eq!(
            send_blocker(&disabled, None, None),
            Some(SendBlocker::Preference)
        );
        // A persisted opt-out is reported as such even when the environment
        // would independently block the send.
        assert_eq!(
            send_blocker(&disabled, Some("1"), Some("off")),
            Some(SendBlocker::Preference)
        );

        let legacy = config::TelemetryConfig {
            enabled: false,
            ..cfg.clone()
        };
        assert_eq!(
            send_blocker(&legacy, None, None),
            Some(SendBlocker::Preference)
        );
    }

    /// The display must never disagree with the eligibility authority: across
    /// every reachable combination, "no blocker" is exactly `send_eligible`.
    #[test]
    fn status_never_contradicts_the_eligibility_authority() {
        let preferences = [
            config::TelemetryPreference::DefaultOn,
            config::TelemetryPreference::ExplicitlyEnabled,
            config::TelemetryPreference::ExplicitlyDisabled,
        ];
        let environments = [None, Some("0"), Some("1"), Some("off"), Some("no")];
        for enabled in [true, false] {
            for preference in preferences {
                for notice_shown in [true, false] {
                    for do_not_track in environments {
                        for env_override in environments {
                            let cfg = config::TelemetryConfig {
                                enabled,
                                preference,
                                notice_shown,
                                last_heartbeat: None,
                            };
                            assert_eq!(
                                send_blocker(&cfg, do_not_track, env_override).is_none(),
                                cfg.send_eligible(do_not_track, env_override),
                                "disagreement for enabled={enabled} preference={preference:?} \
                                 notice_shown={notice_shown} \
                                 DO_NOT_TRACK={do_not_track:?} LEAN_CTX_TELEMETRY={env_override:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Reporting status is read-only: it never rewrites the persisted choice.
    #[test]
    fn status_leaves_the_persisted_choice_untouched() {
        let cfg = config::TelemetryConfig {
            enabled: true,
            preference: config::TelemetryPreference::ExplicitlyEnabled,
            notice_shown: false,
            last_heartbeat: Some("2026-09-20".to_string()),
        };
        let _ = send_blocker(&cfg, Some("1"), Some("off"));
        assert!(cfg.enabled);
        assert_eq!(
            cfg.preference,
            config::TelemetryPreference::ExplicitlyEnabled
        );
        assert!(!cfg.notice_shown);
        assert_eq!(cfg.last_heartbeat.as_deref(), Some("2026-09-20"));
    }
}
