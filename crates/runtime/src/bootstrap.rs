//! Bootstrap: readiness ladder, App construction, scheduler publication.
//!
//! Ladder (docs/runtime-crate.md):
//!   1. image identity — pollable, non-IL2CPP (bounded deadline): the
//!      production poll lives in [`await_unity_framework`], which acquires
//!      and keeps alive the exact handle before `run_bootstrap` verifies the
//!      image identity in a single call;
//!   2. exports — single shot, fail closed;
//!   3. CRIWARE Unity completion — pollable, non-IL2CPP (bounded deadline);
//!   4. `domain_get` — the probe runs EXACTLY ONCE (no polling); null terminates
//!      bootstrap without retry (experiment-validated);
//!   5. attach (RAII detach of this attachment only) + metadata hydration;
//!   6. runtime/layout identity;
//!
//!   then: scheduler target resolution, App build with the production plugin
//!   list (DebugPlugin first when enabled), SchedulerContext publication,
//!   hook CAS install, Handoff publish.
//!
//! Any failure: one-shot termination. The gate is closed before anything
//! else can run; already-installed effects roll back in reverse owner order;
//! the un-handed-off App is dropped.

use crate::app::App;
use crate::scheduler::{
    SchedulerContext, SchedulerHook, ThreadIdentityCheck, install_hook, publish_context,
    scheduler_context,
};
use corelib::{
    DataRoot, ExactHandle, Il2CppApi, Il2CppError, MethodPointerSlot, MethodRef, MethodResolver,
    RuntimeGate, TargetId,
};
use corelib::{Plugin, RuntimeConfig};
use debug::DebugPlugin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use unlock_fps::FpsPlugin;

/// The single experiment-validated scheduler target.
pub const SCHEDULER_TARGET: TargetId = TargetId {
    assembly: "UniRx.dll",
    namespace: "UniRx",
    class: "MainThreadDispatcher",
    method: "LateUpdate",
    param_count: 0,
};

/// Production ladder-1 parameters (docs/runtime-crate.md: 具体总超时与
/// backoff 属于实现参数，必须有界且可测试).
pub const IMAGE_POLL_DEADLINE: Duration = Duration::from_secs(120);
pub const IMAGE_POLL_BACKOFF: Duration = Duration::from_millis(250);

/// Current CRIWARE Unity ABI symbol that returns the top-level Atom Unity
/// initialization-complete flag. The Mach-O export trie prints a leading
/// underscore; `dlsym` takes the C name without it.
pub const CRIWARE_UNITY_READY_SYMBOL: &str = "CRIWARE2813B966";

/// Production ladder-3 parameters. This gate is deliberately later than the
/// minimum IL2CPP-ready instant: the exported predicate becomes true only at
/// the end of CRIWARE's top-level Unity initialization, whose sole direct
/// caller in the validated build is IL2CPP-generated managed code.
pub const CRIWARE_POLL_DEADLINE: Duration = Duration::from_secs(120);
pub const CRIWARE_POLL_BACKOFF: Duration = Duration::from_millis(50);

/// Diagnostic-only delay between export loading and the single dangerous
/// `il2cpp_domain_get` call. This is an experiment parameter, not a
/// production readiness contract.
#[cfg(feature = "bootstrap-timing-probe")]
pub const TIMING_PROBE_DELAY: Duration = Duration::from_secs(5);

/// Deps injected for testability; the production worker builds them from the
/// real backend after ladder 1 acquired the exact handle.
pub struct BootstrapDeps {
    pub api: Arc<dyn Il2CppApi>,
    pub readiness: Arc<dyn BootstrapReadiness>,
    pub resolver: Arc<dyn MethodResolver>,
    pub data_root: DataRoot,
    /// Typed configuration parsed by `scsp_start` before the worker spawned
    /// (docs/runtime-crate.md scsp_start sequence); fail-closed already
    /// applied by `load_config`.
    pub config: RuntimeConfig,
    pub thread_check: ThreadIdentityCheck,
}

/// A non-IL2CPP readiness predicate that may be polled before the single
/// dangerous `il2cpp_domain_get` probe.
pub trait BootstrapReadiness: Send + Sync + 'static {
    fn is_ready(&self) -> Result<bool, Il2CppError>;
}

/// Production readiness probe over the exact UnityFramework handle.
///
/// The resolved function has the validated CRIWARE Unity ABI `int(void)` and
/// reads the same completion word written immediately before the top-level
/// Atom Unity initializer returns. Symbol absence is target drift and fails
/// closed; no global symbol lookup is attempted.
pub struct CriWareUnityReadiness {
    handle: Arc<ExactHandle>,
    status: OnceLock<Option<usize>>,
}

