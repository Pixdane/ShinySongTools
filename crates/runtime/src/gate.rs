//! Per-plugin gate control side.
//!
//! Reader semantics reuse [`GateReader`] (Acquire
//! reads) via the core [`RuntimeGate`] machinery; the control side belongs
//! to the plugin-system layer. A plugin gate starts closed (build phase), is
//! opened once when its Startup systems all succeed, and closes permanently
//! on retirement — the same closed→open→failed-closed state machine as the
//! process gate.

use corelib::{GateReader, RuntimeGate};

/// Control handle of one plugin's gate.
#[derive(Clone)]
pub struct PluginGate {
    inner: RuntimeGate,
}

impl PluginGate {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RuntimeGate::new(),
        }
    }

    /// Read-only side handed to hook sites and debug routes.
    #[must_use]
    pub fn reader(&self) -> GateReader {
        self.inner.reader()
    }

    /// Open the gate (once; later opens are no-ops).
    pub fn open(&self) {
        let _ = self.inner.open();
    }

    /// Close the gate permanently (retirement path).
    pub fn close(&self) {
        self.inner.close();
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.inner.reader().is_open()
    }
}

impl Default for PluginGate {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for PluginGate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PluginGate")
            .field("open", &self.is_open())
            .finish()
    }
}
