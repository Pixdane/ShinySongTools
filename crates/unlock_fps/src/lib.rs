#![doc = include_str!("../README.md")]

use bevy_ecs::prelude::Resource;
use corelib::debug::{DebugHandlerError, MainDebugTopic};
use corelib::hook::InstalledHook;
use corelib::{
    AppCtx, CallbackLatestReader, MainLatestWriter, Plugin, PluginError, StartupCtx, UpdateCtx,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

mod targets;
pub use targets::{MethodInfoOpaque, RATE_TARGET, SetterFn, VSYNC_TARGET};
use targets::{RATE_SITE, RateTarget, VSYNC_SITE, VsyncTarget};

pub const UNLOCKED_TARGET: i32 = 120;
pub const LOCKED_TARGET: i32 = 60;

#[derive(Clone, Copy)]
struct FpsSetting(bool);

struct FpsSites {
    setting: CallbackLatestReader<FpsSetting>,
    diagnostics: Arc<FpsDiagnostics>,
}

#[derive(Default)]
struct FpsDiagnostics {
    apply_count: AtomicU64,
    last_applied: AtomicI32,
    rate_hook_hits: AtomicU64,
    rate_last_input: AtomicI32,
    vsync_hook_hits: AtomicU64,
    vsync_last_input: AtomicI32,
}

unsafe extern "C" fn rate_replacement(value: i32, method: *const MethodInfoOpaque) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        RATE_SITE.dispatch(
            |original| unsafe { original(value, method) },
            || (),
            |callback| {
                callback
                    .container()
                    .diagnostics
                    .rate_hook_hits
                    .fetch_add(1, Ordering::AcqRel);
                callback
                    .container()
                    .diagnostics
                    .rate_last_input
                    .store(value, Ordering::Release);
                let target = callback
                    .container()
                    .setting
                    .try_read(callback.cap())
                    .map_or(
                        value,
                        |setting| {
                            if setting.0 { UNLOCKED_TARGET } else { value }
                        },
                    );
                let _ = callback.call_original(|original| unsafe { original(target, method) });
            },
        );
    }));
}

unsafe extern "C" fn vsync_replacement(value: i32, method: *const MethodInfoOpaque) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        VSYNC_SITE.dispatch(
            |original| unsafe { original(value, method) },
            || (),
            |callback| {
                callback
                    .container()
                    .diagnostics
                    .vsync_hook_hits
                    .fetch_add(1, Ordering::AcqRel);
                callback
                    .container()
                    .diagnostics
                    .vsync_last_input
                    .store(value, Ordering::Release);
                let vsync = callback
                    .container()
                    .setting
                    .try_read(callback.cap())
                    .map_or(value, |setting| if setting.0 { 0 } else { value });
                let _ = callback.call_original(|original| unsafe { original(vsync, method) });
            },
        );
    }));
}

#[derive(Resource)]
struct FpsState {
    unlock_fps: Arc<AtomicBool>,
    updates: AtomicU64,
    diagnostics: Arc<FpsDiagnostics>,
}

#[derive(Resource)]
struct FpsWriter(MainLatestWriter<FpsSetting>);

#[derive(Resource)]
struct FpsHooks {
    rate: InstalledHook<RateTarget, FpsSites>,
    vsync: InstalledHook<VsyncTarget, FpsSites>,
}

pub struct FpsGet;
impl MainDebugTopic for FpsGet {
    const NAME: &'static str = "unlock_fps.get";
    type Request = FpsGetRequest;
    type Response = FpsGetResponse;
}

#[derive(serde::Deserialize)]
pub struct FpsGetRequest {}

#[derive(serde::Serialize)]
pub struct FpsGetResponse {
    pub unlock_fps: bool,
    pub apply_count: u64,
    pub last_applied: i32,
    pub rate_hook_hits: u64,
    pub rate_last_input: i32,
    pub vsync_hook_hits: u64,
    pub vsync_last_input: i32,
}

pub struct FpsSet;
impl MainDebugTopic for FpsSet {
    const NAME: &'static str = "unlock_fps.set";
    type Request = FpsSetRequest;
    type Response = FpsSetResponse;
}

#[derive(serde::Deserialize)]
pub struct FpsSetRequest {
    pub unlock_fps: bool,
}

#[derive(serde::Serialize)]
pub struct FpsSetResponse {
    pub applied: bool,
}

/// Registers the two Unity setter hooks and the optional debug controls.
pub struct FpsPlugin;

impl Plugin for FpsPlugin {
    fn name(&self) -> &'static str {
        "unlock_fps"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        let config = ctx.config().fps;
        let unlock_fps = Arc::new(AtomicBool::new(config.unlock_fps));
        let diagnostics = Arc::new(FpsDiagnostics::default());
        ctx.insert_resource(FpsState {
            unlock_fps: Arc::clone(&unlock_fps),
            updates: AtomicU64::new(0),
            diagnostics: Arc::clone(&diagnostics),
        })?;

        let (writer, setting) = ctx.main_to_callback_latest::<FpsSetting>()?;
        ctx.insert_resource(FpsWriter(writer))?;
        let sites = ctx.register_container(FpsSites {
            setting,
            diagnostics,
        })?;

