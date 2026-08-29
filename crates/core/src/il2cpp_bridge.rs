//! Production IL2CPP backend over `il2cpp-bridge-rs 0.1.4`.
//!
//! This module is the thin adapter the design pins: it keeps the exact
//! UnityFramework handle alive, feeds an exact-handle resolver into
//! `il2cpp_bridge_rs::api::load` (never `RTLD_DEFAULT`), drives the
//! readiness ladder operations ([`Il2CppApi`]), and resolves hook targets
//! through the crate's cache-backed metadata queries ([`MethodResolver`]).
//! It implements nothing a second time: no export table of its own, no
//! high-level initializer, no alternate symbol lookup.
//!
//! Two experiment-validated constraints are upheld here:
//!
//! * the ladder-4 `il2cpp_domain_get` **probe** runs exactly once (never
//!   polled); attach is driven through the raw `thread_*` exports with the
//!   domain captured at that probe, so attach adds no extra call. The
//!   bridge crate's cache hydration still re-reads `domain_get` internally
//!   at ladder 5 — post-gate re-reads are empirically safe (two live A/B
//!   runs) and pinned by the `bridge_fake_happy` fixture.
//! * Attachments are only ever detached by the RAII guard that created them.

use crate::backend::{
    AttachGuard, DomainHandle, Il2CppApi, ImageHandle, ImageIdentity, MethodResolver,
    RuntimeIdentity,
};
use crate::error::{HookError, Il2CppError};
use crate::method_slot::{MethodRef, RawSlotMemory, SlotMemory, TargetId};
use il2cpp_bridge_rs::api;
use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Ladder 1 enumeration: walk the process image list and return the path of
/// the loaded UnityFramework. Identity matching beyond the file name is a
/// bootstrap parameter; global visibility must never be assumed.
#[must_use]
#[allow(
    deprecated,
    reason = "dyld introspection is the v1 platform seam; the mach2 migration is a bootstrap parameter"
)]
pub fn enumerate_unity_framework() -> Option<PathBuf> {
    // SAFETY: dyld image introspection; the count and name pointers are
    // owned by the dynamic loader and valid for the process lifetime.
    let count = unsafe { libc::_dyld_image_count() };
    for index in 0..count {
        // SAFETY: index is within the image count.
        let name = unsafe { libc::_dyld_get_image_name(index) };
        if name.is_null() {
            continue;
        }
        // SAFETY: valid NUL-terminated loader string.
        let text = unsafe { std::ffi::CStr::from_ptr(name) };
        let path = PathBuf::from(text.to_string_lossy().into_owned());
        if path.file_name().is_some_and(|n| n == "UnityFramework") {
            return Some(path);
        }
    }
    None
}

/// The `methodPointer` field is the first field of `MethodInfo` in the
/// validated layout; the slot address equals the `MethodInfo` address.
/// Target drift to a different layout is rejected at ladder 6 and by
/// per-target validation, never silently tolerated here.
const METHOD_POINTER_OFFSET: usize = 0;

/// Raw thread pointer wrapper so the detach closure can cross threads.
/// The pointer is only ever passed to `il2cpp_thread_detach`.
struct DetachTarget(*mut c_void);

impl DetachTarget {
    fn detach(self) {
        // SAFETY: this thread pointer came from `thread_attach` in this
        // process and belongs to the attachment this guard owns.
        unsafe { api::thread_detach(self.0) };
    }
}

// The pointer is an opaque IL2CPP thread handle; handing it to the detach
// closure is the whole point of the guard.
unsafe impl Send for DetachTarget {}

/// Handle to one exact loaded image (the UnityFramework), kept alive for the
/// process lifetime. Symbol lookups through this handle never fall back to
/// global visibility: `dlsym` is scoped to the handle and every hit is
/// checked with `dladdr` to belong to this exact image.
pub struct ExactHandle {
    raw: *mut c_void,
    path: PathBuf,
}

// The raw handle is an opaque reference into the dynamic loader's tables;
// dlsym/dladdr are thread-safe, and the image stays mapped for the process.
unsafe impl Send for ExactHandle {}
unsafe impl Sync for ExactHandle {}

