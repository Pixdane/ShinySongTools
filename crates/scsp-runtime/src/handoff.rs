//! One-shot cross-thread ownership slot for the `App` (docs/runtime-crate.md
//! Handoff 同步).
//!
//! The worker publishes at most once; scheduler callbacks only `try_take`.
//! Holding the guard is restricted to moving one `Box<App>`: no plugin
//! logic, no logging, no original calls inside the critical section.

use crate::app::App;
use std::sync::Mutex;

pub struct Handoff {
    app: Mutex<Option<Box<App>>>,
}

/// Result of a callback-side `try_take`.
pub enum HandoffTake {
    /// The App transferred to this callback.
    Ready(Box<App>),
    /// Slot empty or worker still holds the lock: try again next callback.
    Pending,
    /// Poisoned mutex or broken invariant: the TLS becomes Unavailable.
    Failed,
}

impl Handoff {
    #[must_use]
    pub fn empty() -> Self {
        Self {
            app: Mutex::new(None),
        }
    }

    /// Worker side: publish exactly once. A second publish is refused (the
    /// design allows only one Handoff per process) and returns `false`.
    pub fn publish(&self, app: Box<App>) -> bool {
        let mut guard = match self.app.lock() {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        if guard.is_some() {
            return false;
        }
        *guard = Some(app);
        true
    }

    /// Callback side: never blocks longer than one mutex acquisition.
    pub fn try_take(&self) -> HandoffTake {
        let mut guard = match self.app.lock() {
            Ok(guard) => guard,
            Err(_) => return HandoffTake::Failed,
        };
        match guard.take() {
            Some(app) => HandoffTake::Ready(app),
            None => HandoffTake::Pending,
        }
    }
}

impl Default for Handoff {
    fn default() -> Self {
        Self::empty()
    }
}

impl core::fmt::Debug for Handoff {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Handoff")
    }
}
