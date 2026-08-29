//! The FFI entry point (`scsp_start`).
//!
//! Entry semantics (docs/swift-entry.md + docs/runtime-crate.md):
//!
//! * outermost unwind guard: no Rust panic crosses the FFI boundary;
//! * the process-lifetime one-shot startup marker is claimed BEFORE the
//!   arguments are copied, so a first call with invalid arguments still
//!   consumes the single bootstrap attempt;
//! * null pointer / empty path are rejected (recorded, no retry);
//! * the C string is copied into Rust-owned data before returning — the
//!   Swift-side `withCString` pointer is only valid for this call;
//! * the (placeholder) bootstrap worker is started and the call returns
//!   immediately; the bootstrap body lands in the same worker in the next
//!   phase;
//! * the observability root lives in its own process `OnceLock`; duplicate
//!   entries only reuse it to record an event.

use std::ffi::{CStr, c_char};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// One-shot process startup marker.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Swift calls this once per process from the main queue. Re-entrant or
/// duplicate calls are recorded and ignored.
///
/// # FFI safety
///
/// `documents_path` must be a valid NUL-terminated C string or null; the
/// pointed-to memory is only read within this call.
#[unsafe(no_mangle)]
pub extern "C" fn scsp_start(documents_path: *const c_char) {
    // Outermost unwind guard: a panic here must never cross the FFI
    // boundary.
    let result = std::panic::catch_unwind(|| entry(documents_path));
    if let Err(payload) = result {
        // Nothing may panic here either. This recovery path runs outside any
        // scoped dispatch (the scope guard was unwound away), so it reports
        // through the compact queue, which works from any thread.
        drop(payload);
        let _ = scsp_core::process_event_queue().try_emit(scsp_core::CompactEvent::new(
            scsp_core::CompactEventCode(crate::observability::compact_codes::FFI_ENTRY_PANICKED),
            scsp_core::CompactLevel::Error,
        ));
    }
}

fn entry(documents_path: *const c_char) {
    // Establish the process observability root + the scoped dispatch for
    // this execution root as early as possible (docs: Observability is
    // created inside the outermost startup guard, before argument parsing).
    let _obs = crate::observability::scope();

    // Claim the one-shot marker before touching the arguments: the first
    // call consumes the process's single bootstrap attempt even if its
    // arguments turn out to be invalid.
    if STARTED.swap(true, Ordering::AcqRel) {
        tracing::warn!(target: "scsp_start", "duplicate start ignored");
        return;
    }

    // Validate and copy the path. The Swift-side pointer dies with this
    // call; only Rust-owned data leaves this function.
    let owned = match copy_documents_path(documents_path) {
        Some(path) => path,
        None => {
            tracing::error!(target: "scsp_start", "invalid documents path (null or empty); startup terminated");
            return;
        }
    };
    tracing::info!(target: "scsp_start", "documents path accepted; starting bootstrap worker");

    // The one-shot bootstrap worker: readiness ladder, App build, scheduler
    // publication, and Handoff run here, independent of the caller's stack.
    // The worker is its own execution root and establishes the scoped
    // dispatch itself.
    let spawned = std::thread::Builder::new()
        .name("scsp-bootstrap".to_owned())
        .spawn(move || {
            let _obs = crate::observability::scope();
            let data_root = scsp_core::DataRoot::new(owned);
            let deps = match crate::bootstrap::production_deps(&data_root) {
                Some(deps) => deps,
                None => return,
            };
            if crate::bootstrap::run_bootstrap(deps) {
                tracing::info!(target: "scsp_start", "bootstrap published the App");
            } else {
                tracing::error!(target: "scsp_start", "bootstrap terminated (one-shot)");
            }
        });

    if spawned.is_err() {
        tracing::error!(target: "scsp_start", "failed to spawn bootstrap worker; startup terminated");
    }
    // Return immediately: the Swift main queue is never blocked here.
}

fn copy_documents_path(documents_path: *const c_char) -> Option<PathBuf> {
    if documents_path.is_null() {
        return None;
    }
    // SAFETY: caller contract — a valid NUL-terminated C string for the
    // duration of this call.
    let cstr = unsafe { CStr::from_ptr(documents_path) };
    let text = core::str::from_utf8(cstr.to_bytes()).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_rejects_null_and_empty() {
        assert!(copy_documents_path(std::ptr::null()).is_none());
        let empty = b"\0";
        let ptr = empty.as_ptr() as *const c_char;
        assert!(copy_documents_path(ptr).is_none());
    }

    #[test]
    fn copy_accepts_valid_path() {
        let ok = b"/tmp/some-documents\0";
        let ptr = ok.as_ptr() as *const c_char;
        let path = copy_documents_path(ptr).expect("valid path");
        assert_eq!(path, PathBuf::from("/tmp/some-documents"));
    }
}
