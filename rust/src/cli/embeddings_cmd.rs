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
    match crate::core::ort_environment::check_ort_runtime() {
        Ok(path) => {
            println!("ONNX Runtime: {}", path.display());
            println!(
                "{}",
                crate::core::ort_execution_providers::execution_provider_status()
            );
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}

#[cfg(not(feature = "embeddings"))]
fn status() {
    eprintln!("This lean-ctx build has no semantic embeddings (built without `embeddings`).");
    std::process::exit(1);
}
