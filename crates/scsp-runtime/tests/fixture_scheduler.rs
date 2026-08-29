//! 无游戏 fixture — 调度全链（docs/runtime-crate.md 固定 callback 顺序）：
//! 1. Handoff 竞争窗口：Pending → 本次只调 original，publish 后下一帧领取；
//! 2. 嵌套 callback：外层持有 App（Busy）时嵌套只调 original，不重入 driver；
//! 3. failed 先行观察：Running 的 App 处置为 Exited；
//! 4. BeforeOriginal panic：补调 original 恰好一次（经 PanicOnDrop 资源的
//!    LIFO 回滚在 per-system catch 之外产生帧级 unwind），TLS 保持 Busy；
//! 5. 错误线程：身份判据不匹配 → TLS Unavailable + global failure；
//! 6. 首帧 Startup 顺序与 RuntimeGate 最后开启；TLS 五态断言。
//!
//! 注：Rust panic 在 `extern "C"` original 内会直接 abort（非 unwind），
//! 因此 “original 调用中途崩溃” 的生产场景由 core 的 OriginalGuard 单测
//! 覆盖 no-double-call 语义；本文件的帧全部使用可正常调用的 mock original。
//! 对应验证顺序 §2.12 第 4 条。

mod common;

use common::{MockSlotMemory, original_calls, reset_original_calls};
use scsp_core::{DataRoot, RuntimeGate, SlotMemory};
use shiny_song_tools::scheduler::tls_snapshot;
use shiny_song_tools::{App, Handoff, SchedulerContext, SchedulerHook};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

fn context_with(app: Option<App>, main_thread: bool) -> &'static SchedulerContext {
    reset_original_calls();
    let slot_memory: Arc<dyn SlotMemory> = Arc::new(MockSlotMemory(Arc::new(
        std::sync::atomic::AtomicUsize::new(common::mock_lateupdate as *const () as usize),
    )));
    let hook = SchedulerHook::new(slot_memory, common::mock_lateupdate);
    let handoff = Handoff::empty();
    if let Some(app) = app {
        assert!(handoff.publish(Box::new(app)));
    }
    Box::leak(Box::new(SchedulerContext {
        handoff,
        hook,
        runtime_gate: RuntimeGate::new(),
        failed: AtomicBool::new(false),
        main_thread_check: Arc::new(move || main_thread),
    }))
}

/// Run `body` on a fresh thread (fresh TLS in AwaitingHandoff) and join.
fn on_fresh_thread(body: impl FnOnce() + Send + 'static) {
    std::thread::scope(|scope| {
        scope.spawn(|| {
            reset_original_calls();
            body();
        });
    });
}

fn new_app(ctx: &SchedulerContext, name: &str) -> (App, Arc<std::sync::atomic::AtomicUsize>) {
    let mut app = App::new(
        scsp_plugin_api::RuntimeConfig::default(),
        DataRoot::new(std::env::temp_dir().join(name)),
        ctx.runtime_gate.reader(),
    );
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    app.add_plugin(LocalCountingPlugin {
        counter: Arc::clone(&counter),
    });
    (app, counter)
}