impl CriWareUnityReadiness {
    #[must_use]
    pub fn new(handle: Arc<ExactHandle>) -> Self {
        Self {
            handle,
            status: OnceLock::new(),
        }
    }
}

impl BootstrapReadiness for CriWareUnityReadiness {
    fn is_ready(&self) -> Result<bool, Il2CppError> {
        let address = self
            .status
            .get_or_init(|| self.handle.symbol(CRIWARE_UNITY_READY_SYMBOL))
            .ok_or(Il2CppError::ReadinessSymbolMissing(
                CRIWARE_UNITY_READY_SYMBOL,
            ))?;
        // SAFETY: static reverse engineering pinned this exact export to the
        // CRIWARE Unity `int(void)` readiness predicate. ExactHandle also
        // verifies that the address belongs to this UnityFramework image.
        let status: unsafe extern "C" fn() -> i32 = unsafe { core::mem::transmute(address) };
        // SAFETY: plain side-effect-free CRIWARE status query with no args.
        Ok(unsafe { status() } != 0)
    }
}

/// Production plugin list: DebugPlugin (config-gated) FIRST,
/// then functional plugins in fixed order.
fn production_plugins(config: &RuntimeConfig) -> Vec<Box<dyn Plugin>> {
    let mut list: Vec<Box<dyn Plugin>> = Vec::new();
    // DebugPlugin registers at the HEAD of the list when enabled.
    if config.debug.enabled {
        list.push(Box::new(DebugPlugin));
    }
    if config.fps.unlock_fps {
        list.push(Box::new(FpsPlugin));
    }
    list
}

/// Ladder 1 (pollable, non-IL2CPP): poll the dyld image list
/// until a UnityFramework image appears, then open and keep alive the exact
/// handle. The bounded deadline terminates the one-shot bootstrap.
///
/// Identity matching beyond the file name is a bootstrap parameter (docs
/// 待打磨项); the acquired handle's identity is verified once more through
/// `BootstrapDeps.api.unity_framework_image` inside `run_bootstrap`.
pub fn await_unity_framework(
    deadline: Duration,
    backoff: Duration,
) -> Result<Arc<ExactHandle>, Il2CppError> {
    let deadline_at = Instant::now() + deadline;
    loop {
        if let Some(path) = corelib::enumerate_unity_framework() {
            // SAFETY: the path comes from the dynamic loader's own image
            // table and refers to a live, already-loaded mach-o image.
            return unsafe { ExactHandle::open(&path) }.map(Arc::new);
        }
        if Instant::now() >= deadline_at {
            return Err(Il2CppError::ImageNotFound);
        }
        std::thread::sleep(backoff);
    }
}