impl ExactHandle {
    /// Open `path` with `RTLD_LOCAL` (PlayCover loads UnityFramework that
    /// way; global visibility must not be assumed). An already-loaded image
    /// is adopted via `RTLD_NOLOAD` rather than reloaded.
    ///
    /// # Safety
    ///
    /// `path` must identify a loadable mach-o image. Loading is a process
    /// side effect the runtime bootstrap performs after image enumeration.
    pub unsafe fn open(path: &Path) -> Result<Self, Il2CppError> {
        let c_path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| Il2CppError::ImageIdentityMismatch)?;
        // SAFETY: caller guarantees a valid loadable image path.
        let raw = unsafe {
            libc::dlopen(
                c_path.as_ptr(),
                libc::RTLD_LAZY | libc::RTLD_LOCAL | libc::RTLD_NOLOAD,
            )
        };
        let raw = if raw.is_null() {
            // SAFETY: same path validity contract as above.
            unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) }
        } else {
            raw
        };
        if raw.is_null() {
            return Err(Il2CppError::ImageNotFound);
        }
        Ok(Self {
            raw,
            path: path.to_path_buf(),
        })
    }

    /// Construct from an already-loaded handle (from platform image
    /// enumeration). The image stays mapped for the process lifetime; no
    /// `dlclose` is ever issued.
    ///
    /// # Safety
    ///
    /// `raw` must be a valid dlopen handle that remains mapped.
    pub unsafe fn from_raw(raw: *mut c_void, path: PathBuf) -> Self {
        Self { raw, path }
    }

    /// File path this handle was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Identity facts of the exact image.
    #[must_use]
    pub fn identity(&self) -> ImageIdentity {
        ImageIdentity {
            name: self
                .path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            handle: ImageHandle(self.raw as usize),
        }
    }

    /// Resolve one export, requiring it to belong to this exact image.
    #[must_use]
    pub fn symbol(&self, name: &str) -> Option<usize> {
        let c_name = CString::new(name).ok()?;
        // SAFETY: `self.raw` is a live dlopen handle; c_name outlives the call.
        let address = unsafe { libc::dlsym(self.raw, c_name.as_ptr()) };
        if address.is_null() {
            return None;
        }
        let owner = image_owner(address as usize)?;
        (owner == self.path).then_some(address as usize)
    }
}

impl core::fmt::Debug for ExactHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ExactHandle")
            .field("path", &self.path)
            .field("handle", &(self.raw as usize))
            .finish()
    }
}

/// Image path that owns `address`, via `dladdr`. `None` when the address is
/// not inside any loaded image.
fn image_owner(address: usize) -> Option<PathBuf> {
    let mut info = libc::Dl_info {
        dli_fname: core::ptr::null(),
        dli_fbase: core::ptr::null_mut(),
        dli_sname: core::ptr::null(),
        dli_saddr: core::ptr::null_mut(),
    };
    // SAFETY: `address` may be any value; dladdr reports failure via 0.
    if unsafe { libc::dladdr(address as *const c_void, &mut info) } == 0 {
        return None;
    }
    let fname = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) };
    Some(PathBuf::from(fname.to_string_lossy().into_owned()))
}

/// Production backend: exact handle + bridge-crate API table.
///
/// `domain` stores the single `il2cpp_domain_get` result so ladder 5 attach
/// never has to call it again.
pub struct BridgeBackend {
    handle: Arc<ExactHandle>,
    domain: std::sync::atomic::AtomicUsize,
}

impl BridgeBackend {
    #[must_use]
    pub fn new(handle: Arc<ExactHandle>) -> Self {
        Self {
            handle,
            domain: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// The exact handle is shared with the scheduler-side callback backend.
    #[must_use]
    pub fn handle(&self) -> &Arc<ExactHandle> {
        &self.handle
    }

    /// The captured domain handle, once ladder 3 has run.
    #[must_use]
    pub fn domain(&self) -> Option<DomainHandle> {
        let value = self.domain.load(std::sync::atomic::Ordering::Acquire);
        (value != 0).then_some(DomainHandle(value))
    }
}

impl core::fmt::Debug for BridgeBackend {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BridgeBackend")
            .field("handle", &self.handle)
            .field("domain_captured", &self.domain().is_some())
            .finish()
    }
}

impl Il2CppApi for BridgeBackend {
    fn unity_framework_image(&self) -> Result<ImageIdentity, Il2CppError> {
        Ok(self.handle.identity())
    }

    fn load_exports(&self) -> Result<(), Il2CppError> {
        api::load(|name| {
            self.handle
                .symbol(name)
                .map_or(core::ptr::null_mut(), |a| a as *mut c_void)
        })
        .map(|_| ())
        .map_err(|missing| {
            missing
                .into_iter()
                .next()
                .map_or(Il2CppError::NotReady, Il2CppError::ExportMissing)
        })
    }