/// Counts update-driver frames per App (parallel-test safe).
struct LocalCountingPlugin {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl scsp_plugin_api::Plugin for LocalCountingPlugin {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn build(&self, ctx: &mut scsp_plugin_api::AppCtx<'_>) -> Result<(), scsp_core::PluginError> {
        let counter = Arc::clone(&self.counter);
        ctx.insert_resource(RunCounter(counter))?;
        ctx.add_update_system(counting_update);
        Ok(())
    }
}

#[derive(bevy_ecs::prelude::Resource)]
struct RunCounter(Arc<std::sync::atomic::AtomicUsize>);

fn counting_update(
    _ctx: scsp_plugin_api::UpdateCtx<'_>,
    counter: bevy_ecs::prelude::Res<RunCounter>,
) -> Result<(), scsp_core::PluginError> {
    counter.0.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

// ---------------------------------------------------------------------------
// 1. Handoff 竞争窗口 + 6. 首帧顺序 / RuntimeGate 最后开启 / TLS 五态。
// ---------------------------------------------------------------------------

#[test]
fn handoff_pending_retry_then_first_startup_opens_gate_last() {
    let ctx = context_with(None, true);
    on_fresh_thread(move || {
        // Frame 1: handoff empty → Pending → original only, TLS untouched.
        ctx.run_frame();
        assert_eq!(original_calls(), 1);
        assert_eq!(tls_snapshot(), ("awaiting_handoff", None));

        // The worker publishes; the next frame claims and runs the App.
        let (app, update_runs) = new_app(ctx, "scsp-fixture-sched-pending");
        assert!(ctx.handoff.publish(Box::new(app)));

        ctx.run_frame();
        // First frame: Startup driver only — update has not run yet (checked
        // below); the gate opened LAST (after the startup driver completed).
        assert_eq!(tls_snapshot(), ("running", Some(true)));
        assert!(ctx.runtime_gate.reader().is_open(), "gate opened last");
        assert_eq!(
            update_runs.load(Ordering::Acquire),
            0,
            "first callback runs Startup, not Update"
        );

        // Second frame: the fixed Update driver.
        ctx.run_frame();
        assert_eq!(tls_snapshot(), ("running", Some(true)));
        assert_eq!(update_runs.load(Ordering::Acquire), 1);
    });
}

// ---------------------------------------------------------------------------
// 2. 嵌套 callback：Busy 让路（嵌套只调 original，不重入 driver）。
// ---------------------------------------------------------------------------

static NESTED_CTX: OnceLock<usize> = OnceLock::new();

#[test]
fn nested_callback_sees_busy_and_only_calls_original() {
    let ctx = context_with(None, true);
    NESTED_CTX.set(ctx as *const SchedulerContext as usize).ok();
    on_fresh_thread(move || {
        let (mut app, update_runs) = new_app(ctx, "scsp-fixture-sched-nested");
        // Frame 2 will re-enter the scheduler from inside an Update system.
        app.add_plugin(NestedReentryPlugin);
        assert!(ctx.handoff.publish(Box::new(app)));

        let before = original_calls();
        ctx.run_frame(); // startup frame
        assert_eq!(update_runs.load(Ordering::Acquire), 0, "startup only");
        ctx.run_frame(); // update frame: the update system re-enters
        assert_eq!(tls_snapshot(), ("running", Some(true)));

        // The nested frame saw Busy: it called original once more but did
        // NOT run another driver pass (update ran exactly once).
        // original total = startup frame (1) + update frame outer (1)
        //                + nested passthrough (1).
        assert_eq!(
            original_calls(),
            before + 3,
            "nested callback called original only"
        );
        assert_eq!(update_runs.load(Ordering::Acquire), 1, "no driver re-entry");
    });
}

struct NestedReentryPlugin;

impl scsp_plugin_api::Plugin for NestedReentryPlugin {
    fn name(&self) -> &'static str {
        "nested-reentry"
    }

    fn build(
        &self,
        ctx: &mut scsp_plugin_api::AppCtx<'_>,
    ) -> Result<(), scsp_plugin_api::PluginError> {
        ctx.add_update_system(reenter_from_update);
        Ok(())
    }
}

fn reenter_from_update(_ctx: scsp_plugin_api::UpdateCtx<'_>) -> Result<(), scsp_core::PluginError> {
    let ctx = *NESTED_CTX.get().expect("nested context installed");
    // SAFETY: the fixture leaked the context for the test's lifetime.
    let ctx = unsafe { &*(ctx as *const SchedulerContext) };
    ctx.run_frame();
    Ok(())
}

// ---------------------------------------------------------------------------
// 3. failed 先行观察：Running 的 App 处置为 Exited。
// ---------------------------------------------------------------------------

#[test]
fn failure_before_frame_puts_running_app_into_exited() {
    let ctx = context_with(None, true);
    on_fresh_thread(move || {
        let (app, update_runs) = new_app(ctx, "scsp-fixture-sched-exited");
        assert!(ctx.handoff.publish(Box::new(app)));
        ctx.run_frame();
        assert_eq!(tls_snapshot(), ("running", Some(true)));
        assert_eq!(
            update_runs.load(Ordering::Acquire),
            0,
            "frame 1 runs Startup only"
        );

        // Infrastructure-level global failure without a frame panic.
        ctx.publish_global_failure();
        ctx.run_frame();
        assert_eq!(
            tls_snapshot(),
            ("exited", Some(true)),
            "App disposition: Exited after global failure"
        );
        assert_eq!(original_calls(), 2);
        assert_eq!(
            update_runs.load(Ordering::Acquire),
            0,
            "no driver after failure (Startup ran in frame 1, Update never ran)"
        );
    });
}

// ---------------------------------------------------------------------------
// 4. BeforeOriginal panic：补调一次（驱动级 unwind 经 PanicOnDrop 回滚）。
// ---------------------------------------------------------------------------

#[derive(bevy_ecs::prelude::Resource)]
struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("drop panic during rollback");
    }
}

struct PanicOnDropPlugin;

impl scsp_plugin_api::Plugin for PanicOnDropPlugin {
    fn name(&self) -> &'static str {
        "panic-on-drop"
    }

    fn build(
        &self,
        ctx: &mut scsp_plugin_api::AppCtx<'_>,
    ) -> Result<(), scsp_plugin_api::PluginError> {
        // The resource's Drop panics; the Startup driver's LIFO rollback
        // removes it OUTSIDE the per-system catch_unwind, producing a
        // frame-level unwind before the original call.
        ctx.insert_resource(PanicOnDrop)?;
        ctx.add_startup_system(failing_startup);
        Ok(())
    }
}

fn failing_startup(_ctx: scsp_plugin_api::StartupCtx<'_>) -> Result<(), scsp_core::PluginError> {
    Err(scsp_core::PluginError::Message(
        "startup fails after build insert",
    ))
}

#[test]
fn before_original_panic_compensates_original_exactly_once() {
    let ctx = context_with(None, true);
    on_fresh_thread(move || {
        let mut app = App::new(
            scsp_plugin_api::RuntimeConfig::default(),
            DataRoot::new(std::env::temp_dir().join("scsp-fixture-sched-before")),
            ctx.runtime_gate.reader(),
        );
        app.add_plugin(PanicOnDropPlugin);
        assert!(ctx.handoff.publish(Box::new(app)));

        ctx.run_frame();
        assert_eq!(
            original_calls(),
            1,
            "BeforeOriginal panic compensates the original exactly once"
        );
        assert!(ctx.failed.load(Ordering::Acquire));
        assert!(!ctx.runtime_gate.reader().is_open());
        // The frame never committed its App: TLS stays Busy (retention root).
        assert_eq!(tls_snapshot().0, "busy");
    });
}

// ---------------------------------------------------------------------------
// 5. 错误线程：身份判据不匹配 → TLS Unavailable + global failure。
// ---------------------------------------------------------------------------

#[test]
fn untrusted_thread_publishes_failure_and_marks_unavailable() {
    let untrusted = context_with(None, false);
    on_fresh_thread(move || {
        untrusted.run_frame();
        assert_eq!(tls_snapshot().0, "unavailable");
        assert!(untrusted.failed.load(Ordering::Acquire));
        assert!(!untrusted.runtime_gate.reader().is_open());
        assert_eq!(original_calls(), 1, "passthrough only");
    });
}
