//! Production Unity FPS control plugin.
//!
//! The plugin owns one static hook site per Unity setter. Resolution is by
//! [`TargetId`] identity through the runtime's method resolver; no process
//! address is hard-coded here. Main-domain `fps.get`/`fps.set` topics update
//! an atomic target and a latest-value route consumed by the callback domain.

use bevy_ecs::prelude::Resource;
use corelib::TargetId;
use plugins::debug::{DebugHandlerError, MainDebugTopic};
use plugins::hook::HookTarget;
use plugins::{AppCtx, CallbackLatestReader, MainLatestWriter, Plugin, PluginError, UpdateCtx};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

/// Unity's static `Application.set_targetFrameRate(int)` target.
pub const RATE_TARGET: TargetId = TargetId {
    assembly: "UnityEngine.CoreModule.dll",
    namespace: "UnityEngine",
    class: "Application",
    method: "set_targetFrameRate",
    param_count: 1,
};

/// Unity's static `QualitySettings.set_vSyncCount(int)` target.
pub const VSYNC_TARGET: TargetId = TargetId {
    assembly: "UnityEngine.CoreModule.dll",
    namespace: "UnityEngine",
    class: "QualitySettings",
    method: "set_vSyncCount",
    param_count: 1,
};

/// Opaque IL2CPP `MethodInfo` trailing parameter for static setters.
#[repr(C)]
pub struct MethodInfoOpaque {
    _private: [u8; 0],
}

/// Static Unity setter ABI: `(int value, MethodInfo*) -> void`.
pub type SetterFn = unsafe extern "C" fn(i32, *const MethodInfoOpaque);

#[derive(Clone, Copy)]
struct FpsSetting(i32);

struct FpsSites {
    setting: CallbackLatestReader<FpsSetting>,
    target: Arc<AtomicI32>,
}

struct RateTarget;
impl HookTarget for RateTarget {
    const TARGET: TargetId = RATE_TARGET;
    type Original = SetterFn;

    fn replacement_addr(_: Self::Original) -> usize {
        rate_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: resolver validation binds this slot to the declared Unity
        // setter target and the ABI is the author-owned production contract.
        unsafe { core::mem::transmute(addr) }
    }
}

struct VsyncTarget;
impl HookTarget for VsyncTarget {
    const TARGET: TargetId = VSYNC_TARGET;
    type Original = SetterFn;

    fn replacement_addr(_: Self::Original) -> usize {
        vsync_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: resolver validation binds this slot to the declared Unity
        // setter target and the ABI is the author-owned production contract.
        unsafe { core::mem::transmute(addr) }
    }
}

plugins::define_hook_site!(RATE_SITE: HookSite<RateTarget, FpsSites>);
plugins::define_hook_site!(VSYNC_SITE: HookSite<VsyncTarget, FpsSites>);

unsafe extern "C" fn rate_replacement(value: i32, method: *const MethodInfoOpaque) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        RATE_SITE.dispatch(
            |original| unsafe { original(value, method) },
            || (),
            |callback| {
                let target = callback
                    .container()
                    .setting
                    .try_read(callback.cap())
                    .map_or_else(
                        || callback.container().target.load(Ordering::Acquire),
                        |setting| setting.0,
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
                // Any non-zero v-sync count caps the effective frame rate.
                let _ = callback.call_original(|original| unsafe { original(0, method) });
            },
        );
    }));
}

#[derive(Resource)]
struct FpsState {
    target: Arc<AtomicI32>,
    updates: AtomicU64,
}

#[derive(Resource)]
struct FpsWriter(MainLatestWriter<FpsSetting>);

pub struct FpsGet;
impl MainDebugTopic for FpsGet {
    const NAME: &'static str = "fps.get";
    type Request = FpsGetRequest;
    type Response = FpsGetResponse;
}

#[derive(serde::Deserialize)]
pub struct FpsGetRequest {}

#[derive(serde::Serialize)]
pub struct FpsGetResponse {
    pub target: i32,
}

pub struct FpsSet;
impl MainDebugTopic for FpsSet {
    const NAME: &'static str = "fps.set";
    type Request = FpsSetRequest;
    type Response = FpsSetResponse;
}

#[derive(serde::Deserialize)]
pub struct FpsSetRequest {
    pub target: i32,
}

#[derive(serde::Serialize)]
pub struct FpsSetResponse {
    pub applied: bool,
}

/// Registers the two Unity setter hooks and the optional debug controls.
pub struct FpsPlugin;

impl Plugin for FpsPlugin {
    fn name(&self) -> &'static str {
        "fps"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        let config = ctx.config().fps;
        let target = Arc::new(AtomicI32::new(config.target));
        ctx.insert_resource(FpsState {
            target: Arc::clone(&target),
            updates: AtomicU64::new(0),
        })?;

        let (writer, setting) = ctx.main_to_callback_latest::<FpsSetting>()?;
        ctx.insert_resource(FpsWriter(writer))?;
        let sites = ctx.register_container(FpsSites { setting, target })?;

        ctx.hook(&RATE_SITE)
            .container(Arc::clone(&sites))
            .handler(rate_replacement as SetterFn)?
            .install()?;
        ctx.hook(&VSYNC_SITE)
            .container(sites)
            .handler(vsync_replacement as SetterFn)?
            .install()?;

        ctx.register_main_debug::<fn(bevy_ecs::prelude::Res<'static, FpsState>), FpsGet, _>(
            fps_get_handler,
        )?;
        ctx.register_main_debug::<fn(
            (
                bevy_ecs::prelude::ResMut<'static, FpsState>,
                bevy_ecs::prelude::Res<'static, FpsWriter>,
            ),
        ), FpsSet, _>(fps_set_handler)?;

        Ok(())
    }
}

fn fps_get_handler(
    _ctx: UpdateCtx<'_>,
    _request: FpsGetRequest,
    state: bevy_ecs::prelude::Res<FpsState>,
) -> Result<FpsGetResponse, DebugHandlerError> {
    Ok(FpsGetResponse {
        target: state.target.load(Ordering::Acquire),
    })
}

fn fps_set_handler(
    ctx: UpdateCtx<'_>,
    request: FpsSetRequest,
    params: (
        bevy_ecs::prelude::ResMut<FpsState>,
        bevy_ecs::prelude::Res<FpsWriter>,
    ),
) -> Result<FpsSetResponse, DebugHandlerError> {
    if !(1..=1000).contains(&request.target) {
        return Err(DebugHandlerError(
            "target must be between 1 and 1000".to_owned(),
        ));
    }
    let (state, writer) = params;
    state.target.store(request.target, Ordering::Release);
    state.updates.fetch_add(1, Ordering::AcqRel);
    let _ = writer.0.try_send(&ctx, FpsSetting(request.target));
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
