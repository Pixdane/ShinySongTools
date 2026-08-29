//! Direction-branded cross-domain message endpoints.
//!
//! Callbacks and the main thread are separate execution domains; the
//! direction and the mailbox semantics are part of the endpoint type, so
//! calling an endpoint from the wrong domain is a compile error. Callback
//! side requires `&CallbackCtx`, main side requires `&UpdateCtx<'_>`.
//!
//! The doc's "Mailbox kind token 具体类型呈现" polish item is resolved here
//! as dedicated registration methods per (direction, semantics) pair with
//! branded endpoint types (see `AppCtx`); behavior is unchanged.

use bevy_ecs::message::{Message, Messages};
use bevy_ecs::world::World;
use corelib::{BoundedQueue, CallbackPayload, LatestCell, SendOutcome, SharedSlot};
use std::sync::Arc;

use crate::plugin_api::hook::CallbackCtx;
use crate::plugin_api::host::{MainRouteDrain, SharedEnvelope};
use crate::plugin_api::phase::UpdateCtx;

// ---------------------------------------------------------------------------
// main → callback, latest semantics (payload: CallbackPayload, i.e. Copy)
// ---------------------------------------------------------------------------

/// Writes the latest value toward the callback domain (Update systems only).
pub struct MainLatestWriter<T>(Arc<LatestCell<T>>);

impl<T: CallbackPayload> MainLatestWriter<T> {
    pub(crate) fn new(cell: Arc<LatestCell<T>>) -> Self {
        Self(cell)
    }

    /// Overwrite the current value. Never blocks, never returns Full.
    pub fn try_send(&self, _ctx: &UpdateCtx<'_>, value: T) -> SendOutcome {
        self.0.try_send(value)
    }
}

/// Reads the latest value inside a hook callback.
pub struct CallbackLatestReader<T>(Arc<LatestCell<T>>);

impl<T: CallbackPayload> CallbackLatestReader<T> {
    pub(crate) fn new(cell: Arc<LatestCell<T>>) -> Self {
        Self(cell)
    }

    #[must_use]
    pub fn try_read(&self, _ctx: &CallbackCtx) -> Option<T> {
        self.0.try_read()
    }
}

// ---------------------------------------------------------------------------
// main → callback, bounded semantics
// ---------------------------------------------------------------------------

/// Pushes into the bounded FIFO toward the callback domain (Update systems).
pub struct MainBoundedWriter<T, const N: usize>(Arc<BoundedQueue<T, N>>);

impl<T: CallbackPayload, const N: usize> MainBoundedWriter<T, N> {
    pub(crate) fn new(queue: Arc<BoundedQueue<T, N>>) -> Self {
        Self(queue)
    }

    /// Push one item; `Full` when the queue is full (caller counts the drop).
    pub fn try_send(&self, _ctx: &UpdateCtx<'_>, value: T) -> SendOutcome {
        self.0.try_send(value)
    }
}

/// Pops from the bounded FIFO inside a hook callback.
pub struct CallbackBoundedReader<T, const N: usize>(Arc<BoundedQueue<T, N>>);

impl<T: CallbackPayload, const N: usize> CallbackBoundedReader<T, N> {
    pub(crate) fn new(queue: Arc<BoundedQueue<T, N>>) -> Self {
        Self(queue)
    }

    #[must_use]
    pub fn try_read(&self, _ctx: &CallbackCtx) -> Option<T> {
        self.0.try_read()
    }

    #[must_use]
    pub fn len(&self, _ctx: &CallbackCtx) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self, _ctx: &CallbackCtx) -> bool {
        self.0.is_empty()
    }
}

// ---------------------------------------------------------------------------
// main → callback, shared_latest semantics (owned data via Arc)
// ---------------------------------------------------------------------------

/// Writes an owned value toward the callback domain (Update systems only).
pub struct MainSharedWriter<T>(Arc<SharedSlot<T>>);

impl<T: Send + Sync + 'static> MainSharedWriter<T> {
    pub(crate) fn new(slot: Arc<SharedSlot<T>>) -> Self {
        Self(slot)
    }

    /// Replace the slot contents. `Busy` means the slot lock was contended;
    /// retry from the next Update system run.
    pub fn try_send(
        &self,
        _ctx: &UpdateCtx<'_>,
        value: T,
    ) -> Result<SendOutcome, corelib::SlotBusy> {
        self.0.try_send(Arc::new(value))
    }
}

/// Reads the shared slot inside a hook callback (`Arc` clone only).
pub struct CallbackSharedReader<T>(Arc<SharedSlot<T>>);

impl<T: Send + Sync + 'static> CallbackSharedReader<T> {
    pub(crate) fn new(slot: Arc<SharedSlot<T>>) -> Self {
        Self(slot)
    }

    #[must_use]
    pub fn try_read(&self, _ctx: &CallbackCtx) -> Option<Arc<T>> {
        self.0.try_read()
    }
}

// ---------------------------------------------------------------------------
// callback → main, latest semantics
// ---------------------------------------------------------------------------

