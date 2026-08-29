//! Bootstrap: readiness ladder, App construction, scheduler publication.
//!
//! Ladder (docs/runtime-crate.md):
//!   1. image identity — the ONLY pollable step (bounded deadline);
//!   2. exports — single shot, fail closed;
//!   3. `domain_get` — EXACTLY ONCE per process; null terminates the one-shot
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
use crate::config::load_config;
use crate::scheduler::{
    SchedulerContext, SchedulerHook, ThreadIdentityCheck, install_hook, publish_context,
    scheduler_context,
};
use scsp_core::{
    DataRoot, Il2CppApi, MethodPointerSlot, MethodRef, MethodResolver, RuntimeGate, TargetId,
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

/// Deps injected for testability; the production worker builds them from the
/// real backend.
pub struct BootstrapDeps {
    pub api: Arc<dyn Il2CppApi>,
    pub resolver: Arc<dyn MethodResolver>,
    pub data_root: DataRoot,
    pub thread_check: ThreadIdentityCheck,
    /// Ladder 1 polling budget (production parameter, bounded).
    pub image_poll_deadline: Duration,
    /// Ladder 1 poll backoff (production parameter).
    pub image_poll_backoff: Duration,
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

/// Production dependency construction: exact UnityFramework image from dyld
/// enumeration, the bridge-crate backend, and the pthread identity check.
/// Returns `None` when no UnityFramework image is present yet (the ladder's
/// polling lives inside `run_bootstrap`; a missing image at worker start
/// simply means the ladder keeps polling).
pub fn production_deps(data_root: &DataRoot) -> Option<BootstrapDeps> {
    let image_path = scsp_core::enumerate_unity_framework()?;
    // SAFETY: the path comes from the dynamic loader's own image table and
    // refers to a live, already-loaded mach-o image.
    let handle = unsafe { scsp_core::ExactHandle::open(&image_path) }.ok()?;
    let backend = Arc::new(scsp_core::BridgeBackend::new(Arc::new(handle)));
    Some(BootstrapDeps {
        api: backend.clone(),
        resolver: backend,
        data_root: data_root.clone(),
        thread_check: crate::scheduler::pthread_main_check(),
        image_poll_deadline: Duration::from_secs(120),
        image_poll_backoff: Duration::from_millis(250),
    })
}

/// Run the one-shot bootstrap. Returns `true` when the App was published and
/// the scheduler hook installed.
pub fn run_bootstrap(deps: BootstrapDeps) -> bool {
    // The gate exists from the very start of the bootstrap so every failure
    // path below can close it.
    let gate = RuntimeGate::new();

    // Ladder 1 (only pollable step): image identity within a bounded
    // deadline.
    let deadline = Instant::now() + deps.image_poll_deadline;
    loop {
        match deps.api.unity_framework_image() {
            Ok(image) => {
                tracing::info!(target: "bootstrap", image = %image.name, "ladder 1: image found");
                break;
            }
            Err(err) => {
                if Instant::now() >= deadline {
                    tracing::error!(target: "bootstrap", error = %err, "ladder 1: image deadline exceeded");
                    return bootstrap_failed(&gate, None);
                }
                std::thread::sleep(deps.image_poll_backoff);
            }
        }
    }

    // Ladder 2 (single shot): exports from the exact handle.
    if let Err(err) = deps.api.load_exports() {
        tracing::error!(target: "bootstrap", error = %err, "ladder 2 failed");
        return bootstrap_failed(&gate, None);
    }

    // Ladder 3 (exactly once): domain_get. Null terminates the one-shot
    // bootstrap; polling this probe is forbidden (experiment-validated).
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
    // must still close the process gate.
    let config = load_config(&deps.data_root);
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
        // A second bootstrap in one process is an invariant break.
        tracing::error!(target: "bootstrap", "scheduler context already published");
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
