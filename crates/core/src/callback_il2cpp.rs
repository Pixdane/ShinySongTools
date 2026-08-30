//! Narrow IL2CPP access that is valid only during a hook callback.
//!
//! This is intentionally a read-only v1 capability. It exposes the UTF-16
//! view of a managed string while the callback still owns a live argument or
//! return value; the slice must not escape that callback.

use crate::hook::CallbackCtx;
use std::ffi::c_void;

/// Read-only callback-domain IL2CPP capability.
#[derive(Clone, Copy, Debug)]
pub struct CallbackIl2Cpp {
    _private: (),
}

impl CallbackIl2Cpp {
    pub(crate) const fn new() -> Self {
        Self { _private: () }
    }

    /// Borrow the UTF-16 code units of a live `System.String`.
    ///
    /// Returns `None` for a null pointer, a negative runtime length, or a
    /// malformed non-empty string whose character pointer is null.
    ///
    /// # Safety
    ///
    /// `string` must point to a live IL2CPP `System.String` for the complete
    /// lifetime of `callback`. The returned slice must not escape that
    /// callback. This method performs no allocation and does not retain the
    /// managed object.
    pub unsafe fn string_utf16<'callback>(
        &self,
        callback: &'callback CallbackCtx,
        string: *mut c_void,
    ) -> Option<&'callback [u16]> {
        let _ = callback;
        if string.is_null() {
            return None;
        }
        // SAFETY: guaranteed by this method's target-bound caller contract.
        let len = unsafe { il2cpp_bridge_rs::api::string_length(string) };
        let len = usize::try_from(len).ok()?;
        if len == 0 {
            return Some(&[]);
        }
        // SAFETY: same live managed string contract as the length query.
        let chars = unsafe { il2cpp_bridge_rs::api::string_chars(string) };
        if chars.is_null() {
            return None;
        }
        // SAFETY: IL2CPP guarantees `len` UTF-16 code units for a live
        // System.String; the callback lifetime prevents the borrow escaping.
        Some(unsafe { core::slice::from_raw_parts(chars.cast_const(), len) })
    }
}
