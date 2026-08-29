//! Callback-safe containers: the three mailbox semantics.
//!
//! Callback-side operations are bounded and never block an OS thread, never
//! allocate, and never run arbitrary plugin code. Payloads for
//! [`LatestCell`] and [`BoundedQueue`] are [`CallbackPayload`] (`Copy`),
//! so overwriting or reading never executes a payload destructor.
//! [`SharedSlot`] carries owned structured data (`Arc<T>`) and its replace
//! operation may drop the old value; that is why it is constrained to plain
//! data with side-effect-free `Drop` instead of `Copy`.

use crossbeam_queue::ArrayQueue;
use crossbeam_utils::atomic::AtomicCell;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};

/// Payload bound for `latest` / `bounded` mailboxes.
///
/// `Copy` guarantees mailbox operations never run a payload destructor; the
/// `Send + Sync + 'static` bounds let containers live across execution
/// domains.
pub trait CallbackPayload: Copy + Send + Sync + 'static {}
impl<T: Copy + Send + Sync + 'static> CallbackPayload for T {}

/// Outcome of a non-blocking send into a mailbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// Stored into an empty slot/queue.
    Accepted,
    /// Replaced a previous value (`latest` semantics only).
    Replaced,
    /// Queue was full (`bounded` semantics only); the caller counts the drop.
    Full,
    /// Single-slot lock was contended (`shared` semantics only); the value
    /// was not stored and the caller may retry later.
    Busy,
}

impl SendOutcome {
    #[must_use]
    pub fn is_stored(self) -> bool {
        matches!(self, Self::Accepted | Self::Replaced)
    }
}

/// Latest-value cell: writes overwrite, intermediate states may be lost.
///
/// `AtomicCell` performs the store/load with atomics when `T` fits, and with
/// a bounded internal spin lock otherwise; it never blocks on an OS lock and
/// never allocates, satisfying the callback-side contract.
pub struct LatestCell<T> {
    cell: AtomicCell<Option<T>>,
}

impl<T: CallbackPayload> LatestCell<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cell: AtomicCell::new(None),
        }
    }

    /// Overwrite with `value`. Returns [`SendOutcome::Accepted`] when the
    /// cell was empty and [`SendOutcome::Replaced`] when a previous value
    /// was overwritten (that value is `Copy`-dropped, no destructor runs).
    pub fn try_send(&self, value: T) -> SendOutcome {
        if self.cell.swap(Some(value)).is_some() {
            SendOutcome::Replaced
        } else {
            SendOutcome::Accepted
        }
    }

    /// Read the current value, if any.
    #[must_use]
    pub fn try_read(&self) -> Option<T> {
        self.cell.load()
    }

    /// Take the current value, leaving the cell empty.
    #[must_use]
    pub fn take(&self) -> Option<T> {
        self.cell.swap(None)
    }
}

impl<T: CallbackPayload> Default for LatestCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Bounded FIFO queue with order preservation.
///
/// `try_send` on a full queue returns [`SendOutcome::Full`] without
/// blocking; the caller accumulates the dropped count. Backed by
/// `crossbeam_queue::ArrayQueue`.
pub struct BoundedQueue<T, const N: usize> {
    queue: ArrayQueue<T>,
}

impl<T: CallbackPayload, const N: usize> BoundedQueue<T, N> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            queue: ArrayQueue::new(N),
        }
    }

    pub fn try_send(&self, value: T) -> SendOutcome {
        if self.queue.push(value).is_err() {
            SendOutcome::Full
        } else {
            SendOutcome::Accepted
        }
    }

    #[must_use]
    pub fn try_read(&self) -> Option<T> {
        self.queue.pop()
    }

    /// Number of items currently buffered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

