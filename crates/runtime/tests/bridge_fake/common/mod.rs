//! Shared support for the bridge-adapter fixtures (one process per scenario:
//! the bridge crate's symbol cache and metadata cache are process-global).

use corelib::{BridgeBackend, DataRoot, ExactHandle};
use shiny_song_tools::bootstrap::{BootstrapDeps, CriWareUnityReadiness};
use shiny_song_tools::scheduler::pthread_main_check;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// Build (or reuse) the fake UnityFramework cdylib and return its path.
/// Nested cargo inherits the workspace `.cargo/config.toml`, so the artifact
/// lands under `build/target/` like every other build output.
#[must_use]
pub fn fake_dylib() -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "fake-unity-framework",
            "--message-format=json",
            "--quiet",
        ])
        .output()
        .expect("spawn cargo for fake-unity-framework");
    assert!(
        output.status.success(),
        "fake-unity-framework build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("reason").and_then(|r| r.as_str()) != Some("compiler-artifact") {
            continue;
        }
        let kinds = value["target"]["kind"].as_array();
        let is_cdylib =
            kinds.is_some_and(|kinds| kinds.iter().any(|k| k.as_str() == Some("cdylib")));
        if is_cdylib && let Some(path) = value.get("executable").and_then(|e| e.as_str()) {
            return PathBuf::from(path);
        }
    }
    // Fresh (cached) builds emit no artifact JSON: fall back to the known
    // target-dir locations. The manifest dir is crates/runtime, so the
    // workspace target dir is two levels up.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("build/target");
    for profile in ["debug", "release"] {
        let candidate = root.join(profile).join("libfake_unity_framework.dylib");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!("cdylib artifact not found in cargo output");
}

/// `ExactHandle` over the fake image. The handle stays shared so tests can
/// read the counter exports at runtime (the fake is dlopen-loaded, never
/// linked at build time).
///
/// # Safety
///
/// The path was just produced by cargo and is a loadable dylib.
#[must_use]
pub fn fake_handle(path: &Path) -> Arc<ExactHandle> {
    // SAFETY: see doc comment.
    let handle = unsafe { ExactHandle::open(path) }.expect("open fake UnityFramework");
    Arc::new(handle)
}

/// Production-shaped bootstrap deps over the fake image. The config is the
/// fail-closed default, as `scsp_start` parses it for a missing scsp.toml.
#[must_use]
pub fn fake_deps(handle: &Arc<ExactHandle>) -> BootstrapDeps {
    let backend = Arc::new(BridgeBackend::new(Arc::clone(handle)));
    BootstrapDeps {
        api: backend.clone(),
        readiness: Arc::new(CriWareUnityReadiness::new(Arc::clone(handle))),
        resolver: backend,
        data_root: DataRoot::new(PathBuf::from("/tmp/scsp-fake-documents")),
        config: corelib::RuntimeConfig::default(),
        thread_check: pthread_main_check(),
    }
}

fn counter(handle: &ExactHandle, name: &str) -> usize {
    let address = handle
        .symbol(name)
        .unwrap_or_else(|| panic!("missing counter export {name}"));
    // SAFETY: the address belongs to the fake dylib and has this ABI.
    let function: unsafe extern "C" fn() -> usize = unsafe { core::mem::transmute(address) };
    // SAFETY: plain counter export with no side effects.
    unsafe { function() }
}

/// Real `il2cpp_domain_get` calls made by this process so far.
#[must_use]
pub fn domain_get_count(handle: &ExactHandle) -> usize {
    counter(handle, "scsp_fixture_domain_get_count")
}

/// Attachments the fake runtime has detached so far.
#[must_use]
#[allow(dead_code)]
pub fn detach_count(handle: &ExactHandle) -> usize {
    counter(handle, "scsp_fixture_detach_count")
}

/// Calls made through the production CRIWARE readiness export.
#[must_use]
#[allow(dead_code)]
pub fn criware_ready_count(handle: &ExactHandle) -> usize {
    counter(handle, "scsp_fixture_criware_ready_count")
}