        let rate = ctx
            .hook(&RATE_SITE)
            .container(Arc::clone(&sites))
            .handler(rate_replacement as SetterFn)?
            .install()?;
        let vsync = ctx
            .hook(&VSYNC_SITE)
            .container(sites)
            .handler(vsync_replacement as SetterFn)?
            .install()?;
        ctx.insert_resource(FpsHooks { rate, vsync })?;

        ctx.register_main_debug::<fn(bevy_ecs::prelude::Res<'static, FpsState>), FpsGet, _>(
            fps_get_handler,
        )?;
        ctx.register_main_debug::<fn(
            (
                bevy_ecs::prelude::ResMut<'static, FpsState>,
                bevy_ecs::prelude::Res<'static, FpsWriter>,
                bevy_ecs::prelude::Res<'static, FpsHooks>,
            ),
        ), FpsSet, _>(fps_set_handler)?;
        ctx.add_startup_system(fps_startup);

        Ok(())
    }
}

fn fps_get_handler(
    _ctx: UpdateCtx<'_>,
    _request: FpsGetRequest,
    state: bevy_ecs::prelude::Res<FpsState>,
) -> Result<FpsGetResponse, DebugHandlerError> {
    Ok(FpsGetResponse {
        unlock_fps: state.unlock_fps.load(Ordering::Acquire),
        apply_count: state.diagnostics.apply_count.load(Ordering::Acquire),
        last_applied: state.diagnostics.last_applied.load(Ordering::Acquire),
        rate_hook_hits: state.diagnostics.rate_hook_hits.load(Ordering::Acquire),
        rate_last_input: state.diagnostics.rate_last_input.load(Ordering::Acquire),
        vsync_hook_hits: state.diagnostics.vsync_hook_hits.load(Ordering::Acquire),
        vsync_last_input: state.diagnostics.vsync_last_input.load(Ordering::Acquire),
    })
}

fn fps_startup(
    ctx: StartupCtx<'_>,
    params: (
        bevy_ecs::prelude::Res<FpsState>,
        bevy_ecs::prelude::Res<FpsHooks>,
    ),
) -> Result<(), PluginError> {
    let (state, hooks) = params;
    let unlock_fps = state.unlock_fps.load(Ordering::Acquire);
    apply_setting(ctx.main, &hooks, &state, unlock_fps)
        .map_err(|_| PluginError::Message("unlock_fps startup apply failed"))
}

fn apply_setting(
    main: &corelib::MainThreadToken,
    hooks: &FpsHooks,
    state: &FpsState,
    unlock_fps: bool,
) -> Result<(), corelib::HookError> {
    let target = if unlock_fps {
        UNLOCKED_TARGET
    } else {
        LOCKED_TARGET
    };
    let vsync = if unlock_fps { 0 } else { 1 };
    hooks
        .vsync
        .call_original_on_main(main, |original, method| {
            // SAFETY: the installed target declares the static setter ABI and
            // the MethodInfo address was captured from that same target.
            unsafe { original(vsync, method as *const MethodInfoOpaque) };
        })?;
    hooks.rate.call_original_on_main(main, |original, method| {
        // SAFETY: same target-bound ABI and MethodInfo contract as above.
        unsafe { original(target, method as *const MethodInfoOpaque) };
    })?;
    state
        .diagnostics
        .last_applied
        .store(target, Ordering::Release);
    state.diagnostics.apply_count.fetch_add(1, Ordering::AcqRel);
    Ok(())
}

fn fps_set_handler(
    ctx: UpdateCtx<'_>,
    request: FpsSetRequest,
    params: (
        bevy_ecs::prelude::ResMut<FpsState>,
        bevy_ecs::prelude::Res<FpsWriter>,
        bevy_ecs::prelude::Res<FpsHooks>,
    ),
) -> Result<FpsSetResponse, DebugHandlerError> {
    let (state, writer, hooks) = params;
    state
        .unlock_fps
        .store(request.unlock_fps, Ordering::Release);
    state.updates.fetch_add(1, Ordering::AcqRel);
    let _ = writer.0.try_send(&ctx, FpsSetting(request.unlock_fps));
    apply_setting(ctx.main, &hooks, &state, request.unlock_fps)
        .map_err(|_| DebugHandlerError("unlock_fps apply failed".to_owned()))?;
    Ok(FpsSetResponse { applied: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_targets_are_stable_and_distinct() {
        assert_eq!(RATE_TARGET.assembly, "UnityEngine.CoreModule.dll");
        assert_eq!(RATE_TARGET.class, "Application");
        assert_eq!(RATE_TARGET.method, "set_targetFrameRate");
        assert_eq!(VSYNC_TARGET.class, "QualitySettings");
        assert_eq!(VSYNC_TARGET.method, "set_vSyncCount");
        assert_ne!(RATE_TARGET, VSYNC_TARGET);
    }

    #[test]
    fn setter_abi_is_the_declared_static_shape() {
        assert_eq!(
            core::mem::size_of::<SetterFn>(),
            core::mem::size_of::<usize>()
        );
        assert_eq!(core::mem::size_of::<MethodInfoOpaque>(), 0);
    }
}
