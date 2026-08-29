//! 无游戏 fixture — owner ledger 回滚矩阵：
//! 1. 直接插入重复类型 → ResourceConflict，不覆盖，build 失败退役；
//! 2. Startup 失败 → ledger LIFO 移除该 owner 的 Build+Startup 资源；
//! 3. 依赖被移除资源的插件在首跑 param validation 失败退役；
//! 4. 逐 system panic：World 保活、该 owner 退役、其余 owner 继续、
//!    Update 失败不移除资源。
//!
//! 对应 docs/plugin-system.md「局部回滚与 restore ledger」「AppWorld 共享与
//! panic 边界」与验证顺序 §2.12 第 2 条。

mod common;

use bevy_ecs::prelude::Resource;
use corelib::{DataRoot, RuntimeGate};
use plugins::{AppCtx, Plugin, PluginError, StartupCtx, UpdateCtx};
use shiny_song_tools::{App, PluginState};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Resource)]
struct FirstOwnerState(u32);

#[derive(Resource, Default)]
struct AliveMarker(AtomicUsize);

fn first_startup(mut ctx: StartupCtx<'_>) -> Result<(), PluginError> {
    ctx.insert_resource(FirstOwnerState(11))?;
    Ok(())
}

fn first_startup_fails(_ctx: StartupCtx<'_>) -> Result<(), PluginError> {
    Err(PluginError::Message("startup boom"))
}

fn dependent_update(
    _ctx: UpdateCtx<'_>,
    _state: bevy_ecs::prelude::Res<FirstOwnerState>,
) -> Result<(), PluginError> {
    Ok(())
}

fn survivor_update(
    _ctx: UpdateCtx<'_>,
    marker: bevy_ecs::prelude::Res<AliveMarker>,
) -> Result<(), PluginError> {
    marker.0.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

fn panicking_update(
    _ctx: UpdateCtx<'_>,
    _marker: bevy_ecs::prelude::Res<AliveMarker>,
) -> Result<(), PluginError> {
    panic!("update panic");
}

fn new_app(gate: &RuntimeGate) -> App {
    App::new(
        plugins::RuntimeConfig::default(),
        DataRoot::new(std::env::temp_dir().join("scsp-fixture-rollback")),
        gate.reader(),
    )
}

/// Inserts `FirstOwnerState` during build (worker phase).
struct OwnerStatePlugin(u32);

impl Plugin for OwnerStatePlugin {
    fn name(&self) -> &'static str {
        "owner-state"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        ctx.insert_resource(FirstOwnerState(self.0))
    }
}

#[test]
fn duplicate_resource_insert_conflicts_and_retires_builder() {
    struct ConflictPlugin;

    impl Plugin for ConflictPlugin {
        fn name(&self) -> &'static str {
            "conflict"
        }

        fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
            // FirstOwnerState already exists (previous plugin's build):
            // must conflict, not overwrite.
            ctx.insert_resource(FirstOwnerState(99))
        }
    }

    let gate = RuntimeGate::new();
    let mut app = new_app(&gate);
    app.add_plugin(OwnerStatePlugin(11));
    app.add_plugin(ConflictPlugin);

    let records = app.plugins().records();
    assert_eq!(records[0].state, PluginState::Active, "first stays active");
    assert_eq!(
        records[1].state,
        PluginState::Retired,
        "conflicting plugin retired at build"
    );
    assert!(
        !records[1].gate.is_open(),
        "retired owner gate must be closed"
    );
    assert_eq!(
        app.world().resource::<FirstOwnerState>().0,
        11,
        "conflicting insert must not overwrite"
    );
}

#[test]
fn startup_failure_removes_resources_lifo_and_dependents_fail_param_validation() {
    struct FailAfterInsertPlugin;

    impl Plugin for FailAfterInsertPlugin {
        fn name(&self) -> &'static str {
            "fail-after-insert"
        }

        fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
            // Build inserts first; the second Startup system fails after the
            // first inserted more. Both must be removed LIFO.
            ctx.insert_resource(FirstOwnerState(5))?;
            ctx.add_startup_system(first_startup);
            ctx.add_startup_system(first_startup_fails);
            Ok(())
        }
    }

    struct DependentPlugin;

    impl Plugin for DependentPlugin {
        fn name(&self) -> &'static str {
            "dependent"
        }

        fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
            ctx.add_update_system(dependent_update);
            Ok(())
        }
    }

    let gate = RuntimeGate::new();
    let mut app = new_app(&gate);
    app.add_plugin(FailAfterInsertPlugin);
    app.add_plugin(DependentPlugin);

    let token = common::fixture_main_token();
    app.run_startup(&token);
    app.run_update(&token);

    assert!(
        !app.world().contains_resource::<FirstOwnerState>(),
        "LIFO removal must clear the failed owner's resources"
    );
    let records = app.plugins().records();
    assert_eq!(records[0].state, PluginState::Retired);
    assert_eq!(
        records[1].state,
        PluginState::Retired,
        "dependent plugin must retire on first-run param validation failure"
    );
    assert!(
        !records[1].gate.is_open(),
        "dependent's gate must be closed"
    );
}

#[test]
fn per_system_panic_keeps_world_alive_and_other_owners_continue() {
    struct PanicPlugin;

    impl Plugin for PanicPlugin {
        fn name(&self) -> &'static str {
            "panicker"
        }

        fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
            // Build-inserted resource; startup succeeds; the Update system
            // panics on the first frame.
            ctx.insert_resource(FirstOwnerState(11))?;
            ctx.add_update_system(panicking_update);
            Ok(())
        }
    }

    struct SurvivorPlugin;

    impl Plugin for SurvivorPlugin {
        fn name(&self) -> &'static str {
            "survivor"
        }

        fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
            ctx.insert_resource(AliveMarker::default())?;
            ctx.add_update_system(survivor_update);
            Ok(())
        }
    }

    let gate = RuntimeGate::new();
    let mut app = new_app(&gate);
    app.add_plugin(PanicPlugin);
    app.add_plugin(SurvivorPlugin);

    let token = common::fixture_main_token();
    app.run_startup(&token);
    app.run_update(&token);

    let records = app.plugins().records();
    assert_eq!(records[0].state, PluginState::Retired, "panicker retired");
    assert_eq!(records[1].state, PluginState::Active, "survivor continues");
    // Update-failure path must NOT remove resources (no implicit contract
    // break for others): Startup-inserted FirstOwnerState stays.
    assert!(
        app.world().contains_resource::<FirstOwnerState>(),
        "update failure keeps resources in the world"
    );
    // World is alive and the surviving owner kept running.
    assert_eq!(
        app.world()
            .resource::<AliveMarker>()
            .0
            .load(Ordering::Acquire),
        1
    );
    app.run_update(&token);
    assert_eq!(
        app.world()
            .resource::<AliveMarker>()
            .0
            .load(Ordering::Acquire),
        2,
        "world stays usable"
    );
}
