//! Shiny Song Tools runtime.
//!
//! This crate carries two halves of the design:
//!
//! * **App / driver** (this phase): the `Send` composition root, the
//!   owner-scoped [`PluginManager`] with its resource ledger and restore
//!   actions, and the fixed driver (MessageMaintenance → CommandDrain →
//!   plugin Update). See `docs/plugin-system.md`.
//! * **bootstrap / scheduler** (next phase): `scsp_start`, the bootstrap
//!   worker, the readiness ladder, `Handoff`, the Unity main-thread TLS, and
//!   the `SchedulerFrame` panic boundary. See `docs/runtime-crate.md`.
//!
//! The staticlib target (`libshiny_song_tools.a`) is produced by this crate
//! directly: the design collapses the former plugin-system/runtime split
//! into one package, and a separate FFI crate would add a link unit without
//! adding a consumer boundary.

pub mod app;
pub mod bootstrap;
pub mod config;
pub mod core_state;
#[cfg(feature = "debug")]
pub mod debug;
pub mod ffi;
pub mod gate;
pub mod handoff;
pub mod host;
pub mod introspection;
pub mod inventory;
pub mod manager;
pub mod observability;
pub mod routes;
pub mod scheduler;

pub use app::{App, StartupReport, UpdateReport};
pub use core_state::{TopicDomain, TopicEntry, TopicRegistry};
pub use gate::PluginGate;
pub use handoff::{Handoff, HandoffTake};
pub use inventory::{PluginInventory, PluginSummary};
pub use manager::{PluginState, ResourceLedgerEntry};
pub use routes::{RouteEntry, RouteTable};
pub use scheduler::{
    Il2CppObjectOpaque, LateUpdateFn, MethodInfoOpaque, SchedulerContext, SchedulerHook,
    ThreadIdentityCheck, scheduler_context,
};

pub use plugins as plugin_api;
