//! No-game fake UnityFramework for `scsp-core`'s bridge adapter fixtures.
//!
//! Ported from the scsp-playcover-hook experiment `il2cpp-bridge-rs-usage`
//! (fake-runtime crate) and extended for the production bootstrap: a second
//! `mscorlib` image so ladder 6's `cache::assembly("mscorlib")` succeeds, and
//! an `il2cpp_domain_get` call counter so fixtures can document the real
//! post-gate call pattern.
//!
//! Behavior knobs (read at call time from the process environment):
//! * `SCSP_FAKE_CACHE_FAIL`  — `domain_get_assemblies` returns null (cache
//!   hydration fails);
//! * `SCSP_FAKE_ATTACH_FAIL` — `il2cpp_thread_attach` returns null;
//! * `SCSP_FAKE_TARGET_DRIFT` — the resolved method is named `Other`.
//!
//! Introspection exports (not part of the IL2CPP surface):
//! * `scsp_fixture_domain_get_count` — how many times the process really
//!   called `il2cpp_domain_get`;
//! * `scsp_fixture_detach_count` — how many attachments were detached.
//! * `scsp_fixture_criware_ready_count` — how many times the production
//!   CRIWARE completion predicate was called.

// The exported functions are a C ABI surface (IL2CPP stand-ins), not a Rust
// API; per-function `# Safety` docs would be noise on a fake.
#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

include!(concat!(env!("OUT_DIR"), "/required_stubs.rs"));

// Opaque stand-in objects. Distinct statics give distinct pointer identities
// without any real structure behind them.
static DOMAIN: u8 = 1;
static UNIRX_ASSEMBLY: u8 = 2;
static MSCORLIB_ASSEMBLY: u8 = 3;
static UNIRX_IMAGE: u8 = 4;
static MSCORLIB_IMAGE: u8 = 5;
static CLASS: u8 = 6;
static RETURN_TYPE: u8 = 7;
static THREAD: u8 = 8;
static ATTACHED: AtomicBool = AtomicBool::new(false);
static DETACH_COUNT: AtomicUsize = AtomicUsize::new(0);
static DOMAIN_GET_COUNT: AtomicUsize = AtomicUsize::new(0);
static CRIWARE_READY_COUNT: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
struct FakeMethod {
    method_pointer: *mut c_void,
}

unsafe extern "C" fn original_late_update(_: *mut c_void, _: *const c_void) {}

/// The method info lives on the heap, NOT in a static: Rust places
/// pointer-bearing statics in `__DATA_CONST`, which is read-only after
/// relocation — a MethodPointerSlot CAS write there faults (SIGBUS). Real
/// IL2CPP MethodInfo structures live in writable `__DATA`.
fn method_info() -> *mut c_void {
    static METHOD: OnceLock<usize> = OnceLock::new();
    *METHOD.get_or_init(|| {
        Box::into_raw(Box::new(FakeMethod {
            method_pointer: original_late_update as *mut c_void,
        }))
        .cast::<c_void>() as usize
    }) as *mut c_void
}

const ASSEMBLY_TABLE: [SyncPointer; 2] = [
    SyncPointer(&UNIRX_ASSEMBLY as *const u8 as *mut c_void),
    SyncPointer(&MSCORLIB_ASSEMBLY as *const u8 as *mut c_void),
];

#[repr(transparent)]
struct SyncPointer(*mut c_void);
unsafe impl Sync for SyncPointer {}

fn opaque(value: &'static u8) -> *mut c_void {
    value as *const u8 as *mut c_void
}

