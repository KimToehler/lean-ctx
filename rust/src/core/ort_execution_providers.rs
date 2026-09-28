#![allow(dead_code)]
//! ONNX Runtime execution provider selection: CPU default, opt-in GPU providers.
//!
//! Each GPU EP is gated behind its own Cargo feature (`ort-cuda`, `ort-rocm`, etc.).
//! `LEAN_CTX_ORT_EXECUTION_PROVIDER=cpu|gpu|auto` controls runtime selection.
//! By default, `auto` enables GPU only when the selected ORT dylib looks like a
//! GPU runtime; otherwise CPU is used. ORT falls back to CPU when a registered
//! GPU EP is unusable.

use std::path::Path;

const PROVIDER_ENV: &str = "LEAN_CTX_ORT_EXECUTION_PROVIDER";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderPolicy {
    Cpu,
    Gpu,
    Auto,
}

/// Build the execution provider list for the current runtime policy.
pub(crate) fn execution_providers() -> Vec<ort::ep::ExecutionProviderDispatch> {
    match provider_policy() {
        ProviderPolicy::Cpu => cpu_execution_providers(),
        ProviderPolicy::Gpu => gpu_execution_providers(),
        ProviderPolicy::Auto => {
            if selected_runtime_looks_gpu() {
                gpu_execution_providers()
            } else {
                tracing::debug!(
                    env = PROVIDER_ENV,
                    "ONNX Runtime GPU auto-detect did not find a GPU runtime; using CPU"
                );
                cpu_execution_providers()
            }
        }
    }
}

pub(crate) fn execution_provider_status() -> String {
    let policy = provider_policy_name();
    let compiled = compiled_gpu_provider_names();
    let compiled = if compiled.is_empty() {
        "none".to_string()
    } else {
        compiled.join(",")
    };
    format!(
        "ORT execution provider policy: {policy} (env {PROVIDER_ENV}; compiled GPU EPs: {compiled})"
    )
}

/// GPU readiness for `lean-ctx embeddings status`: whether the CUDA provider
/// and its CUDA/cuDNN libraries load (CUDA builds), or a hint when a GPU
/// runtime is selected but this binary is CPU-only.
pub(crate) fn gpu_runtime_status() -> Option<String> {
    #[cfg(feature = "ort-cuda")]
    {
        Some(match probe_cuda_runtime() {
            Ok(()) => "CUDA runtime: OK (CUDA provider, CUDA 12 and cuDNN 9 load)".to_string(),
            Err(e) => format!("CUDA runtime: not loadable\n{}", cuda_missing_message(&e)),
        })
    }
    #[cfg(not(feature = "ort-cuda"))]
    {
        let supported = matches!(
            (std::env::consts::OS, std::env::consts::ARCH),
            ("linux" | "windows", "x86_64")
        );
        (supported && selected_runtime_looks_gpu()).then(|| {
            "GPU: the selected ONNX Runtime has CUDA support, but this lean-ctx binary is CPU-only \
             — run `lean-ctx enable-gpu` to install the CUDA build."
                .to_string()
        })
    }
}

/// Whether the current policy resolves to a GPU execution provider — regardless
/// of whether that provider's runtime dependencies can actually be loaded.
fn policy_wants_gpu() -> bool {
    if compiled_gpu_provider_names().is_empty() {
        return false;
    }
    match provider_policy() {
        ProviderPolicy::Cpu => false,
        ProviderPolicy::Gpu => true,
        ProviderPolicy::Auto => selected_runtime_looks_gpu(),
    }
}

/// Whether a real GPU execution provider will *actually* run inference — i.e.
/// the policy wants a GPU **and** the provider's runtime libraries load. Used to
/// scale batch size: small mini-batches under-utilize a GPU and pay
/// kernel-launch/host↔device-copy overhead per call that isn't amortized
/// (notably under WSL2 GPU passthrough), but oversizing batches for a GPU that
/// silently fell back to CPU makes the CPU path dramatically slower — so this
/// must reflect the EP that ORT will really register, not just the policy.
pub(crate) fn gpu_active() -> bool {
    if !policy_wants_gpu() {
        return false;
    }
    // The shipped Linux/Windows GPU build compiles only the CUDA EP. If its
    // runtime deps (libcudart/libcublas/libcudnn/…) can't be dlopen'd, ORT
    // silently registers CPU instead; don't size batches for a phantom GPU.
    #[cfg(feature = "ort-cuda")]
    {
        cuda_runtime_available()
    }
    #[cfg(not(feature = "ort-cuda"))]
    {
        true
    }
}

