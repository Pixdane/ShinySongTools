//! Process-level observability root.
//!
//! Created in the outermost startup guard of `scsp_start`, before any
//! argument parsing, and kept alive until process exit. The full scoped
//! `tracing::Dispatch` + Unified Logging layer + compact-event drain worker
//! wiring lands with the bootstrap phase; the root object and its process
//! `OnceLock` exist from the start so entry semantics (duplicate entries
//! reuse the same root to record events) never change.

use std::sync::OnceLock;

/// Process-lifetime observability root.
pub struct ObservabilityRoot {
    /// Monotonic startup sequence for diagnostics.
    created_at: std::time::Instant,
}

impl ObservabilityRoot {
    fn new() -> Self {
        Self {
            created_at: std::time::Instant::now(),
        }
    }

    #[must_use]
    pub fn uptime(&self) -> std::time::Duration {
        self.created_at.elapsed()
    }
}

static ROOT: OnceLock<ObservabilityRoot> = OnceLock::new();

/// The process-wide root, created on first call and reused forever.
pub fn root() -> &'static ObservabilityRoot {
    ROOT.get_or_init(ObservabilityRoot::new)
}
