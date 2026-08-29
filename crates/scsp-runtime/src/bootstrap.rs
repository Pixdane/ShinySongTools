//! Bootstrap: readiness ladder, App construction, scheduler publication.
//!
//! Ladder (docs/runtime-crate.md):
//!   1. image identity — the ONLY pollable step (bounded deadline): the
//!      production poll lives in [`await_unity_framework`], which acquires
//!      and keeps alive the exact handle before `run_bootstrap` verifies the
//!      image identity in a single call;
//!   2. exports — single shot, fail closed;
//!   3. `domain_get` — the probe runs EXACTLY ONCE (no polling); null terminates
//!      bootstrap without retry (experiment-validated);
//!   4. attach (RAII detach of this attachment only) + metadata hydration;
//!   5. runtime/layout identity;
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
use scsp_core::{
    DataRoot, ExactHandle, Il2CppApi, Il2CppError, MethodPointerSlot, MethodRef, MethodResolver,
    RuntimeGate, TargetId,
};
use scsp_plugin_api::{Plugin, RuntimeConfig};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

/// The single experiment-validated scheduler target.
pub const SCHEDULER_TARGET: TargetId = TargetId {
    assembly: "UniRx.dll",
    namespace: "UniRx",
    class: "MainThreadDispatcher",
    method: "LateUpdate",
    param_count: 0,
};

/// Production ladder-1 parameters (docs/runtime-crate.md: 具体总超时与
/// backoff 属于实现参数，必须有界且可测试). Only ladder 1 may poll.
pub const IMAGE_POLL_DEADLINE: Duration = Duration::from_secs(120);
pub const IMAGE_POLL_BACKOFF: Duration = Duration::from_millis(250);

/// Deps injected for testability; the production worker builds them from the
/// real backend after ladder 1 acquired the exact handle.
pub struct BootstrapDeps {
    pub api: Arc<dyn Il2CppApi>,
    pub resolver: Arc<dyn MethodResolver>,
    pub data_root: DataRoot,
    /// Typed configuration parsed by `scsp_start` before the worker spawned
    /// (docs/runtime-crate.md scsp_start sequence); fail-closed already
    /// applied by `load_config`.
    pub config: RuntimeConfig,
    pub thread_check: ThreadIdentityCheck,
}

/// Production plugin list: DebugPlugin (feature-gated, config-gated) FIRST,
/// then functional plugins in fixed order (none until later phases).
fn production_plugins(config: &RuntimeConfig) -> Vec<Box<dyn Plugin>> {
    #[cfg(feature = "debug")]
    let mut list: Vec<Box<dyn Plugin>> = Vec::new();
    #[cfg(not(feature = "debug"))]
    let list: Vec<Box<dyn Plugin>> = Vec::new();
    // DebugPlugin registers at the HEAD of the list when enabled.
    #[cfg(feature = "debug")]
    if config.debug.enabled {
        list.push(Box::new(crate::debug::DebugPlugin));
    }
    #[cfg(not(feature = "debug"))]
    let _ = config;
    list
}

/// Ladder 1 (the ONLY pollable rung, non-IL2CPP): poll the dyld image list
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
        if let Some(path) = scsp_core::enumerate_unity_framework() {
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

/// Production dependency construction over an already-acquired exact handle.
pub fn production_deps(
    handle: Arc<ExactHandle>,
    data_root: &DataRoot,
    config: RuntimeConfig,
) -> BootstrapDeps {
    let backend = Arc::new(scsp_core::BridgeBackend::new(handle));
    BootstrapDeps {
        api: backend.clone(),
        resolver: backend,
        data_root: data_root.clone(),
        config,
        thread_check: crate::scheduler::pthread_main_check(),
    }
}

/// Run the one-shot bootstrap. Returns `true` when the App was published and
/// the scheduler hook installed.
pub fn run_bootstrap(deps: BootstrapDeps) -> bool {
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

    // Ladder 3: the domain probe runs exactly once; null terminates the
    // one-shot bootstrap. Polling this probe is forbidden
    // (experiment-validated). The bridge's cache hydration re-reads
    // domain_get internally at ladder 4 — post-gate re-reads are empirically
    // safe (two live A/B runs) and pinned by the bridge_fake_happy fixture.
    if let Err(err) = deps.api.domain_get() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 3 failed (one-shot terminated)");
        return bootstrap_failed(&gate, None);
    }

    // Ladder 4: attach (RAII detach of this attachment only) + metadata
    // hydration (expensive; runs after attach by design).
    let attach = match deps.api.attach_current_thread() {
        Ok(attach) => attach,
        Err(err) => {
            tracing::error!(target: "bootstrap", error = %err, "ladder 4 attach failed");
            return bootstrap_failed(&gate, None);
        }
    };
    if let Err(err) = deps.api.hydrate_metadata() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 4 metadata hydration failed");
        return bootstrap_failed(&gate, None);
    }

    // Ladder 5 (single shot): runtime/layout identity.
    if let Err(err) = deps.api.runtime_identity() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 5 identity mismatch");
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
