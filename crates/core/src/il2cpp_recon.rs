//! Development-only IL2CPP introspection facade (recon plugin consumer).
//!
//! Reads the hydrated, already-decrypted metadata cache — the same source the
//! hook resolver uses — and reports class surfaces (methods with their native
//! RVAs, fields with offsets) plus static field storage. This is the runtime
//! counterpart of an IL2CPP dump, scoped to one queried class; offline
//! dumping is impossible here because `global-metadata.dat` is encrypted.

use crate::error::Il2CppError;
use il2cpp_bridge_rs::memory::info::image::get_image_base;
use il2cpp_bridge_rs::{api, structs::core::members::field::Field};
use std::ffi::c_void;

const UNITY_FRAMEWORK_IMAGE: &str = "UnityFramework";

/// One method of a queried class, with its native code location.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MethodSurface {
    pub name: String,
    pub param_count: u32,
    /// Parameter type names in declaration order (the resolver's canonical
    /// form, matching `TargetId::parameter_types`).
    pub param_types: Vec<String>,
    /// Return type name in the resolver's canonical form.
    pub return_type: String,
    pub is_static: bool,
    /// Compiled function address, ASLR slide included.
    pub va: u64,
    /// Static offset from the UnityFramework image base, derived here from
    /// `va` (the bridge cache's own `rva` field is computed against a
    /// different base and does not match the mapped image).
    pub rva: u64,
}

/// One field of a queried class.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FieldSurface {
    pub name: String,
    pub type_name: String,
    pub is_static: bool,
    pub offset: i32,
}

/// Raw 8-byte view of one static field's storage slot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StaticValue {
    pub name: String,
    /// Raw word at `static_field_data + offset`; for reference-type fields
    /// this is the instance pointer (0 = null).
    pub raw: u64,
}

/// Complete surface of one queried class.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ClassSurface {
    pub assembly: String,
    pub full_name: String,
    pub class_address: usize,
    pub static_field_data: usize,
    pub image_base: usize,
    pub methods: Vec<MethodSurface>,
    pub fields: Vec<FieldSurface>,
    pub statics: Vec<StaticValue>,
}

/// Base address of the loaded UnityFramework image, if known.
#[must_use]
pub fn image_base() -> Option<usize> {
    get_image_base(UNITY_FRAMEWORK_IMAGE)
}

/// Surface of one class from the hydrated metadata cache. `class_full_name`
/// uses the resolver's "Namespace.Class" form.
///
/// # Errors
///
/// [`Il2CppError::ClassNotFound`] when the assembly or class is absent from
/// the hydrated cache.
pub fn class_surface(assembly: &str, class_full_name: &str) -> Result<ClassSurface, Il2CppError> {
    let cached = api::cache::assembly(assembly)
        .ok_or_else(|| Il2CppError::ClassNotFound(assembly.to_owned()))?
        .class(class_full_name)
        .ok_or_else(|| Il2CppError::ClassNotFound(format!("{assembly}::{class_full_name}")))?;

    let image_base = get_image_base(UNITY_FRAMEWORK_IMAGE).unwrap_or(0);

    let methods = cached
        .methods
        .iter()
        .map(|method| {
            let va = method.va;
            MethodSurface {
                name: method.name.clone(),
                param_count: u32::from(method.param_count),
                param_types: method
                    .args
                    .iter()
                    .map(|arg| arg.type_info.cpp_name())
                    .collect(),
                return_type: method.return_type.cpp_name(),
                is_static: method.is_static,
                va,
                rva: va.saturating_sub(image_base as u64),
            }
        })
        .collect();

    let fields: Vec<FieldSurface> = cached
        .fields
        .iter()
        .map(|field| FieldSurface {
            name: field.name.clone(),
            type_name: field.type_info.name.clone(),
            is_static: field.is_static,
            offset: field.offset,
        })
        .collect();

    // Raw static storage: reference-type statics surface their instance
    // pointer here, which is how singletons are located without a hook.
    let statics = read_statics(cached.static_field_data, &cached.fields);

    Ok(ClassSurface {
        assembly: cached.assembly_name.clone(),
        full_name: format!("{}.{}", cached.namespace, cached.name),
        class_address: cached.address as usize,
        static_field_data: cached.static_field_data as usize,
        image_base,
        methods,
        fields,
        statics,
    })
}