/// When the policy expects a GPU but the CUDA runtime can't be loaded (so ORT
/// falls back to CPU), returns a user-facing explanation with the exact install
/// commands for the missing libraries. Returns `None` when the GPU actually
/// works or when CPU was requested.
pub(crate) fn gpu_fallback_warning() -> Option<String> {
    #[cfg(feature = "ort-cuda")]
    {
        if !policy_wants_gpu() || cuda_runtime_available() {
            return None;
        }
        let detail = probe_cuda_runtime().err().unwrap_or_default();
        Some(cuda_missing_message(&detail))
    }
    #[cfg(not(feature = "ort-cuda"))]
    {
        None
    }
}

/// Filename of the ORT CUDA provider shared library for the current platform.
#[cfg(feature = "ort-cuda")]
fn cuda_provider_lib_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "onnxruntime_providers_cuda.dll"
    }
    #[cfg(target_os = "macos")]
    {
        "libonnxruntime_providers_cuda.dylib"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "libonnxruntime_providers_cuda.so"
    }
}

/// Path to the ORT CUDA provider library, resolved next to the selected ORT dylib.
#[cfg(feature = "ort-cuda")]
fn cuda_provider_lib_path() -> Option<std::path::PathBuf> {
    let dylib = crate::core::ort_environment::resolved_ort_dylib_path().ok()?;
    Some(dylib.parent()?.join(cuda_provider_lib_name()))
}

/// Probe whether the CUDA provider library and its transitive CUDA runtime
/// dependencies can be loaded — the same resolution ORT performs when it
/// registers the EP. When the first attempt fails, the CUDA 12 / cuDNN 9
/// libraries are preloaded from the pip `nvidia-*` wheels
/// (`onnxruntime-gpu[cuda,cudnn]`) and, on Windows, the CUDA Toolkit and
/// cuDNN install dirs; the loader then reuses those modules by name. `Err`
/// carries the loader message plus, where they can be determined, the missing
/// libraries.
#[cfg(feature = "ort-cuda")]
fn probe_cuda_runtime() -> Result<(), String> {
    let dylib = crate::core::ort_environment::resolved_ort_dylib_path()
        .map_err(|e| format!("ONNX Runtime not found: {e}"))?;
    let path = cuda_provider_lib_path()
        .ok_or_else(|| "could not resolve the ORT CUDA provider library path".to_string())?;
    if !path.exists() {
        return Err(format!(
            "{} not found — {} is a CPU-only ONNX Runtime; install onnxruntime-gpu",
            path.display(),
            dylib.display()
        ));
    }
    let first = match load_cuda_provider(&path) {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };

    let windows = cfg!(target_os = "windows");
    let dirs = cuda_candidate_dirs(
        &dylib,
        &crate::core::ort_environment::python_site_packages_dirs(),
        &cuda_toolkit_roots(std::env::vars_os()),
        std::env::var_os("ProgramFiles")
            .map(std::path::PathBuf::from)
            .as_deref(),
        windows,
    );
    let preloaded = preload_cuda_libs(&dirs, windows);
    if preloaded > 0 && load_cuda_provider(&path).is_ok() {
        tracing::info!("CUDA provider loaded after preloading {preloaded} CUDA/cuDNN libraries");
        return Ok(());
    }

    // dlopen on Linux already names the first missing `.so`; the Windows loader
    // only says "os error 126", so list what is missing from the search path.
    if !windows {
        return Err(first);
    }
    let mut search = dirs;
    if let Some(paths) = std::env::var_os("PATH") {
        search.extend(std::env::split_paths(&paths));
    }
    let missing = missing_cuda_libs(&search, windows);
    if missing.is_empty() {
        Err(first)
    } else {
        Err(format!("{first} (not found: {})", missing.join(", ")))
    }
}

