//! Unity target identities and ABI bindings for the FPS control layer.

use crate::FpsSites;
use corelib::TargetId;
#[allow(unused_imports)]
use corelib::hook::{HookSite, HookTarget};

/// Unity static `Application.set_targetFrameRate(int)` target.
pub const RATE_TARGET: TargetId = TargetId {
    assembly: "UnityEngine.CoreModule.dll",
    namespace: "UnityEngine",
    class: "Application",
    method: "set_targetFrameRate",
    param_count: 1,
};

/// Unity static `QualitySettings.set_vSyncCount(int)` target.
pub const VSYNC_TARGET: TargetId = TargetId {
    assembly: "UnityEngine.CoreModule.dll",
    namespace: "UnityEngine",
    class: "QualitySettings",
    method: "set_vSyncCount",
    param_count: 1,
};

#[repr(C)]
pub struct MethodInfoOpaque {
    _private: [u8; 0],
}

pub type SetterFn = unsafe extern "C" fn(i32, *const MethodInfoOpaque);

pub struct RateTarget;
impl HookTarget for RateTarget {
    const TARGET: TargetId = RATE_TARGET;
    type Original = SetterFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::rate_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        unsafe { core::mem::transmute(addr) }
    }
}

pub struct VsyncTarget;
impl HookTarget for VsyncTarget {
    const TARGET: TargetId = VSYNC_TARGET;
    type Original = SetterFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::vsync_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        unsafe { core::mem::transmute(addr) }
    }
}

corelib::define_hook_site!(RATE_SITE: HookSite<RateTarget, FpsSites>);
corelib::define_hook_site!(VSYNC_SITE: HookSite<VsyncTarget, FpsSites>);