/// Read one pointer-sized raw word per instance static field with a valid
/// offset. Malformed offsets and null storage are skipped, never guessed.
fn read_statics(static_data: *mut c_void, fields: &[Field]) -> Vec<StaticValue> {
    let mut out = Vec::new();
    if static_data.is_null() {
        return out;
    }
    for field in fields {
        if !field.is_static || field.offset < 0 {
            continue;
        }
        // Statically-known field offsets into the static data block of a
        // live class; the read is a bounded 8-byte load inside the block.
        let raw = unsafe {
            std::ptr::read_volatile((static_data as usize + field.offset as usize) as *const u64)
        };
        out.push(StaticValue {
            name: field.name.clone(),
            raw,
        });
    }
    out
}

/// Resolve one method's compiled function pointer by exact name from the
/// hydrated cache (the runtime equivalent of upstream's `get_method_pointer`).
///
/// Returns `Ok(None)` when the method is absent, ambiguous, or not compiled:
/// callers treat this as "not available" and never guess.
///
/// # Errors
///
/// [`Il2CppError::ClassNotFound`] when the assembly or class is absent from
/// the hydrated cache.
pub fn resolve_method_va(
    assembly: &str,
    class_full_name: &str,
    method: &str,
    param_count: u32,
) -> Result<Option<u64>, Il2CppError> {
    let cached = api::cache::assembly(assembly)
        .ok_or_else(|| Il2CppError::ClassNotFound(assembly.to_owned()))?
        .class(class_full_name)
        .ok_or_else(|| Il2CppError::ClassNotFound(format!("{assembly}::{class_full_name}")))?;
    let matches: Vec<_> = cached
        .methods
        .iter()
        .filter(|m| m.name == method && u32::from(m.param_count) == param_count)
        .collect();
    let mut it = matches.into_iter();
    match (it.next(), it.next()) {
        (Some(m), None) if m.va != 0 => Ok(Some(m.va)),
        _ => Ok(None),
    }
}

/// Create a live IL2CPP string from UTF-16 code units (main domain). Returns
/// 0 on failure.
///
/// # Safety
///
/// The main thread must be attached to the IL2CPP runtime.
#[must_use]
pub unsafe fn new_string_utf16(units: &[u16]) -> usize {
    // SAFETY: caller guarantees an attached main thread; the API copies the
    // units into a newly allocated managed string.
    unsafe { api::string_new_utf16(units.as_ptr(), units.len() as i32) as usize }
}

/// Read a live `System.String` object's UTF-16 contents (main domain).
///
/// # Safety
///
/// `ptr` must reference a live IL2CPP string object (or be null). The GC is
/// non-moving, so the pointer stays valid while the object is reachable.
#[must_use]
pub unsafe fn read_il2cpp_string_utf16(ptr: usize) -> Option<Vec<u16>> {
    if ptr == 0 {
        return None;
    }
    // SAFETY: IL2CPP string layout on 64-bit: klass(8) monitor(8) length(4)
    // then UTF-16 code units; the caller contract guarantees a live object.
    unsafe {
        let len = std::ptr::read_volatile((ptr + 0x10) as *const i32);
        if len <= 0 {
            return Some(Vec::new());
        }
        let len = len as usize;
        let mut out = Vec::with_capacity(len);
        for index in 0..len {
            out.push(std::ptr::read_volatile(
                (ptr + 0x14 + index * 2) as *const u16,
            ));
        }
        Some(out)
    }
}
