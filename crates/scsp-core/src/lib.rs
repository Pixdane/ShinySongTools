//! Platform primitives for the Shiny Song Tools runtime.
//!
//! `scsp-core` sits at the bottom of the dependency graph. It provides the
//! thread capability ([`MainThreadToken`]), process gates, the MethodPointer
//! slot wrapper, callback-safe containers, compact events, the plugin error
//! chain, and the IL2CPP backend abstraction. It knows nothing about `App`,
//! plugins, or the bootstrap process.
//!
//! The IL2CPP backend exists in two shapes behind one trait pair
//! ([`backend::Il2CppApi`] + [`backend::MethodResolver`]):
//!
//! * [`il2cpp_bridge::BridgeBackend`] — the production backend, a thin
//!   adapter over the pinned `il2cpp-bridge-rs 0.1.4` crate driven by the
//!   exact UnityFramework handle. Only reachable on the live path.
//! * Mock implementations inside the no-game fixtures, which drive the same
//!   protocol without a game process.

pub mod backend;
pub mod base;
pub mod container;
pub mod error;
pub mod event;
pub mod gate;
pub mod il2cpp_bridge;
pub mod main_thread;
pub mod method_slot;
pub mod original;

pub use backend::{
    AttachGuard, DomainHandle, Il2CppApi, ImageHandle, ImageIdentity, MethodResolver,
    ResolvedMethod, RuntimeIdentity,
};
pub use base::{DataRoot, OwnerId, RouteId, TopicId};
pub use container::{BoundedQueue, CallbackPayload, LatestCell, SendOutcome, SharedSlot, SlotBusy};
pub use error::{HookError, Il2CppError, PluginError, RestoreError};
pub use event::{
    CALLBACK_EVENT_QUEUE_CAPACITY, CallbackObservability, CompactEvent, CompactEventCode,
    CompactLevel, CompactOwnerId, CompactSiteId, process_event_queue,
};
pub use gate::{GateReader, RuntimeGate};
pub use il2cpp_bridge::{BridgeBackend, ExactHandle, enumerate_unity_framework};
pub use main_thread::MainThreadToken;
pub use method_slot::{MethodPointerSlot, MethodRef, RawSlotMemory, SlotMemory, TargetId};
pub use original::{OriginalGuard, OriginalPhase};
