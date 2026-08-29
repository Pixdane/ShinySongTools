//! Compact events: the callback/scheduler hot-path observability channel.
//!
//! Hot-path code submits fixed-size [`CompactEvent`] values into a
//! process-level bounded queue through [`CallbackObservability`]. A runtime
//! drain worker converts them into normal tracing events. Queue-full only
//! bumps the dropped counter; emitting never changes original behavior and
//! never triggers retirement or failure.

use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Fixed capacity of the process-level compact event queue.
pub const CALLBACK_EVENT_QUEUE_CAPACITY: usize = 4096;

/// Stable severity for compact events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CompactLevel {
    Info = 0,
    Warn = 1,
    Error = 2,
}

/// Stable event code. v1 only allows core/runtime predefined codes; plugin
/// callbacks never register custom descriptors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct CompactEventCode(pub u16);

/// Identity of the emitting owner (plugin id or a runtime infrastructure id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct CompactOwnerId(pub u32);

/// Identity of the emission site (a compact per-site index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct CompactSiteId(pub u32);

/// Fixed-size, `Copy`, destructor-free event record.
#[derive(Debug, Clone, Copy)]
pub struct CompactEvent {
    pub code: CompactEventCode,
    pub level: CompactLevel,
    pub owner: CompactOwnerId,
    pub site: CompactSiteId,
    pub arg0: u64,
    pub arg1: u64,
}

impl CompactEvent {
    #[must_use]
    pub const fn new(code: CompactEventCode, level: CompactLevel) -> Self {
        Self {
            code,
            level,
            owner: CompactOwnerId(0),
            site: CompactSiteId(0),
            arg0: 0,
            arg1: 0,
        }
    }

    #[must_use]
    pub const fn owner(mut self, owner: CompactOwnerId) -> Self {
        self.owner = owner;
        self
    }

    #[must_use]
    pub const fn site(mut self, site: CompactSiteId) -> Self {
        self.site = site;
        self
    }

    #[must_use]
    pub const fn args(mut self, arg0: u64, arg1: u64) -> Self {
        self.arg0 = arg0;
        self.arg1 = arg1;
        self
    }
}

/// Shared queue + dropped counter behind every producer handle.
struct CompactEventInner {
    queue: ArrayQueue<CompactEvent>,
    dropped: AtomicU64,
}

/// Producer-side handle for callback and scheduler hot paths.
///
/// This is infrastructure, not a plugin message route: it does not read any
/// gate, cannot reach `App`/`World`, keeps recording after a plugin
/// retires, and rejects dynamic strings or arbitrary plugin payloads by
/// construction (only `CompactEvent` fits through).
#[derive(Clone)]
pub struct CallbackObservability {
    inner: Arc<CompactEventInner>,
}

impl CallbackObservability {
    /// Create a fresh queue + counter pair.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(CALLBACK_EVENT_QUEUE_CAPACITY)
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(CompactEventInner {
                queue: ArrayQueue::new(capacity),
                dropped: AtomicU64::new(0),
            }),
        }
    }

    /// Submit one event. `false` means the queue was full and the event was
    /// counted as dropped.
    pub fn try_emit(&self, event: CompactEvent) -> bool {
        match self.inner.queue.push(event) {
            Ok(()) => true,
            Err(_) => {
                self.inner.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Total dropped count since creation (monotonic, diagnostics only).
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Drain up to `max` events for the runtime drain worker.
    pub fn drain(&self, max: usize) -> impl Iterator<Item = CompactEvent> + '_ {
        let inner = &self.inner;
        (0..max).map_while(|_| inner.queue.pop())
    }
}

impl Default for CallbackObservability {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for CallbackObservability {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CallbackObservability")
            .field("queued", &self.inner.queue.len())
            .field("dropped", &self.inner.dropped.load(Ordering::Relaxed))
            .finish()
    }
}

static PROCESS_QUEUE: OnceLock<CallbackObservability> = OnceLock::new();

/// Process-level producer, created on first use. The runtime installs the
/// drain worker over the same queue; hot-path code may grab the handle
/// before the worker exists because emitting is valid at any time.
#[must_use]
pub fn process_event_queue() -> CallbackObservability {
    let _ = PROCESS_QUEUE.get_or_init(CallbackObservability::new);
    PROCESS_QUEUE.get().expect("just initialized").clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_and_drain_preserves_fifo_and_counts_drops() {
        let obs = CallbackObservability::with_capacity(2);
        let e = CompactEvent::new(CompactEventCode(1), CompactLevel::Info);
        assert!(obs.try_emit(e.args(1, 0)));
        assert!(obs.try_emit(e.args(2, 0)));
        assert!(!obs.try_emit(e.args(3, 0)), "queue full is reported");
        assert_eq!(obs.dropped(), 1);
        let drained: Vec<_> = obs.drain(10).collect();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].arg0, 1);
        assert_eq!(drained[1].arg0, 2);
        assert_eq!(obs.drain(10).count(), 0);
    }
}
