//! Process-level runtime gate and per-plugin gate readers.
//!
//! The gate is a single `AtomicBool`-backed flag. `GateReader` reads with
//! `Acquire`; the control side writes with `Release`. This module is the
//! only authoritative definition of gate memory ordering; other documents
//! reference it instead of restating it.
//!
//! The [`RuntimeGate`] starts closed, is opened once after the first Startup
//! driver completes successfully, and can never be reopened after a close.
//! Per-plugin gates reuse [`GateReader`] for their reader side.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

/// Read-only handle to a gate. Callbacks, debug routes, and I/O workers hold
/// only this side.
#[derive(Clone)]
pub struct GateReader(Arc<AtomicBool>);

impl GateReader {
    pub(crate) fn new(flag: Arc<AtomicBool>) -> Self {
        Self(flag)
    }

    /// `true` when the gate is open. Reads with `Acquire` semantics, pairing
    /// with the `Release` writes performed by [`RuntimeGate::open`] and
    /// [`RuntimeGate::close`].
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl core::fmt::Debug for GateReader {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("GateReader").field(&self.is_open()).finish()
    }
}

/// Control state of a [`RuntimeGate`]:
/// `0` = closed (initial), `1` = open, `2` = permanently closed.
const GATE_CLOSED: u8 = 0;
const GATE_OPEN: u8 = 1;
const GATE_FAILED_CLOSED: u8 = 2;

/// The process-wide runtime gate control handle.
///
/// Initial state is closed. `open` succeeds at most once and only while no
/// close has happened; after [`RuntimeGate::close`] the gate stays closed for
/// the rest of the process. The flag itself (`GateReader`) is the shared
/// reader surface.
#[derive(Clone)]
pub struct RuntimeGate {
    flag: Arc<AtomicBool>,
    control: Arc<AtomicU8>,
}

impl RuntimeGate {
    /// Create a new runtime gate in the closed state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            flag: Arc::new(AtomicBool::new(false)),
            control: Arc::new(AtomicU8::new(GATE_CLOSED)),
        }
    }

    /// Read-only side of this gate.
    #[must_use]
    pub fn reader(&self) -> GateReader {
        GateReader::new(Arc::clone(&self.flag))
    }

    /// Open the gate. Returns `false` (without changing anything) if the gate
    /// was already opened or has been closed. Writers use `Release` so that
    /// every `Acquire` read of `true` also observes everything published
    /// before the open.
    pub fn open(&self) -> bool {
        self.control
            .compare_exchange(GATE_CLOSED, GATE_OPEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok_and(|_| {
                self.flag.store(true, Ordering::Release);
                true
            })
    }

    /// Close the gate permanently. Subsequent `open` calls fail; the flag is
    /// cleared with `Release` so callbacks observing `is_open() == false`
    /// under `Acquire` never enter runtime logic again.
    pub fn close(&self) {
        self.control.store(GATE_FAILED_CLOSED, Ordering::Release);
        self.flag.store(false, Ordering::Release);
    }

    /// Whether the gate has been closed (permanently). Diagnostic use only;
    /// functional decisions must go through a `GateReader`.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.control.load(Ordering::Acquire) == GATE_FAILED_CLOSED
    }
}

impl Default for RuntimeGate {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for RuntimeGate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RuntimeGate")
            .field("open", &self.reader().is_open())
            .field("closed", &self.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_reader_reads_with_expected_visibility() {
        let gate = RuntimeGate::new();
        let reader = gate.reader();
        assert!(!reader.is_open(), "runtime gate starts closed");
        assert!(gate.open(), "first open succeeds");
        assert!(reader.is_open());
        assert!(!gate.open(), "second open is a no-op");
        gate.close();
        assert!(!reader.is_open(), "close is visible through readers");
        assert!(!gate.open(), "gate cannot reopen after close");
        assert!(!reader.is_open());
        assert!(gate.is_closed());
    }
}