/// Load the ORT CUDA provider the way ONNX Runtime does. On Windows,
/// `LOAD_WITH_ALTERED_SEARCH_PATH` lets it resolve its siblings
/// (`onnxruntime_providers_shared.dll`) from its own directory.
#[cfg(feature = "ort-cuda")]
fn load_cuda_provider(path: &Path) -> Result<(), String> {
    // SAFETY: loading the ORT CUDA provider shared library, exactly as ONNX
    // Runtime itself does when registering the CUDA EP. We drop it immediately;
    // this only checks that its runtime dependencies resolve.
    #[cfg(target_os = "windows")]
    let loaded = unsafe {
        libloading::os::windows::Library::load_with_flags(
            path,
            libloading::os::windows::LOAD_WITH_ALTERED_SEARCH_PATH,
        )
    }
    .map(|_lib| ());
    #[cfg(not(target_os = "windows"))]
    // SAFETY: as above — the same provider load ORT performs.
    let loaded = unsafe { libloading::Library::new(path) }.map(|_lib| ());

    loaded.or_else(|e| {
        let err = e.to_string();
        // ORT provider plugins are normally loaded by libonnxruntime itself.
        // A direct dlopen may fail on ORT host symbols after CUDA/cuDNN deps
        // have resolved; that is still enough for this dependency probe.
        if err.contains("Provider_GetHost") {
            Ok(())
        } else {
            Err(err)
        }
    })
}

/// Load every CUDA/cuDNN library found in `dirs`, dependencies first, and keep
/// it loaded for the process lifetime. Returns how many were loaded.
#[cfg(feature = "ort-cuda")]
fn preload_cuda_libs(dirs: &[std::path::PathBuf], windows: bool) -> usize {
    let mut loaded = 0;
    for prefix in cuda_lib_prefixes(windows) {
        let Some(lib) = find_cuda_lib(dirs, prefix, windows) else {
            continue;
        };
        // SAFETY: loading an NVIDIA runtime library that ORT's CUDA provider
        // would load itself; same trust boundary as ort::init_from.
        #[cfg(target_os = "windows")]
        let result = unsafe {
            libloading::os::windows::Library::load_with_flags(
                &lib,
                libloading::os::windows::LOAD_WITH_ALTERED_SEARCH_PATH,
            )
        }
        .map(libloading::Library::from);
        #[cfg(not(target_os = "windows"))]
        // SAFETY: as above — an NVIDIA library the CUDA provider loads itself.
        let result = unsafe { libloading::Library::new(&lib) };
        match result {
            Ok(handle) => {
                // Intentionally leaked: the provider must find it by name later.
                std::mem::forget(handle);
                loaded += 1;
                tracing::debug!("Preloaded {}", lib.display());
            }
            Err(e) => tracing::debug!("Could not preload {}: {e}", lib.display()),
        }
    }
    loaded
}

/// CUDA 12 / cuDNN 9 libraries needed by ONNX Runtime's CUDA provider, in
/// dependency order (matches `ort::ep::cuda::{CUDA,CUDNN}_DYLIBS`, plus
/// curand/nvrtc which the provider also imports).
fn cuda_lib_prefixes(windows: bool) -> &'static [&'static str] {
    if windows {
        &[
            "cudart64_12",
            "cublasLt64_12",
            "cublas64_12",
            "cufft64_11",
            "curand64_10",
            "nvrtc64_12",
            "cudnn64_9",
            "cudnn_graph64_9",
            "cudnn_ops64_9",
            "cudnn_heuristic64_9",
            "cudnn_adv64_9",
            "cudnn_cnn64_9",
            "cudnn_engines_precompiled64_9",
            "cudnn_engines_runtime_compiled64_9",
        ]
    } else {
        &[
            "libcudart.so.12",
            "libcublasLt.so.12",
            "libcublas.so.12",
            "libcufft.so.11",
            "libcurand.so.10",
            "libnvrtc.so.12",
            "libcudnn.so.9",
            "libcudnn_graph.so.9",
            "libcudnn_ops.so.9",
            "libcudnn_heuristic.so.9",
            "libcudnn_adv.so.9",
            "libcudnn_cnn.so.9",
            "libcudnn_engines_precompiled.so.9",
            "libcudnn_engines_runtime_compiled.so.9",
        ]
    }
}

