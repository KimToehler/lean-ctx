//! `lean-ctx embeddings` — ONNX Runtime status for semantic embeddings.

pub(crate) fn cmd_embeddings(rest: &[String]) {
    match rest.first().map(String::as_str) {
        None | Some("status") => status(),
        Some(other) => {
            eprintln!("Unknown embeddings subcommand: {other}");
            eprintln!("Usage: lean-ctx embeddings status");
            eprintln!(
                "lean-ctx no longer installs ONNX Runtime itself; set ORT_DYLIB_PATH instead."
            );
            std::process::exit(2);
        }
    }
}

#[cfg(feature = "embeddings")]
fn status() {
    let source = ort_dylib_path_source(std::env::var_os("ORT_DYLIB_PATH").as_deref());
    match crate::core::ort_environment::check_ort_runtime() {
        Ok(path) => {
            println!("ONNX Runtime: {}", path.display());
            println!("{source}");
            println!(
                "{}",
                crate::core::ort_execution_providers::execution_provider_status()
            );
            if let Some(gpu) = crate::core::ort_execution_providers::gpu_runtime_status() {
                println!("{gpu}");
            }
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

/// Where the runtime path came from. The MCP config `env` block is invisible
/// here, so say explicitly when the variable is unset in this process.
#[cfg(any(feature = "embeddings", test))]
fn ort_dylib_path_source(value: Option<&std::ffi::OsStr>) -> String {
    match value {
        Some(v) => format!(
            "ORT_DYLIB_PATH: {} (set in this process)",
            v.to_string_lossy()
        ),
        None => "ORT_DYLIB_PATH: not set in this process — found by automatic search \
                 (an MCP config \"env\" block applies only to the MCP server, not to this terminal)"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ort_dylib_path_source_reports_scope() {
        let set = ort_dylib_path_source(Some(std::ffi::OsStr::new("C:\\ort\\onnxruntime.dll")));
        assert!(set.contains("C:\\ort\\onnxruntime.dll"));
        assert!(set.contains("set in this process"));
        let unset = ort_dylib_path_source(None);
        assert!(unset.contains("not set in this process"));
        assert!(unset.contains("MCP config"));
    }
}

#[cfg(not(feature = "embeddings"))]
fn status() {
    eprintln!("This lean-ctx build has no semantic embeddings (built without `embeddings`).");
    std::process::exit(1);
}