impl<T: CallbackPayload, const N: usize> Default for BoundedQueue<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Single slot carrying owned structured data (`Arc<T>`).
///
/// `try_send` replaces the slot; the old `Arc` only drops when its reference
/// count reaches zero, and that `T` must be plain data with side-effect-free
/// `Drop` (serde-derived structs). `try_read` clones the `Arc`, which only
/// bumps the reference count and does not allocate. The lock policy is
/// `try_lock`-first: a contended slot reports [`SendOutcome::Busy`] /
/// `Err`([`SlotBusy`]) instead of blocking the caller.
pub struct SharedSlot<T> {
    slot: Mutex<Option<Arc<T>>>,
}

/// Error returned when the shared slot lock was contended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotBusy;

impl<T: Send + Sync + 'static> SharedSlot<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: Mutex::new(None),
        }
    }

    /// Replace the slot contents with `value`.
    pub fn try_send(&self, value: Arc<T>) -> Result<SendOutcome, SlotBusy> {
        match self.slot.try_lock() {
            Ok(mut guard) => {
                let outcome = if guard.is_some() {
                    SendOutcome::Replaced
                } else {
                    SendOutcome::Accepted
                };
                *guard = Some(value);
                Ok(outcome)
            }
            Err(TryLockError::WouldBlock) => Err(SlotBusy),
            Err(TryLockError::Poisoned(_)) => Err(SlotBusy),
        }
    }

    /// Clone the current `Arc` out of the slot, if any.
    #[must_use]
    pub fn try_read(&self) -> Option<Arc<T>> {
        let guard: MutexGuard<'_, Option<Arc<T>>> = lock(&self.slot)?;
        guard.clone()
    }

    /// Take the current value out of the slot, if any.
    #[must_use]
    pub fn take(&self) -> Option<Arc<T>> {
        let mut guard: MutexGuard<'_, Option<Arc<T>>> = lock(&self.slot)?;
        guard.take()
    }

    /// Whether the slot currently holds a value. A contended lock reports
    /// `true` (conservative: callers that must not overwrite treat busy as
    /// occupied).
    #[must_use]
    pub fn is_set(&self) -> bool {
        match self.slot.try_lock() {
            Ok(guard) => guard.is_some(),
            Err(_) => true,
        }
    }
}

impl<T: Send + Sync + 'static> Default for SharedSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Option<MutexGuard<'_, T>> {
    match mutex.try_lock() {
        Ok(guard) => Some(guard),
        // A poisoned slot still contains valid data; recover the guard.
        Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_cell_overwrites_and_reports_replacement() {
        let cell: LatestCell<u32> = LatestCell::new();
        assert_eq!(cell.try_send(1), SendOutcome::Accepted);
        assert_eq!(cell.try_send(2), SendOutcome::Replaced);
        assert_eq!(cell.try_read(), Some(2));
        assert_eq!(cell.take(), Some(2));
        assert_eq!(cell.try_read(), None);
    }

    #[test]
    fn bounded_queue_is_fifo_and_counts_full() {
        let queue: BoundedQueue<u8, 2> = BoundedQueue::new();
        assert_eq!(queue.try_send(1), SendOutcome::Accepted);
        assert_eq!(queue.try_send(2), SendOutcome::Accepted);
        assert_eq!(queue.try_send(3), SendOutcome::Full);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.try_read(), Some(1), "FIFO order preserved");
        assert_eq!(queue.try_read(), Some(2));
        assert_eq!(queue.try_read(), None);
    }

    #[test]
    fn shared_slot_replaces_arc_and_read_clones() {
        let slot: SharedSlot<String> = SharedSlot::new();
        assert_eq!(
            slot.try_send(Arc::new("a".to_owned())),
            Ok(SendOutcome::Accepted)
        );
        let first = slot.try_read().expect("value present");
        assert_eq!(
            slot.try_send(Arc::new("b".to_owned())),
            Ok(SendOutcome::Replaced)
        );
        assert_eq!((*first).as_str(), "a", "old Arc stays alive for readers");
        drop(first);
        assert_eq!((*slot.try_read().expect("value")).as_str(), "b");
        assert!(slot.take().is_some());
        assert!(slot.try_read().is_none());
    }
}