/// Whether `file` is the library named by `prefix` (`cudart64_12.dll`,
/// `nvrtc64_120_0.dll`, `libcudart.so.12`, `libcudart.so.12.8.90`).
fn is_cuda_lib(file: &str, prefix: &str, windows: bool) -> bool {
    if windows {
        let file = file.to_ascii_lowercase();
        let prefix = prefix.to_ascii_lowercase();
        file.strip_prefix(&prefix)
            .and_then(|rest| rest.strip_suffix(".dll"))
            .is_some_and(|mid| {
                mid.is_empty()
                    || mid.starts_with('_')
                    || mid.starts_with(|c: char| c.is_ascii_digit())
            })
    } else {
        file.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    }
}

/// First library matching `prefix` in `dirs` (in order).
fn find_cuda_lib(
    dirs: &[std::path::PathBuf],
    prefix: &str,
    windows: bool,
) -> Option<std::path::PathBuf> {
    dirs.iter().find_map(|dir| {
        let mut hits: Vec<_> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|f| is_cuda_lib(f, prefix, windows))
            })
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        hits.sort();
        hits.into_iter().next()
    })
}

/// Required libraries not present in any of `dirs`, as user-facing names.
fn missing_cuda_libs(dirs: &[std::path::PathBuf], windows: bool) -> Vec<String> {
    cuda_lib_prefixes(windows)
        .iter()
        .filter(|prefix| find_cuda_lib(dirs, prefix, windows).is_none())
        .map(|prefix| {
            if windows {
                format!("{prefix}*.dll")
            } else {
                (*prefix).to_string()
            }
        })
        .collect()
}

/// CUDA Toolkit roots from `CUDA_PATH` and the versioned `CUDA_PATH_V12_*`
/// variables the Windows installer sets (CUDA 12 first; the provider needs it).
fn cuda_toolkit_roots(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<std::path::PathBuf> {
    let mut versioned = Vec::new();
    let mut default = None;
    for (key, value) in vars {
        let Some(key) = key.to_str() else { continue };
        if value.is_empty() {
            continue;
        }
        let key = key.to_ascii_uppercase();
        if key.starts_with("CUDA_PATH_V12") {
            versioned.push((key, std::path::PathBuf::from(value)));
        } else if key == "CUDA_PATH" {
            default = Some(std::path::PathBuf::from(value));
        }
    }
    // Newest CUDA 12 minor first.
    versioned.sort_by(|a, b| b.0.cmp(&a.0));
    let mut roots: Vec<_> = versioned.into_iter().map(|(_, p)| p).collect();
    if let Some(default) = default
        && !roots.contains(&default)
    {
        roots.push(default);
    }
    roots
}

/// Directories that may hold the CUDA/cuDNN libraries, most specific first:
/// next to the ORT runtime, the pip `nvidia-*` wheels (the runtime's own
/// site-packages, then every other known one), and on Windows the CUDA
/// Toolkit `bin` dirs and `%ProgramFiles%\NVIDIA\CUDNN\v9.*\bin[\12.*]`.
fn cuda_candidate_dirs(
    ort_dylib: &Path,
    site_packages: &[std::path::PathBuf],
    cuda_roots: &[std::path::PathBuf],
    program_files: Option<&Path>,
    windows: bool,
) -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    let mut push = |dir: std::path::PathBuf| {
        if dir.is_dir() && !dirs.contains(&dir) {
            dirs.push(dir);
        }
    };
    let runtime_dir = ort_dylib.parent();
    if let Some(dir) = runtime_dir {
        push(dir.to_path_buf());
    }
    // <site-packages>/onnxruntime/capi/onnxruntime.dll → <site-packages>
    let own_site = runtime_dir
        .filter(|d| d.file_name().is_some_and(|n| n == "capi"))
        .and_then(Path::parent)
        .filter(|d| d.file_name().is_some_and(|n| n == "onnxruntime"))
        .and_then(Path::parent);
    let lib_dir = if windows { "bin" } else { "lib" };
    for site in own_site
        .into_iter()
        .chain(site_packages.iter().map(std::path::PathBuf::as_path))
    {
        for dir in sorted_subdirs(&site.join("nvidia")) {
            push(dir.join(lib_dir));
        }
    }
    if windows {
        for root in cuda_roots {
            push(root.join("bin"));
            push(root.join("bin").join("x64"));
        }
        if let Some(pf) = program_files {
            // cuDNN 9 installer: CUDNN\v9.x\bin\12.x (newest version first).
            for version in sorted_subdirs(&pf.join("NVIDIA").join("CUDNN"))
                .into_iter()
                .rev()
            {
                if !version
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("v9"))
                {
                    continue;
                }
                let bin = version.join("bin");
                for cuda in sorted_subdirs(&bin).into_iter().rev() {
                    if cuda
                        .file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("12"))
                    {
                        push(cuda);
                    }
                }
                push(bin);
            }
        }
    }
    dirs
}

