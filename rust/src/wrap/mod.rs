//! `lean-ctx wrap <agent>` — one-command setup for any supported agent.
//!
//! Orchestrates shell hooks, MCP registration, agent hooks, daemon, and
//! optional IDE launch into a single idempotent operation.  Every file
//! mutation is recorded in a snapshot so `lean-ctx unwrap <agent>` can
//! restore the pre-wrap state byte-for-byte.

mod launch;
mod snapshot;
mod unwrap;
mod verify;

use crate::core::editor_registry::{self, EditorTarget, WriteOptions};
use crate::core::portable_binary::resolve_agent_binary;
use crate::hooks::{self, HookMode};

use snapshot::WrapSnapshot;

pub use unwrap::run_unwrap;

/// Entry point for `lean-ctx wrap <agent>`.
pub fn run_wrap(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return;
    }

    let agent_key = match args.first() {
        Some(a) if !a.starts_with('-') => a.as_str(),
        _ => {
            let detected = detect_single_agent();
            if let Some(agent) = detected {
                eprintln!("Detected: {agent}");
                run_wrap_for_agent(&agent);
                return;
            }
            eprintln!("Usage: lean-ctx wrap <agent>");
            eprintln!();
            eprintln!("Supported agents:");
            for name in available_agent_keys() {
                eprintln!("  {name}");
            }
            eprintln!();
            eprintln!("Example: lean-ctx wrap cursor");
            std::process::exit(1);
        }
    };

    run_wrap_for_agent(agent_key);
}

fn run_wrap_for_agent(agent_key: &str) {
    let Some(home) = dirs::home_dir() else {
        eprintln!("Cannot determine home directory");
        std::process::exit(1);
    };

    let targets = editor_registry::build_targets(&home);
    let matching: Vec<&EditorTarget> = targets
        .iter()
        .filter(|t| t.agent_key == agent_key)
        .collect();

    if matching.is_empty() {
        eprintln!("Unknown agent: '{agent_key}'");
        eprintln!();
        eprintln!("Supported agents:");
        for name in available_agent_keys() {
            eprintln!("  {name}");
        }
        std::process::exit(1);
    }

    let binary = resolve_agent_binary();
    let mut snap = WrapSnapshot::new(agent_key);

    // --- Step 1: Snapshot existing configs ---
    for target in &matching {
        snap.record_file(&target.config_path);
    }

    // --- Step 2: Shell hooks ---
    eprintln!("  Installing shell hooks...");
    crate::shell_hook::install_all(true);

    // --- Step 3: MCP config ---
    eprintln!("  Registering MCP server...");
    let mut mcp_ok = false;
    for target in &matching {
        if !target.detect_path.exists() {
            eprintln!(
                "    {}: not installed ({})",
                target.name,
                target.detect_path.display()
            );
            continue;
        }
        match editor_registry::write_config_with_options(
            target,
            &binary,
            WriteOptions {
                overwrite_invalid: true,
            },
        ) {
            Ok(result) => {
                let action = match result.action {
                    editor_registry::WriteAction::Created => "created",
                    editor_registry::WriteAction::Updated => "updated",
                    editor_registry::WriteAction::Already => "already configured",
                };
                eprintln!("    {}: {action}", target.name);
                mcp_ok = true;
            }
            Err(e) => eprintln!("    {}: error: {e}", target.name),
        }
    }

    if !mcp_ok {
        eprintln!();
        eprintln!(
            "No installed instance of '{agent_key}' found. \
             Install {agent_key}, then re-run: lean-ctx wrap {agent_key}"
        );
        std::process::exit(1);
    }

    // --- Step 4: Agent hooks ---
    let mode = hooks::recommend_hook_mode(agent_key);
    eprintln!(
        "  Installing agent hooks ({})...",
        match mode {
            HookMode::Mcp => "MCP",
            HookMode::Hybrid => "Hybrid",
            HookMode::Replace => "Replace",
        }
    );
    hooks::install_agent_hook_respecting_rules_off(agent_key, true, mode);

    // Re-render managed rule files on every wrap so [solution] changes add or
    // remove the canonical solution block instead of leaving stale guidance.
    for status in crate::rules_inject::collect_rules_status(&home) {
        snap.record_file(std::path::Path::new(&status.path));
    }
    let rules = crate::rules_inject::inject_rules_for_agent(&home, agent_key);
    for error in rules.errors {
        eprintln!("  Rules: {error}");
    }

    // --- Step 4b: Consolidate split data dirs ---
    if let Some(report) = crate::core::data_consolidate::consolidate() {
        if report.files_moved > 0 {
            eprintln!(
                "  Consolidated {} files from {} split data dir(s)",
                report.files_moved,
                report.merged_from.len()
            );
        }
    }

    // --- Step 5: Daemon ---
    eprintln!("  Starting daemon...");
    if !crate::daemon::is_daemon_running() {
        let _ = crate::daemon::start_daemon(&[]);
    }

    // --- Step 6: Save snapshot for unwrap ---
    if let Err(e) = snap.save() {
        eprintln!("  Warning: could not save wrap snapshot: {e}");
    }

    // --- Step 7: Verify MCP ---
    let mcp_verified = verify::probe_mcp_server(&binary);

    // --- Step 8: Launch / restart hint ---
    let launch_result = launch::handle_agent_launch(agent_key);

    // --- Step 9: Self-test ---
    run_self_test(agent_key, &home);

    // --- Step 10: Summary ---
    print_summary(agent_key, mcp_verified, &launch_result);
}

