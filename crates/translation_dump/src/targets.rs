//! Localization target identity and native ABI binding.

use crate::DumpSites;
use corelib::HookMechanism;
use corelib::TargetId;
use corelib::hook::HookTarget;
use std::ffi::c_void;

/// `ENTERPRISE.Localization.LocalizationManager.GetTextOrNull(string, int)`.
///
/// The identity matches scsp-localify's pinned localization hook; the
/// complete managed signature is validated before the hook installation.
/// Invoked by ordinary game code through AOT direct calls, so the hook uses
/// the entry-patch mechanism: the MethodPointer slot swap is only visible to
/// slot-dispatched callers and never fired on the live game.
pub const LOCALIZATION_GET_TEXT_OR_NULL_TARGET: TargetId = TargetId {
    assembly: "PRISM.Legacy.dll",
    namespace: "ENTERPRISE.Localization",
    class: "LocalizationManager",
    method: "GetTextOrNull",
    param_count: 2,
    is_static: false,
    return_type: "string",
    parameter_types: &["string", "int"],
};

/// `ENTERPRISE.Localization.LocalizationManager.GetText(string, int)`.
///
/// Runtime recon (2026-08-30) showed 896 direct call sites against this
/// overload versus 10 for `GetTextOrNull` — it is the live UI text path on
/// the iOS build. Same ABI family as `GetTextOrNull`; if a game update
/// changes the parameter types, validation fails closed and the hook is
/// skipped by the plugin.
pub const LOCALIZATION_GET_TEXT_TARGET: TargetId = TargetId {
    assembly: "PRISM.Legacy.dll",
    namespace: "ENTERPRISE.Localization",
    class: "LocalizationManager",
    method: "GetText",
    param_count: 2,
    is_static: false,
    return_type: "string",
    parameter_types: &["string", "int"],
};

#[repr(C)]
pub struct Il2CppStringOpaque {
    _private: [u8; 0],
}

#[repr(C)]
pub struct MethodInfoOpaque {
    _private: [u8; 0],
}

pub type GetTextOrNullFn = unsafe extern "C" fn(
    *mut c_void,
    *mut Il2CppStringOpaque,
    i32,
    *const MethodInfoOpaque,
) -> *mut Il2CppStringOpaque;

pub struct GetTextOrNullTarget;

impl HookTarget for GetTextOrNullTarget {
    const TARGET: TargetId = LOCALIZATION_GET_TEXT_OR_NULL_TARGET;
    const MECHANISM: HookMechanism = HookMechanism::EntryPatch;
    type Original = GetTextOrNullFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::get_text_or_null_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: HookTarget validation proves the managed signature before
        // the framework captures this target's MethodPointer.
        unsafe { core::mem::transmute(addr) }
    }
}

corelib::define_hook_site!(
    GET_TEXT_OR_NULL_SITE: HookSite<GetTextOrNullTarget, DumpSites>
);

pub struct GetTextTarget;

impl HookTarget for GetTextTarget {
    const TARGET: TargetId = LOCALIZATION_GET_TEXT_TARGET;
    const MECHANISM: HookMechanism = HookMechanism::EntryPatch;
    type Original = GetTextOrNullFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::get_text_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: HookTarget validation proves the managed signature before
        // the framework captures this target's MethodPointer.
        unsafe { core::mem::transmute(addr) }
    }
}

corelib::define_hook_site!(GET_TEXT_SITE: HookSite<GetTextTarget, DumpSites>);

/// `PRISM.DataFile.GetBytes(string)` — static data-file reader: every class-3
/// JSON (scenario text, master data) flows through here.
pub const DATA_FILE_GET_BYTES_TARGET: TargetId = TargetId {
    assembly: "PRISM.Legacy.dll",
    namespace: "PRISM",
    class: "DataFile",
    method: "GetBytes",
    param_count: 1,
    is_static: true,
    return_type: "byte[]",
    parameter_types: &["string"],
};