fn sorted_subdirs(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out: Vec<_> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// Cached result of [`probe_cuda_runtime`]; the dlopen runs at most once.
#[cfg(feature = "ort-cuda")]
fn cuda_runtime_available() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| probe_cuda_runtime().is_ok())
}

/// User-facing message shown when the CUDA runtime is missing, including the
/// exact commands to install the required libraries.
#[cfg(feature = "ort-cuda")]
fn cuda_missing_message(probe_err: &str) -> String {
    cuda_missing_message_for(cfg!(target_os = "windows"), probe_err)
}

fn cuda_missing_message_for(windows: bool, probe_err: &str) -> String {
    let head = format!(
        "GPU requested (via {env}) but the CUDA runtime libraries required by ONNX Runtime \
         could not be loaded — embedding is running on CPU. Loader error: {probe_err}\n\
         ONNX Runtime 1.{ort_minor}.x needs CUDA 12 + cuDNN 9 and a current NVIDIA driver.\n",
        env = PROVIDER_ENV,
        ort_minor = ort::MINOR_VERSION,
    );
    if windows {
        return format!(
            "{head}Easiest — in the same Python that provides onnxruntime.dll:\n  \
             pip install \"onnxruntime-gpu[cuda,cudnn]\"\n  \
             lean-ctx then preloads site-packages\\nvidia\\*\\bin automatically (no PATH changes).\n\
             Or install the CUDA Toolkit 12.x plus cuDNN 9 for CUDA 12 (found via CUDA_PATH and \
             C:\\Program Files\\NVIDIA\\CUDNN\\v9.x\\bin\\12.x). A CUDA 13 toolkit alone does not \
             provide the *64_12.dll files ONNX Runtime needs.\n\
             Check with: lean-ctx embeddings status\n\
             To silence this and stay on CPU, set {PROVIDER_ENV}=cpu."
        );
    }
    format!(
        "{head}Easiest — in the same Python that provides libonnxruntime:\n  \
         pip install \"onnxruntime-gpu[cuda,cudnn]\"\n  \
         lean-ctx then preloads site-packages/nvidia/*/lib automatically.\n\
         Or install them system-wide on Ubuntu / WSL2:\n  \
         wget -O /tmp/cuda-keyring_1.1-1_all.deb https://developer.download.nvidia.com/compute/cuda/repos/wsl-ubuntu/x86_64/cuda-keyring_1.1-1_all.deb\n  \
         sudo dpkg -i /tmp/cuda-keyring_1.1-1_all.deb && rm -f /tmp/cuda-keyring_1.1-1_all.deb && sudo apt-get update\n  \
         sudo apt-get install -y cuda-cudart-12-8 libcublas-12-8 libcurand-12-8 libcufft-12-8\n  \
         python3 -m venv $HOME/.local/share/lean-ctx/cuda-libs\n  \
         $HOME/.local/share/lean-ctx/cuda-libs/bin/python -m pip install nvidia-cudnn-cu12==9.8.0.87\n  \
         # then ensure the loader can find them (if not already on the path):\n  \
         export LD_LIBRARY_PATH=$($HOME/.local/share/lean-ctx/cuda-libs/bin/python -c 'import pathlib, nvidia.cudnn; print(pathlib.Path(nvidia.cudnn.__file__).parent / '\''lib'\'')'):/usr/local/cuda-12.8/targets/x86_64-linux/lib:/usr/lib/x86_64-linux-gnu:$LD_LIBRARY_PATH\n\
         To silence this and stay on CPU, set {PROVIDER_ENV}=cpu."
    )
}