fn run_self_test(agent_key: &str, home: &std::path::Path) {
    let mut issues: Vec<String> = Vec::new();

    // 1. Hook coverage
    if !crate::core::rules_channel::client_hook_covered(agent_key, home) {
        issues.push(format!(
            "Hook coverage not detected for {agent_key}.              Native tool calls may be visible in chat.              Re-run: lean-ctx wrap {agent_key}"
        ));
    }

    // 2. Stats split
    let dirs = crate::core::data_dir::all_data_dirs_with_stats();
    if dirs.len() > 1 {
        issues.push(format!(
            "Stats data found in {} locations (should be 1).              Run: lean-ctx doctor --fix",
            dirs.len()
        ));
    }

    // 3. Daemon running
    if !crate::daemon::is_daemon_running() {
        issues.push(
            "Daemon is not running. Context caching and live stats              will be unavailable until it starts."
                .to_string(),
        );
    }

    if issues.is_empty() {
        eprintln!("  [32mSelf-test: all checks passed[0m");
    } else {
        eprintln!();
        eprintln!("  [33mSelf-test: {} issue(s) detected:[0m", issues.len());
        for issue in &issues {
            eprintln!("    [33m- {issue}[0m");
        }
    }
}

fn print_summary(agent_key: &str, mcp_ok: bool, launch_hint: &str) {
    let tool_count = crate::server::registry::tool_count();

    eprintln!();
    eprintln!("\x1b[1;32mlean-ctx wrapped {agent_key} successfully.\x1b[0m");
    eprintln!();

    let mcp_status = if mcp_ok {
        format!(
            "\x1b[32m{tool_count} tools verified\x1b[0m (ctx_read, ctx_search, ctx_shell + more)"
        )
    } else {
        format!("{tool_count} tools \x1b[33m(pending IDE restart)\x1b[0m")
    };
    eprintln!("  MCP server:  {mcp_status}");
    eprintln!("  Shell hooks: \x1b[32minstalled\x1b[0m (git, cargo, npm, docker + 90 patterns)");
    eprintln!("  Agent hooks: \x1b[32minstalled\x1b[0m");
    eprintln!();

    if !launch_hint.is_empty() {
        eprintln!("  \x1b[33m{launch_hint}\x1b[0m");
        eprintln!();
    }

    eprintln!("  Undo:   \x1b[2mlean-ctx unwrap {agent_key}\x1b[0m");
    eprintln!("  Verify: \x1b[2mlean-ctx doctor\x1b[0m");
    eprintln!("  Stats:  \x1b[2mlean-ctx gain\x1b[0m (after first use)");
}

fn print_help() {
    println!("Usage: lean-ctx wrap <agent>");
    println!();
    println!("One-command setup: installs shell hooks, MCP server registration,");
    println!("agent hooks, and starts the daemon. Everything needed to use lean-ctx");
    println!("with the specified agent.");
    println!();
    println!("Supported agents:");
    for name in available_agent_keys() {
        println!("  {name}");
    }
    println!();
    println!("Examples:");
    println!("  lean-ctx wrap cursor     # Set up lean-ctx for Cursor");
    println!("  lean-ctx wrap claude     # Set up lean-ctx for Claude Code");
    println!("  lean-ctx wrap codex      # Set up lean-ctx for Codex CLI");
    println!();
    println!("Undo:  lean-ctx unwrap <agent>");
    println!("Full:  lean-ctx setup  (interactive wizard with all options)");
}

fn available_agent_keys() -> Vec<String> {
    let home = dirs::home_dir().unwrap_or_default();
    let targets = editor_registry::build_targets(&home);
    let mut keys: Vec<String> = targets.into_iter().map(|t| t.agent_key).collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

fn detect_single_agent() -> Option<String> {
    let home = dirs::home_dir()?;
    let targets = editor_registry::build_targets(&home);
    let installed: Vec<String> = targets
        .iter()
        .filter(|t| t.detect_path.exists())
        .map(|t| t.agent_key.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    if installed.len() == 1 {
        installed.into_iter().next()
    } else {
        None
    }
}
