//! 无游戏 fixture — App: Send（Send-only World 的 App 跨线程转移，模拟 Handoff）：
//! App 从构造线程转移到另一线程后由该线程独占运行 driver。
//! 对应 docs/runtime-architecture.md 不变量「App 是唯一组合根…始终为同一个
//! Send 类型」与 docs/plugin-system.md「App: Send 可由编译期与无游戏 fixture 证明」。

mod common;

use bevy_ecs::prelude::Resource;
use corelib::{DataRoot, RuntimeGate};
use plugins::{AppCtx, Plugin, PluginError, UpdateCtx};
use shiny_song_tools::App;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

#[derive(Resource, Default)]
struct FrameCounter(AtomicUsize);

fn update_counts(
    _ctx: UpdateCtx<'_>,
    counter: bevy_ecs::prelude::Res<FrameCounter>,
) -> Result<(), PluginError> {
    counter.0.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

struct CountingPlugin;

impl Plugin for CountingPlugin {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        ctx.insert_resource(FrameCounter::default())?;
        ctx.add_update_system(update_counts);
        Ok(())
    }
}

#[test]
fn app_is_send_and_runs_after_cross_thread_handoff() {
    // Compile-time half of the proof: App must be Send to cross threads.
    fn assert_send<T: Send>() {}
    assert_send::<App>();

    let gate = RuntimeGate::new();
    let mut app = App::new(
        plugins::RuntimeConfig::default(),
        DataRoot::new(std::env::temp_dir().join("scsp-fixture-app-send")),
        gate.reader(),
    );
    app.add_plugin(CountingPlugin);

    let (tx, rx) = mpsc::channel::<App>();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            let mut app = rx.recv().expect("handoff publishes the app exactly once");
            // The receiving thread now exclusively owns and drives the App —
            // the worker-phase analog of the post-Handoff main thread.
            let token = common::fixture_main_token();
            app.run_update(&token);
            app.run_update(&token);
            app
        });
        tx.send(app).expect("send app to worker");
        let app = worker.join().expect("worker ran the app");
        let counter = app.world().resource::<FrameCounter>();
        assert_eq!(
            counter.0.load(Ordering::Acquire),
            2,
            "both update frames ran on the receiving thread"
        );
    });
}