fn env_enabled(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

fn is_mscorlib(assembly: *mut c_void) -> bool {
    assembly == opaque(&MSCORLIB_ASSEMBLY)
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_domain_get() -> *mut c_void {
    DOMAIN_GET_COUNT.fetch_add(1, Ordering::AcqRel);
    opaque(&DOMAIN)
}

#[unsafe(no_mangle)]
pub extern "C" fn scsp_fixture_domain_get_count() -> usize {
    DOMAIN_GET_COUNT.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub extern "C" fn scsp_fixture_detach_count() -> usize {
    DETACH_COUNT.load(Ordering::Acquire)
}

/// Stand-in for the statically validated CRIWARE Unity completion export.
#[unsafe(export_name = "CRIWARE2813B966")]
pub extern "C" fn criware_unity_ready() -> i32 {
    CRIWARE_READY_COUNT.fetch_add(1, Ordering::AcqRel);
    i32::from(!env_enabled("SCSP_FAKE_CRIWARE_NOT_READY"))
}

#[unsafe(no_mangle)]
pub extern "C" fn scsp_fixture_criware_ready_count() -> usize {
    CRIWARE_READY_COUNT.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn il2cpp_domain_get_assemblies(
    _: *mut c_void,
    size: *mut usize,
) -> *mut *mut c_void {
    if env_enabled("SCSP_FAKE_CACHE_FAIL") {
        return std::ptr::null_mut();
    }
    if !size.is_null() {
        // SAFETY: caller contract — writable usize out-parameter.
        unsafe { *size = ASSEMBLY_TABLE.len() };
    }
    ASSEMBLY_TABLE.as_ptr().cast::<*mut c_void>() as *mut *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_assembly_get_image(assembly: *mut c_void) -> *mut c_void {
    if is_mscorlib(assembly) {
        opaque(&MSCORLIB_IMAGE)
    } else {
        opaque(&UNIRX_IMAGE)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_image_get_name(image: *mut c_void) -> *const c_char {
    if image == opaque(&MSCORLIB_IMAGE) {
        c"mscorlib".as_ptr()
    } else {
        c"UniRx.dll".as_ptr()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_image_get_filename(image: *mut c_void) -> *const c_char {
    il2cpp_image_get_name(image)
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_image_get_entry_point(_: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_image_get_class_count(image: *mut c_void) -> u32 {
    u32::from(image != opaque(&MSCORLIB_IMAGE))
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_image_get_class(image: *mut c_void, index: u32) -> *mut c_void {
    if image != opaque(&MSCORLIB_IMAGE) && index == 0 {
        opaque(&CLASS)
    } else {
        std::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn il2cpp_class_from_name(
    image: *mut c_void,
    namespace: *const c_char,
    name: *const c_char,
) -> *mut c_void {
    if image == opaque(&MSCORLIB_IMAGE) || namespace.is_null() || name.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: caller contract — valid NUL-terminated strings.
    let namespace = unsafe { CStr::from_ptr(namespace) }.to_bytes();
    // SAFETY: caller contract — valid NUL-terminated strings.
    let name = unsafe { CStr::from_ptr(name) }.to_bytes();
    if namespace == b"UniRx" && name == b"MainThreadDispatcher" {
        opaque(&CLASS)
    } else {
        std::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_name(_: *mut c_void) -> *const c_char {
    c"MainThreadDispatcher".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_namespace(_: *mut c_void) -> *const c_char {
    c"UniRx".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_assemblyname(_: *mut c_void) -> *const c_char {
    c"UniRx.dll".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_parent(_: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_image(_: *mut c_void) -> *mut c_void {
    opaque(&UNIRX_IMAGE)
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_type(_: *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_type_token(_: *mut c_void) -> u32 {
    0x0200_0001
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_fields(_: *mut c_void, _: *mut *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_interfaces(_: *mut c_void, _: *mut *mut c_void) -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_class_get_nested_types(
    _: *mut c_void,
    _: *mut *mut c_void,
) -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn il2cpp_class_get_methods(
    _: *mut c_void,
    iterator: *mut *mut c_void,
) -> *mut c_void {
    if iterator.is_null() {
        return std::ptr::null_mut();
    }
    if unsafe { (*iterator).is_null() } {
        // SAFETY: iterator is a valid in-out pointer per the IL2CPP contract.
        unsafe { *iterator = opaque(&CLASS) };
        method_info()
    } else {
        std::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_name(_: *mut c_void) -> *const c_char {
    if env_enabled("SCSP_FAKE_TARGET_DRIFT") {
        c"Other".as_ptr()
    } else {
        c"LateUpdate".as_ptr()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_flags(_: *mut c_void, _: *mut c_void) -> u32 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_return_type(_: *mut c_void) -> *mut c_void {
    opaque(&RETURN_TYPE)
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_param_count(_: *mut c_void) -> u8 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_param_name(_: *mut c_void, _: u32) -> *const c_char {
    std::ptr::null()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_param(_: *mut c_void, _: u32) -> *mut c_void {
    std::ptr::null_mut()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_token(_: *mut c_void) -> u32 {
    0x0600_0001
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_is_generic(_: *mut c_void) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_is_inflated(_: *mut c_void) -> bool {
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_is_instance(_: *mut c_void) -> bool {
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_method_get_declaring_type(_: *mut c_void) -> *mut c_void {
    opaque(&CLASS)
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_type_get_name(_: *mut c_void) -> *const c_char {
    c"System.Void".as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_thread_current() -> *mut c_void {
    if ATTACHED.load(Ordering::Acquire) {
        opaque(&THREAD)
    } else {
        std::ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_thread_attach(_: *mut c_void) -> *mut c_void {
    if env_enabled("SCSP_FAKE_ATTACH_FAIL") {
        return std::ptr::null_mut();
    }
    ATTACHED.store(true, Ordering::Release);
    opaque(&THREAD)
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_thread_detach(_: *mut c_void) {
    ATTACHED.store(false, Ordering::Release);
    DETACH_COUNT.fetch_add(1, Ordering::AcqRel);
}

#[unsafe(no_mangle)]
pub extern "C" fn il2cpp_is_vm_thread(_: *mut c_void) -> bool {
    ATTACHED.load(Ordering::Acquire)
}