pub(crate) fn execution_provider_help() -> &'static str {
    "By default lean-ctx auto-detects GPU runtimes from ORT_DYLIB_PATH and otherwise uses CPU. Set LEAN_CTX_ORT_EXECUTION_PROVIDER=cpu|gpu|auto to override."
}

fn cpu_execution_providers() -> Vec<ort::ep::ExecutionProviderDispatch> {
    vec![ort::ep::CPU::default().build()]
}

/// Build the list of GPU execution providers in registration-priority order.
pub(crate) fn gpu_execution_providers() -> Vec<ort::ep::ExecutionProviderDispatch> {
    #[allow(unused_mut)]
    let mut eps: Vec<ort::ep::ExecutionProviderDispatch> = Vec::new();
    let compiled_gpu_count = compiled_gpu_provider_names().len();

    #[cfg(feature = "ort-cuda")]
    {
        // Runs the probe (and CUDA/cuDNN preload) before ORT loads the provider.
        if cuda_runtime_available() {
            tracing::info!("Enabling CUDA execution provider for ONNX Runtime");
        } else {
            tracing::debug!("CUDA runtime not loadable; ONNX Runtime will fall back to CPU");
        }
        eps.push(ort::ep::CUDA::default().build());
    }

    #[cfg(feature = "ort-rocm")]
    {
        tracing::info!("Enabling ROCm execution provider for ONNX Runtime");
        eps.push(ort::ep::ROCm::default().build());
    }

    #[cfg(feature = "ort-webgpu")]
    {
        tracing::info!("Enabling WebGPU execution provider for ONNX Runtime");
        eps.push(ort::ep::WebGPU::default().build());
    }
    #[cfg(all(target_os = "windows", feature = "ort-directml"))]
    {
        tracing::info!("Enabling DirectML execution provider for ONNX Runtime");
        eps.push(ort::ep::DirectML::default().build());
    }

    #[cfg(all(any(target_os = "macos", target_os = "ios"), feature = "ort-coreml"))]
    {
        tracing::info!("Enabling CoreML execution provider for ONNX Runtime");
        eps.push(ort::ep::CoreML::default().build());
    }

    if compiled_gpu_count == 0 {
        tracing::warn!(
            "GPU execution provider requested, but this lean-ctx binary was built without ort-cuda/ort-rocm/etc.; using CPU only"
        );
    } else if eps.is_empty() {
        tracing::debug!("No GPU execution providers configured — using CPU only");
    }

    eps.push(ort::ep::CPU::default().build());
    eps
}

fn provider_policy() -> ProviderPolicy {
    match std::env::var(PROVIDER_ENV) {
        Ok(value) => provider_policy_from_value(&value),
        Err(_) => ProviderPolicy::Auto,
    }
}

fn provider_policy_name() -> &'static str {
    match provider_policy() {
        ProviderPolicy::Cpu => "cpu",
        ProviderPolicy::Gpu => "gpu",
        ProviderPolicy::Auto => "auto",
    }
}

fn provider_policy_from_value(value: &str) -> ProviderPolicy {
    match value.trim().to_lowercase().as_str() {
        "gpu" | "cuda" | "rocm" | "webgpu" | "directml" | "coreml" => ProviderPolicy::Gpu,
        "auto" => ProviderPolicy::Auto,
        _ => ProviderPolicy::Cpu,
    }
}

fn selected_runtime_looks_gpu() -> bool {
    crate::core::ort_environment::resolved_ort_dylib_path()
        .ok()
        .as_deref()
        .is_some_and(runtime_path_looks_gpu)
}

