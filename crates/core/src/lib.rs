#![doc = include_str!("../README.md")]
#![doc = "\n\n---\n\n"]
#![doc = include_str!("../PLUGIN_API.md")]

pub mod backend;
pub mod base;
pub mod callback_il2cpp;
pub mod container;
#[cfg(target_arch = "aarch64")]
pub mod entry_patch;
pub mod error;
pub mod event;
pub mod gate;
pub mod il2cpp_bridge;
pub mod il2cpp_recon;
pub mod main_thread;
pub mod method_slot;
pub mod original;
pub mod plugin_api;

extern crate self as corelib;

pub use backend::{
    AttachGuard, DomainHandle, Il2CppApi, ImageHandle, ImageIdentity, MethodResolver,
    ResolvedMethod, RuntimeIdentity,
};
pub use base::{DataRoot, OwnerId, RouteId, TopicId};
pub use callback_il2cpp::CallbackIl2Cpp;
pub use container::{BoundedQueue, CallbackPayload, LatestCell, SendOutcome, SharedSlot, SlotBusy};
#[cfg(target_arch = "aarch64")]
pub use entry_patch::EntryPatchMemory;
pub use error::{HookError, Il2CppError, PluginError, RestoreError};
pub use event::{
    CALLBACK_EVENT_QUEUE_CAPACITY, CallbackObservability, CompactEvent, CompactEventCode,
    CompactLevel, CompactOwnerId, CompactSiteId, process_event_queue,
};
pub use gate::{GateReader, RuntimeGate};
pub use il2cpp_bridge::{BridgeBackend, ExactHandle, enumerate_unity_framework};
pub use main_thread::MainThreadToken;
pub use method_slot::{
    HookMechanism, MethodPointerSlot, MethodRef, RawSlotMemory, SlotMemory, TargetId,
};
pub use original::{OriginalGuard, OriginalPhase};
pub use plugin_api::*;
pub use plugin_api::{config, context, debug, hook, host, phase, plugin, route};