/// Writes the latest value toward the main domain (inside hook callbacks).
pub struct CallbackLatestWriter<T>(Arc<LatestCell<T>>);

impl<T: CallbackPayload> CallbackLatestWriter<T> {
    pub(crate) fn new(cell: Arc<LatestCell<T>>) -> Self {
        Self(cell)
    }

    pub fn try_send(&self, _ctx: &CallbackCtx, value: T) -> SendOutcome {
        self.0.try_send(value)
    }
}

/// Main-side end of a callback→main latest route. The sanctioned consumption
/// path is the `CommandDrain` stage, which delivers into `Messages<T>`;
/// plugins read via the standard `MessageReader<T>` system param. This
/// handle additionally exposes depth for introspection.
pub struct MainLatestReader<T>(Arc<LatestCell<T>>);

impl<T: CallbackPayload> MainLatestReader<T> {
    pub(crate) fn new(cell: Arc<LatestCell<T>>) -> Self {
        Self(cell)
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        usize::from(self.0.try_read().is_some())
    }
}

impl<T> MainRouteDrain for MainLatestReader<T>
where
    T: CallbackPayload + Message,
{
    fn drain(&self, world: &mut World, _watermark: usize) -> usize {
        let Some(value) = self.0.take() else {
            return 0;
        };
        match world.get_resource_mut::<Messages<T>>() {
            Some(mut messages) => {
                messages.write(value);
                1
            }
            None => 0,
        }
    }

    fn depth(&self) -> usize {
        usize::from(self.0.try_read().is_some())
    }
}

// ---------------------------------------------------------------------------
// callback → main, bounded semantics
// ---------------------------------------------------------------------------

/// Pushes into the bounded FIFO toward the main domain (hook callbacks).
pub struct CallbackBoundedWriter<T, const N: usize>(Arc<BoundedQueue<T, N>>);

impl<T: CallbackPayload, const N: usize> CallbackBoundedWriter<T, N> {
    pub(crate) fn new(queue: Arc<BoundedQueue<T, N>>) -> Self {
        Self(queue)
    }

    pub fn try_send(&self, _ctx: &CallbackCtx, value: T) -> SendOutcome {
        self.0.try_send(value)
    }
}

/// Main-side end of a callback→main bounded route; drained by
/// `CommandDrain` into `Messages<T>` with the phase-entry watermark.
pub struct MainBoundedReader<T, const N: usize>(Arc<BoundedQueue<T, N>>);

impl<T: CallbackPayload, const N: usize> MainBoundedReader<T, N> {
    pub(crate) fn new(queue: Arc<BoundedQueue<T, N>>) -> Self {
        Self(queue)
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        self.0.len()
    }
}

impl<T, const N: usize> MainRouteDrain for MainBoundedReader<T, N>
where
    T: CallbackPayload + Message,
{
    fn drain(&self, world: &mut World, watermark: usize) -> usize {
        let Some(mut messages) = world.get_resource_mut::<Messages<T>>() else {
            return 0;
        };
        let mut delivered = 0;
        for _ in 0..watermark {
            let Some(value) = self.0.try_read() else {
                break;
            };
            messages.write(value);
            delivered += 1;
        }
        delivered
    }

    fn depth(&self) -> usize {
        self.0.len()
    }
}

// ---------------------------------------------------------------------------
// callback → main, shared_latest semantics
// ---------------------------------------------------------------------------

/// Writes an owned value toward the main domain (hook callbacks).
pub struct CallbackSharedWriter<T>(Arc<SharedSlot<T>>);

impl<T: Send + Sync + 'static> CallbackSharedWriter<T> {
    pub(crate) fn new(slot: Arc<SharedSlot<T>>) -> Self {
        Self(slot)
    }

    /// Replace the slot contents with an owned `Arc<T>`; the previous value
    /// only drops once its reference count reaches zero.
    pub fn try_send(
        &self,
        _ctx: &CallbackCtx,
        value: Arc<T>,
    ) -> Result<SendOutcome, corelib::SlotBusy> {
        self.0.try_send(value)
    }
}

/// Main-side end of a callback→main shared route; drained by
/// `CommandDrain` into `Messages<SharedEnvelope<T>>`.
pub struct MainSharedReader<T>(Arc<SharedSlot<T>>);

impl<T: Send + Sync + 'static> MainSharedReader<T> {
    pub(crate) fn new(slot: Arc<SharedSlot<T>>) -> Self {
        Self(slot)
    }

    #[must_use]
    pub fn depth(&self) -> usize {
        usize::from(self.0.try_read().is_some())
    }
}

impl<T> MainRouteDrain for MainSharedReader<T>
where
    T: Send + Sync + 'static + Message,
{
    fn drain(&self, world: &mut World, _watermark: usize) -> usize {
        let Some(value) = self.0.take() else {
            return 0;
        };
        match world.get_resource_mut::<Messages<SharedEnvelope<T>>>() {
            Some(mut messages) => {
                messages.write(SharedEnvelope(value));
                1
            }
            None => 0,
        }
    }

    fn depth(&self) -> usize {
        usize::from(self.0.try_read().is_some())
    }
}