/// `PRISM.Interactions.Live.LiveMVOverlayView.UpdateLyrics(string)` — MV
/// playback lyric line display.
pub const LIVE_MV_UPDATE_LYRICS_TARGET: TargetId = TargetId {
    assembly: "PRISM.Interactions.Live.dll",
    namespace: "PRISM.Interactions.Live",
    class: "LiveMVOverlayView",
    method: "UpdateLyrics",
    param_count: 1,
    is_static: false,
    return_type: "void",
    parameter_types: &["string"],
};

/// `PRISM.TimelineController.SetLyric(string)` — timeline lyric line display.
pub const TIMELINE_SET_LYRIC_TARGET: TargetId = TargetId {
    assembly: "PRISM.Legacy.dll",
    namespace: "PRISM",
    class: "TimelineController",
    method: "SetLyric",
    param_count: 1,
    is_static: false,
    return_type: "void",
    parameter_types: &["string"],
};

pub type GetBytesFn =
    unsafe extern "C" fn(*mut Il2CppStringOpaque, *const MethodInfoOpaque) -> *mut c_void;

pub type LyricsFn =
    unsafe extern "C" fn(*mut c_void, *mut Il2CppStringOpaque, *const MethodInfoOpaque);

pub struct DataFileGetBytesTarget;

impl HookTarget for DataFileGetBytesTarget {
    const TARGET: TargetId = DATA_FILE_GET_BYTES_TARGET;
    const MECHANISM: HookMechanism = HookMechanism::EntryPatch;
    type Original = GetBytesFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::get_bytes_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: HookTarget validation proves the managed signature before
        // the framework captures this target's MethodPointer.
        unsafe { core::mem::transmute(addr) }
    }
}

pub struct LiveMvUpdateLyricsTarget;

impl HookTarget for LiveMvUpdateLyricsTarget {
    const TARGET: TargetId = LIVE_MV_UPDATE_LYRICS_TARGET;
    const MECHANISM: HookMechanism = HookMechanism::EntryPatch;
    type Original = LyricsFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::update_lyrics_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: HookTarget validation proves the managed signature before
        // the framework captures this target's MethodPointer.
        unsafe { core::mem::transmute(addr) }
    }
}

pub struct TimelineSetLyricTarget;

impl HookTarget for TimelineSetLyricTarget {
    const TARGET: TargetId = TIMELINE_SET_LYRIC_TARGET;
    const MECHANISM: HookMechanism = HookMechanism::EntryPatch;
    type Original = LyricsFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::set_lyric_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: HookTarget validation proves the managed signature before
        // the framework captures this target's MethodPointer.
        unsafe { core::mem::transmute(addr) }
    }
}

corelib::define_hook_site!(GET_BYTES_SITE: HookSite<DataFileGetBytesTarget, DumpSites>);
corelib::define_hook_site!(UPDATE_LYRICS_SITE: HookSite<LiveMvUpdateLyricsTarget, DumpSites>);
corelib::define_hook_site!(SET_LYRIC_SITE: HookSite<TimelineSetLyricTarget, DumpSites>);

/// `TMPro.TMP_Text.set_text(string)` — catch-all display-text capture: every
/// TextMeshPro string (including lyric lines) flows through here. High
/// volume; main side deduplicates by text.
pub const TMP_TEXT_SET_TEXT_TARGET: TargetId = TargetId {
    assembly: "Unity.TextMeshPro.dll",
    namespace: "TMPro",
    class: "TMP_Text",
    method: "set_text",
    param_count: 1,
    is_static: false,
    return_type: "void",
    parameter_types: &["string"],
};

pub struct TmpTextSetTextTarget;

impl HookTarget for TmpTextSetTextTarget {
    const TARGET: TargetId = TMP_TEXT_SET_TEXT_TARGET;
    const MECHANISM: HookMechanism = HookMechanism::EntryPatch;
    type Original = LyricsFn;

    fn replacement_addr(_: Self::Original) -> usize {
        crate::tmp_text_replacement as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: HookTarget validation proves the managed signature before
        // the framework captures this target's MethodPointer.
        unsafe { core::mem::transmute(addr) }
    }
}

corelib::define_hook_site!(TMP_TEXT_SITE: HookSite<TmpTextSetTextTarget, DumpSites>);