/// Ladder 3: poll only the non-IL2CPP CRIWARE completion predicate within a
/// bounded monotonic deadline. A missing symbol or deadline expiry terminates
/// bootstrap before any IL2CPP API call.
pub fn await_bootstrap_readiness(
    readiness: &dyn BootstrapReadiness,
    deadline: Duration,
    backoff: Duration,
) -> Result<(), Il2CppError> {
    let deadline_at = Instant::now() + deadline;
    loop {
        if readiness.is_ready()? {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline_at {
            return Err(Il2CppError::ReadinessDeadlineExceeded);
        }
        std::thread::sleep(backoff.min(deadline_at.saturating_duration_since(now)));
    }
}

/// Production dependency construction over an already-acquired exact handle.
pub fn production_deps(
    handle: Arc<ExactHandle>,
    data_root: &DataRoot,
    config: RuntimeConfig,
) -> BootstrapDeps {
    let readiness = Arc::new(CriWareUnityReadiness::new(Arc::clone(&handle)));
    let backend = Arc::new(corelib::BridgeBackend::new(handle));
    BootstrapDeps {
        api: backend.clone(),
        readiness,
        resolver: backend,
        data_root: data_root.clone(),
        config,
        thread_check: crate::scheduler::pthread_main_check(),
    }
}

/// Diagnostic-only startup timing probe.
///
/// The probe changes one variable from the failed production attempt: after
/// the exact image and export table are available, it waits for a fixed,
/// bounded interval before calling `domain_get` exactly once. It deliberately
/// stops there: no thread attach, metadata hydration, target resolution,
/// MethodPointer access, App construction, or hook installation occurs.
///
/// A successful live run would confirm the initialization-race hypothesis;
/// it would not make a fixed delay a production readiness mechanism.
#[cfg(feature = "bootstrap-timing-probe")]
pub fn run_bootstrap_timing_probe(deps: BootstrapDeps) -> bool {
    run_bootstrap_timing_probe_with_delay(deps, TIMING_PROBE_DELAY)
}

/// Test seam for the diagnostic probe; production always uses
/// [`TIMING_PROBE_DELAY`].
#[cfg(feature = "bootstrap-timing-probe")]
pub fn run_bootstrap_timing_probe_with_delay(deps: BootstrapDeps, delay: Duration) -> bool {
    match deps.api.unity_framework_image() {
        Ok(image) => {
            tracing::info!(target: "bootstrap_probe", image = %image.name, "image identity verified");
        }
        Err(err) => {
            tracing::error!(target: "bootstrap_probe", error = %err, "image identity failed; probe terminated");
            return false;
        }
    }

    if let Err(err) = deps.api.load_exports() {
        tracing::error!(target: "bootstrap_probe", error = %err, "export loading failed; probe terminated");
        return false;
    }

    tracing::info!(
        target: "bootstrap_probe",
        delay_ms = delay.as_millis() as u64,
        "timing probe armed; waiting before the single domain_get call"
    );
    std::thread::sleep(delay);

    match deps.api.domain_get() {
        Ok(_) => {
            tracing::info!(target: "bootstrap_probe", "domain_get returned non-null; timing probe complete");
            true
        }
        Err(err) => {
            tracing::error!(target: "bootstrap_probe", error = %err, "domain_get failed; timing probe complete");
            false
        }
    }
}

/// Run the one-shot bootstrap. Returns `true` when the App was published and
/// the scheduler hook installed.
pub fn run_bootstrap(deps: BootstrapDeps) -> bool {
    run_bootstrap_with_readiness_wait(deps, CRIWARE_POLL_DEADLINE, CRIWARE_POLL_BACKOFF)
}

/// Test seam for the production bootstrap; production always uses the
/// [`CRIWARE_POLL_DEADLINE`] and [`CRIWARE_POLL_BACKOFF`] constants.
pub fn run_bootstrap_with_readiness_wait(
    deps: BootstrapDeps,
    readiness_deadline: Duration,
    readiness_backoff: Duration,
) -> bool {
    // The gate exists from the very start of the bootstrap so every failure
    // path below can close it.
    let gate = RuntimeGate::new();

    // Ladder 1 (single-shot verification): the exact handle was acquired by
    // `await_unity_framework` — the bounded-deadline poll of the image list
    // that production runs before this function. Verify the identity of the
    // acquired image; no IL2CPP API beyond this query is touched here.
    match deps.api.unity_framework_image() {
        Ok(image) => {
            tracing::info!(target: "bootstrap", image = %image.name, "ladder 1: image identity verified");
        }
        Err(err) => {
            tracing::error!(target: "bootstrap", error = %err, "ladder 1: image identity failed");
            return bootstrap_failed(&gate, None);
        }
    }

    // Ladder 2 (single shot): exports from the exact handle.
    if let Err(err) = deps.api.load_exports() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 2 failed");
        return bootstrap_failed(&gate, None);
    }

    // Ladder 3 (bounded poll, non-IL2CPP): wait until the top-level CRIWARE
    // Unity initializer has reached its completion flag. Missing/drifted
    // symbols and timeout both terminate before domain_get is touched.
    if let Err(err) = await_bootstrap_readiness(
        deps.readiness.as_ref(),
        readiness_deadline,
        readiness_backoff,
    ) {
        tracing::error!(target: "bootstrap", error = %err, "ladder 3: CRIWARE Unity readiness failed");
        return bootstrap_failed(&gate, None);
    }

    // Ladder 4: the domain probe runs exactly once; null terminates the
    // one-shot bootstrap. Polling this probe is forbidden
    // (experiment-validated). The bridge's cache hydration re-reads
    // domain_get internally at ladder 5 — post-gate re-reads are empirically
    // safe (two live A/B runs) and pinned by the bridge_fake_happy fixture.
    if let Err(err) = deps.api.domain_get() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 4 failed (one-shot terminated)");
        return bootstrap_failed(&gate, None);
    }

    // Ladder 5: attach (RAII detach of this attachment only) + metadata
    // hydration (expensive; runs after attach by design).
    let attach = match deps.api.attach_current_thread() {
        Ok(attach) => attach,
        Err(err) => {
            tracing::error!(target: "bootstrap", error = %err, "ladder 5 attach failed");
            return bootstrap_failed(&gate, None);
        }
    };
    if let Err(err) = deps.api.hydrate_metadata() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 5 metadata hydration failed");
        return bootstrap_failed(&gate, None);
    }

    // Ladder 6 (single shot): runtime/layout identity.
    if let Err(err) = deps.api.runtime_identity() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 6 identity mismatch");
        return bootstrap_failed(&gate, None);
    }

    // Scheduler target resolution: missing scheduler target fails the whole
    // bootstrap (functional plugin targets would only retire their plugin).
    let method = match deps.resolver.resolve(&SCHEDULER_TARGET) {
        Ok(method) => method,
        Err(err) => {
            tracing::error!(target: "bootstrap", error = %err, "scheduler target resolution failed");
            return bootstrap_failed(&gate, None);
        }
    };
    if !validate_scheduler_method(&method) {
        tracing::error!(target: "bootstrap", "scheduler target drifted from the validated shape");
        return bootstrap_failed(&gate, None);
    }

    // Typed original capture + slot bind (reviewed raw->typed boundary).
    let slot_memory = deps.resolver.slot_memory(&method);
    let slot = match MethodPointerSlot::bind(Arc::clone(&slot_memory)) {
        Ok(slot) => Arc::new(slot),
        Err(err) => {
            tracing::error!(target: "bootstrap", error = %err, "slot bind failed");
            return bootstrap_failed(&gate, None);
        }
    };
    let original: crate::scheduler::LateUpdateFn = unsafe {
        // SAFETY: reviewed construction boundary — the slot's captured
        // original is a code pointer with the exact scheduler ABI validated
        // by ladder 5 and the identity match above.
        core::mem::transmute::<usize, crate::scheduler::LateUpdateFn>(slot.original())
    };

    // App build (worker phase, thread-independent). Keep a gate clone: the
    // original moves into the scheduler context, and the failure paths below
    // must still close the process gate. The config was parsed by scsp_start
    // (fail-closed) and travels in deps.
    let config = deps.config.clone();
    let mut app = App::new(config.clone(), deps.data_root.clone(), gate.reader());
    let gate_for_failures = gate.clone();
    app.set_method_resolver(Arc::clone(&deps.resolver));
    for plugin in production_plugins(&config) {
        app.add_boxed_plugin(plugin);
    }

    // Publication order (fixed): original captured -> hook built -> empty
    // Handoff + context with a still-closed gate -> SCHEDULER.set -> CAS
    // install -> Handoff.publish(App).
    let hook = SchedulerHook::new(slot_memory, original);
    let context = SchedulerContext {
        handoff: crate::handoff::Handoff::empty(),
        hook,
        runtime_gate: gate,
        failed: AtomicBool::new(false),
        main_thread_check: deps.thread_check,
    };
    if !publish_context(context) {
        // A second bootstrap in one process is an invariant break: the
        // published context (and its gate) belongs to the first bootstrap.
        // This attempt's gate was consumed by the refused context and never
        // became the process gate — closing it would be meaningless. The
        // freshly built App is unwound here so any hook that DID install on
        // a not-yet-occupied target gets its slot restored before the App
        // drops; `scsp_start`'s one-shot marker keeps production off this
        // path.
        tracing::error!(target: "bootstrap", "scheduler context already published; unwinding this attempt's App");
        app.teardown_all();
        return false;
    }
    let context = scheduler_context().expect("just published");
    let mut app = Some(app);

    if install_hook(&context.hook).is_err() {
        tracing::error!(target: "bootstrap", "scheduler hook install failed; rolling back and dropping the un-handed-off App");
        if let Some(app) = app.as_mut() {
            app.teardown_all();
        }
        gate_for_failures.close();
        return false;
    }

    if !context
        .handoff
        .publish(Box::new(app.take().expect("app not yet published")))
    {
        tracing::error!(target: "bootstrap", "handoff publish refused; rolling back and dropping the App");
        if let Some(app) = app.as_mut() {
            app.teardown_all();
        }
        gate_for_failures.close();
        return false;
    }

    // Detach this worker's attachment only.
    drop(attach);
    true
}

fn bootstrap_failed(gate: &RuntimeGate, app: Option<&mut App>) -> bool {
    gate.close();
    if let Some(app) = app {
        // Reverse-owner rollback of already-installed effects (worker side:
        // MainThread restore actions report failed, they never run here).
        app.teardown_all();
    }
    false
}

/// Validated scheduler shape: instance method, zero explicit parameters,
/// `System.Void` return is carried by the exact identity match against the
/// experiment-validated target.
fn validate_scheduler_method(method: &MethodRef) -> bool {
    method.matches_target(&SCHEDULER_TARGET)
}

#[cfg(test)]
mod production_plugin_tests {
    use super::production_plugins;
    use corelib::{DebugConfig, FpsConfig, RuntimeConfig};

    #[test]
    fn debug_plugin_precedes_fps_when_both_are_enabled() {
        let config = RuntimeConfig {
            debug: DebugConfig { enabled: true },
            fps: FpsConfig { unlock_fps: true },
        };
        let names: Vec<_> = production_plugins(&config)
            .iter()
            .map(|plugin| plugin.name())
            .collect();
        assert_eq!(names, vec!["debug", "unlock_fps"]);
    }
}
