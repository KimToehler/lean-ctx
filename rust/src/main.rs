/// Tuning for the bundled jemalloc (#1899).
///
/// tikv-jemalloc-sys builds jemalloc with the `_rjem_` symbol prefix, so it
/// reads `_rjem_malloc_conf` — never plain `malloc_conf`. The old plain export
/// was ignored by our allocator on every platform, but FreeBSD's libc malloc
/// (itself jemalloc) did read it and rejected `background_thread`.
///
/// It lives in the binary crate on purpose: jemalloc ships a weak default, so
/// a definition inside the rlib is never pulled in by the linker.
/// `background_thread` is Linux-only; elsewhere jemalloc prints
/// "option background_thread currently supports pthread only".
#[cfg(all(feature = "jemalloc", not(windows), not(target_env = "musl")))]
#[allow(non_upper_case_globals)]
#[used]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static malloc_conf: &[u8] = lean_ctx::JEMALLOC_CONF;

fn main() {
    // #356: before anything touches the filesystem, a launchd-standalone
    // process (daemon/proxy/auto-updater booted from a stale, pre-seatbelt
    // plist — e.g. a brew-only upgrade) re-execs itself under the
    // deny-~/Documents seatbelt. No-op for terminal/editor children (they
    // inherit the host TCC grant). macOS-only: TCC and `sandbox-exec` are
    // macOS features, so the guard module isn't built on other platforms.
    #[cfg(target_os = "macos")]
    lean_ctx::core::tcc_guard_sandbox::reexec_under_seatbelt_if_needed();

    // Crash log + stderr message for every panic in any thread (#378
    // diagnosability: stderr is lost for daemon/LaunchAgent processes,
    // ~/.lean-ctx/logs/crash.log is not).
    lean_ctx::core::crash_log::install_panic_hook();

    // Prevent SIGABRT on uncaught panics (e.g. during MCP startup bursts).
    // The panic hook above still prints details; we just exit cleanly.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        lean_ctx::cli::dispatch::run();
    }));
    if res.is_err() {
        std::process::exit(1);
    }
}

#[cfg(all(test, feature = "jemalloc", not(windows), not(target_env = "musl")))]
mod tests {
    /// #1899: proves the linker kept our export AND jemalloc parsed it —
    /// jemalloc's built-in default for this option is 10 000 ms.
    #[test]
    fn jemalloc_applies_exported_conf() {
        // SAFETY: static NUL-terminated ctl name; `opt.dirty_decay_ms` is an
        // `ssize_t`, which `isize` matches on every supported target.
        let decay: isize =
            unsafe { tikv_jemalloc_ctl::raw::read(b"opt.dirty_decay_ms\0") }.unwrap();
        assert_eq!(decay, 1000);
    }
}
