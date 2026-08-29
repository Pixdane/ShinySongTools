//! The three-phase original-call guard.
//!
//! One shared implementation backs both the scheduler replacement (runtime
//! crate) and hook target wrappers (plugin API): the original may only be
//! called once, and recovery paths branch on the recorded phase. The guard
//! records phases; it never calls anything itself.

use core::cell::Cell;

/// Phase of the exactly-once original call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginalPhase {
    /// The original has not been called yet.
    BeforeOriginal,
    /// The original call is in progress; on unwind its effect must be
    /// treated as unknown (never retried).
    CallingOriginal,
    /// The original returned normally.
    AfterOriginal,
}

/// Stack-local guard tracking one original-call opportunity.
pub struct OriginalGuard {
    phase: Cell<OriginalPhase>,
}

impl OriginalGuard {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Cell::new(OriginalPhase::BeforeOriginal),
        }
    }

    /// Current phase.
    #[must_use]
    pub fn phase(&self) -> OriginalPhase {
        self.phase.get()
    }

    /// `true` when the original still needs to be called.
    #[must_use]
    pub fn needs_original(&self) -> bool {
        self.phase() == OriginalPhase::BeforeOriginal
    }

    /// Transition `BeforeOriginal -> CallingOriginal`. Returns `false` if the
    /// original was already called or is currently being called, rejecting
    /// duplicate invocations.
    pub fn begin_call(&self) -> bool {
        match self.phase.get() {
            OriginalPhase::BeforeOriginal => {
                self.phase.set(OriginalPhase::CallingOriginal);
                true
            }
            _ => false,
        }
    }

    /// Transition `CallingOriginal -> AfterOriginal`.
    pub fn end_call(&self) {
        if self.phase.get() == OriginalPhase::CallingOriginal {
            self.phase.set(OriginalPhase::AfterOriginal);
        }
    }
}

impl Default for OriginalGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for OriginalGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OriginalGuard")
            .field("phase", &self.phase())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_once_transitions() {
        let guard = OriginalGuard::new();
        assert!(guard.needs_original());
        assert!(guard.begin_call(), "first call accepted");
        assert!(!guard.begin_call(), "duplicate call rejected");
        guard.end_call();
        assert_eq!(guard.phase(), OriginalPhase::AfterOriginal);
        assert!(!guard.needs_original());
        assert!(!guard.begin_call());
    }
}