    fn domain_get(&self) -> Result<DomainHandle, Il2CppError> {
        // Ladder 3 probe: the bootstrap calls this exactly once; repeated
        // calls return the captured value without touching IL2CPP. Null
        // terminates the one-shot bootstrap.
        let captured = self.domain();
        if let Some(domain) = captured {
            return Ok(domain);
        }
        // SAFETY: exports were loaded by `load_exports` (ladder ordering is
        // enforced by the bootstrap); the wrapper is a plain C call.
        let domain = unsafe { api::domain_get() };
        if domain.is_null() {
            return Err(Il2CppError::DomainUnavailable);
        }
        self.domain
            .store(domain as usize, std::sync::atomic::Ordering::Release);
        Ok(DomainHandle(domain as usize))
    }

    fn attach_current_thread(&self) -> Result<AttachGuard, Il2CppError> {
        // Ladder 5: attach with the domain captured at ladder 4, never by
        // re-calling domain_get. An existing attachment (made by external
        // code) is adopted without owning it: the guard does not detach.
        let domain = self.domain().ok_or(Il2CppError::NotReady)?.0 as *mut c_void;
        // SAFETY: exports loaded; ladder ordering enforced by bootstrap.
        if !unsafe { api::thread_current() }.is_null() {
            return Ok(AttachGuard::new(|| {}));
        }
        // SAFETY: exports loaded; domain captured at ladder 3.
        let thread = unsafe { api::thread_attach(domain) };
        if thread.is_null() {
            return Err(Il2CppError::AttachFailed);
        }
        // Raw pointer wrapper so the detach closure can cross threads; the
        // pointer is only dereferenced by the guard's detach closure, which
        // runs on the thread that drops the guard.
        let detach_target = DetachTarget(thread);
        Ok(AttachGuard::new(move || detach_target.detach()))
    }

    fn hydrate_metadata(&self) -> Result<(), Il2CppError> {
        // Ladder 5 (post-attach): full cache hydration (~seconds on live
        // metadata; a normal bootstrap-worker workload).
        if api::cache::init() {
            Ok(())
        } else {
            Err(Il2CppError::NotReady)
        }
    }

    fn runtime_identity(&self) -> Result<RuntimeIdentity, Il2CppError> {
        // Ladder 6: structural identity facts visible through the loaded
        // exports and cache. Exact version-string matching against the
        // supported set is a bootstrap parameter and stays in the bootstrap.
        let corlib = api::cache::assembly("mscorlib").ok_or(Il2CppError::IdentityMismatch)?;
        Ok(RuntimeIdentity {
            unity_version: corlib.name.clone(),
            il2cpp_variant: "il2cpp-bridge-rs/0.1.4".to_owned(),
        })
    }
}

impl MethodResolver for BridgeBackend {
    fn resolve(&self, target: &TargetId) -> Result<MethodRef, HookError> {
        // Read-only cache queries against live, hydrated metadata. The
        // class lookup accepts the fully qualified "Namespace.Class" form.
        let assembly = api::cache::assembly(target.assembly).ok_or(HookError::TargetUnavailable)?;
        let full_name = format!("{}.{}", target.namespace, target.class);
        let class = assembly
            .class(&full_name)
            .ok_or(HookError::TargetUnavailable)?;
        // Fail closed on ambiguity: two overloads with the same name and
        // parameter count are target drift, not a resolvable target.
        let matching = class
            .methods
            .iter()
            .filter(|m| m.name == target.method && m.args.len() == target.param_count as usize)
            .count();
        if matching != 1 {
            return Err(HookError::SignatureMismatch);
        }
        let method = class
            .method((target.method, target.param_count as usize))
            .ok_or(HookError::SignatureMismatch)?;
        let method_info = method.address as usize;
        if method_info == 0 {
            return Err(HookError::TargetUnavailable);
        }
        Ok(MethodRef {
            assembly: target.assembly.to_owned(),
            namespace: target.namespace.to_owned(),
            class: target.class.to_owned(),
            method: target.method.to_owned(),
            param_count: target.param_count,
            method_info,
            method_pointer_slot: method_info.wrapping_add(METHOD_POINTER_OFFSET),
        })
    }

    fn slot_memory(&self, method: &MethodRef) -> Arc<dyn SlotMemory> {
        // SAFETY: the slot is the first field of a live MethodInfo held by
        // the process for its lifetime; alignment/liveness is re-verified by
        // `MethodPointerSlot::bind`.
        unsafe { Arc::new(RawSlotMemory::from_addr(method.method_pointer_slot)) }
    }
}
