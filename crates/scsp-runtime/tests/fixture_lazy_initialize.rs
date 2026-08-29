//! 无游戏 fixture — 惰性初始化机制（boxed system 首次运行前才 initialize）：
//! Update system 引用 Startup 阶段才插入的 resource 正常解析；后注册插件
//! 引用前序插件 Startup 产物同样成立。
//! 对应 docs/plugin-system.md「惰性初始化规则」与 docs/2026-08-29-… §2.12 第 2 条。

mod common;

use bevy_ecs::prelude::Resource;
use scsp_core::{DataRoot, GateReader, RuntimeGate};
use scsp_plugin_api::{AppCtx, Plugin, PluginError, StartupCtx, UpdateCtx};
use shiny_song_tools::App;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Resource)]
struct StartupProduct(u64);

#[derive(Resource, Default)]
struct UpdateRan(AtomicUsize);

#[derive(Resource, Default)]
struct CrossPluginObserved(AtomicU64);

fn startup_insert(mut ctx: StartupCtx<'_>) -> Result<(), PluginError> {
    ctx.insert_resource(StartupProduct(7))?;
    Ok(())
}

fn update_reads_startup_resource(
    _ctx: UpdateCtx<'_>,
    product: bevy_ecs::prelude::Res<StartupProduct>,
    ran: bevy_ecs::prelude::Res<UpdateRan>,
) -> Result<(), PluginError> {
    assert_eq!(product.0, 7, "startup-inserted resource must resolve");
    ran.0.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

fn cross_plugin_update(
    _ctx: UpdateCtx<'_>,
    product: bevy_ecs::prelude::Res<StartupProduct>,
    observed: bevy_ecs::prelude::Res<CrossPluginObserved>,
) -> Result<(), PluginError> {
    observed.0.store(product.0, Ordering::Release);
    Ok(())
}

struct FirstPlugin;

impl Plugin for FirstPlugin {
    fn name(&self) -> &'static str {
        "first"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        // The update system is registered at build time, while its resource
        // only comes into existence during Startup: lazy initialize is what
        // makes this valid.
        ctx.insert_resource(UpdateRan::default())?;
        ctx.add_startup_system(startup_insert);
        ctx.add_update_system(update_reads_startup_resource);
        Ok(())
    }
}

struct SecondPlugin;

impl Plugin for SecondPlugin {
    fn name(&self) -> &'static str {
        "second"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        ctx.insert_resource(CrossPluginObserved::default())?;
        ctx.add_update_system(cross_plugin_update);
        Ok(())
    }
}

#[test]
fn lazy_initialize_update_system_resolves_startup_resource() {
    let gate = RuntimeGate::new();
    let reader: GateReader = gate.reader();
    let mut app = App::new(
        scsp_plugin_api::RuntimeConfig::default(),
        DataRoot::new(std::env::temp_dir().join("scsp-fixture-lazy-init")),
        reader,
    );
    app.add_plugin(FirstPlugin);
    app.add_plugin(SecondPlugin);

    let token = common::fixture_main_token();

    // First callback: Startup only.
    let report = app.run_startup(&token);
    assert!(
        report.retired.is_empty(),
        "startup must succeed: {report:?}"
    );
    assert!(
        app.world().contains_resource::<StartupProduct>(),
        "startup insert applied at the system boundary"
    );

    // Subsequent callback: Update systems lazily initialize and resolve.
    let report = app.run_update(&token);
    assert!(report.retired.is_empty(), "update must succeed: {report:?}");
    let ran_count = app
        .world()
        .resource::<UpdateRan>()
        .0
        .load(Ordering::Acquire);
    assert_eq!(ran_count, 1);
    let observed = app.world().resource::<CrossPluginObserved>();
    assert_eq!(
        observed.0.load(Ordering::Acquire),
        7,
        "later plugin's update resolves an earlier plugin's Startup product"
    );

    // Second frame: update runs again.
    app.run_update(&token);
    assert_eq!(
        app.world()
            .resource::<UpdateRan>()
            .0
            .load(Ordering::Acquire),
        2
    );
}