fn runtime_path_looks_gpu(path: &Path) -> bool {
    let path_text = path.to_string_lossy().to_lowercase();
    if path_text.contains("gpu") || path_text.contains("cuda") || path_text.contains("rocm") {
        return true;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    [
        "libonnxruntime_providers_cuda.so",
        "libonnxruntime_providers_rocm.so",
        "onnxruntime_providers_cuda.dll",
        "onnxruntime_providers_rocm.dll",
        "libonnxruntime_providers_cuda.dylib",
        "libonnxruntime_providers_rocm.dylib",
    ]
    .iter()
    .any(|name| parent.join(name).exists())
}

fn compiled_gpu_provider_names() -> Vec<&'static str> {
    let mut names = vec![
        #[cfg(feature = "ort-cuda")]
        "cuda",
        #[cfg(feature = "ort-rocm")]
        "rocm",
        #[cfg(feature = "ort-webgpu")]
        "webgpu",
        #[cfg(all(target_os = "windows", feature = "ort-directml"))]
        "directml",
        #[cfg(all(any(target_os = "macos", target_os = "ios"), feature = "ort-coreml"))]
        "coreml",
    ];
    let _ = &mut names; // suppress unused_mut when no GPU feature is active
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_policy_defaults_to_cpu_for_unknown_values() {
        assert_eq!(provider_policy_from_value(""), ProviderPolicy::Cpu);
        assert_eq!(provider_policy_from_value("bogus"), ProviderPolicy::Cpu);
        assert_eq!(provider_policy_from_value("cpu"), ProviderPolicy::Cpu);
    }

    #[test]
    fn provider_policy_accepts_gpu_and_auto_aliases() {
        assert_eq!(provider_policy_from_value("gpu"), ProviderPolicy::Gpu);
        assert_eq!(provider_policy_from_value("CUDA"), ProviderPolicy::Gpu);
        assert_eq!(provider_policy_from_value("auto"), ProviderPolicy::Auto);
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    #[test]
    fn cuda_lib_names_match_real_file_names() {
        for (file, prefix) in [
            ("cudart64_12.dll", "cudart64_12"),
            ("CUDART64_12.DLL", "cudart64_12"),
            ("nvrtc64_120_0.dll", "nvrtc64_12"),
            ("cublasLt64_12.dll", "cublasLt64_12"),
            ("cudnn_ops64_9.dll", "cudnn_ops64_9"),
        ] {
            assert!(is_cuda_lib(file, prefix, true), "{file}");
        }
        assert!(!is_cuda_lib("cublasLt64_12.dll", "cublas64_12", true));
        assert!(!is_cuda_lib("cudart64_13.dll", "cudart64_12", true));
        assert!(!is_cuda_lib("cudnn64_9.lib", "cudnn64_9", true));

        assert!(is_cuda_lib("libcudart.so.12", "libcudart.so.12", false));
        assert!(is_cuda_lib(
            "libcudart.so.12.8.90",
            "libcudart.so.12",
            false
        ));
        assert!(!is_cuda_lib("libcudart.so.120", "libcudart.so.12", false));
        assert!(!is_cuda_lib("libcublasLt.so.12", "libcublas.so.12", false));
    }

    #[test]
    fn cuda_candidate_dirs_find_pip_nvidia_wheels_next_to_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("Lib").join("site-packages");
        let dylib = site
            .join("onnxruntime")
            .join("capi")
            .join("onnxruntime.dll");
        touch(&dylib);
        touch(&site.join("nvidia/cuda_runtime/bin/cudart64_12.dll"));
        touch(&site.join("nvidia/cudnn/bin/cudnn64_9.dll"));

        let dirs = cuda_candidate_dirs(&dylib, std::slice::from_ref(&site), &[], None, true);
        assert_eq!(dirs[0], dylib.parent().unwrap());
        assert!(dirs.contains(&site.join("nvidia/cuda_runtime/bin")));
        assert!(dirs.contains(&site.join("nvidia/cudnn/bin")));
        // The own site-packages is listed once even when passed again.
        assert_eq!(dirs.len(), 3);

        assert_eq!(
            find_cuda_lib(&dirs, "cudnn64_9", true),
            Some(site.join("nvidia/cudnn/bin/cudnn64_9.dll"))
        );
        let missing = missing_cuda_libs(&dirs, true);
        assert!(!missing.iter().any(|m| m.starts_with("cudart64_12")));
        assert!(missing.contains(&"cublas64_12*.dll".to_string()));
    }

    #[test]
    fn cuda_candidate_dirs_use_linux_lib_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("lib/python3.12/site-packages");
        let dylib = site.join("onnxruntime/capi/libonnxruntime.so.1.24.1");
        touch(&dylib);
        touch(&site.join("nvidia/cublas/lib/libcublas.so.12"));
        let dirs = cuda_candidate_dirs(&dylib, &[], &[], None, false);
        assert!(dirs.contains(&site.join("nvidia/cublas/lib")));
        assert!(find_cuda_lib(&dirs, "libcublas.so.12", false).is_some());
    }

    #[test]
    fn cuda_candidate_dirs_cover_windows_toolkit_and_cudnn_installer() {
        let tmp = tempfile::tempdir().unwrap();
        let runtime = tmp.path().join("ort/onnxruntime.dll");
        touch(&runtime);
        let toolkit = tmp.path().join("CUDA/v12.8");
        std::fs::create_dir_all(toolkit.join("bin")).unwrap();
        let pf = tmp.path().join("ProgramFiles");
        for dir in [
            "v9.1/bin/12.6",
            "v9.1/bin/13.0",
            "v9.8/bin/12.9",
            "v8.9/bin",
        ] {
            std::fs::create_dir_all(pf.join("NVIDIA/CUDNN").join(dir)).unwrap();
        }

        let dirs = cuda_candidate_dirs(
            &runtime,
            &[],
            std::slice::from_ref(&toolkit),
            Some(&pf),
            true,
        );
        let cudnn = pf.join("NVIDIA/CUDNN");
        assert!(dirs.contains(&toolkit.join("bin")));
        let newest = dirs
            .iter()
            .position(|d| *d == cudnn.join("v9.8/bin/12.9"))
            .unwrap();
        let older = dirs
            .iter()
            .position(|d| *d == cudnn.join("v9.1/bin/12.6"))
            .unwrap();
        assert!(newest < older, "newest cuDNN first: {dirs:?}");
        assert!(
            !dirs.contains(&cudnn.join("v9.1/bin/13.0")),
            "CUDA 13 build skipped"
        );
        assert!(
            !dirs.iter().any(|d| d.starts_with(cudnn.join("v8.9"))),
            "cuDNN 8 skipped"
        );

        // The Linux search never looks at Windows install locations.
        let linux = cuda_candidate_dirs(&runtime, &[], &[toolkit], Some(&pf), false);
        assert_eq!(linux, vec![runtime.parent().unwrap().to_path_buf()]);
    }

    #[test]
    fn cuda_toolkit_roots_prefer_cuda_12() {
        let vars = [
            ("CUDA_PATH", r"C:\CUDA\v13.0"),
            ("CUDA_PATH_V13_0", r"C:\CUDA\v13.0"),
            ("CUDA_PATH_V12_4", r"C:\CUDA\v12.4"),
            ("CUDA_PATH_V12_8", r"C:\CUDA\v12.8"),
            ("PATH", r"C:\Windows"),
        ]
        .map(|(k, v)| (k.into(), v.into()));
        let roots = cuda_toolkit_roots(vars.into_iter());
        let roots: Vec<_> = roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            roots,
            [r"C:\CUDA\v12.8", r"C:\CUDA\v12.4", r"C:\CUDA\v13.0"]
        );
    }

    #[test]
    fn cuda_missing_message_is_platform_specific() {
        let windows = cuda_missing_message_for(true, "os error 126 (not found: cudnn64_9*.dll)");
        assert!(windows.contains("cudnn64_9*.dll"));
        assert!(windows.contains("onnxruntime-gpu[cuda,cudnn]"));
        assert!(windows.contains(r"C:\Program Files\NVIDIA\CUDNN\v9.x\bin\12.x"));
        assert!(!windows.contains("apt-get"));
        assert!(windows.ends_with(&format!("{PROVIDER_ENV}=cpu.")));

        let linux = cuda_missing_message_for(false, "libcudnn.so.9: cannot open");
        assert!(linux.contains("onnxruntime-gpu[cuda,cudnn]"));
        assert!(linux.contains("apt-get install"));
        assert!(!linux.contains("CUDA_PATH"));
    }
}
