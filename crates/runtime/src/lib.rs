#![doc = include_str!("../README.md")]
#![doc = "\n\n---\n\n"]
#![doc = include_str!("../APP.md")]
#![doc = "\n\n---\n\n"]
#![doc = include_str!("../BOOTSTRAP.md")]
#![doc = "\n\n---\n\n"]
#![doc = include_str!("../FFI.md")]
#![doc = "\n\n---\n\n"]
#![doc = include_str!("../ARCHITECTURE.md")]

pub mod app;
pub mod bootstrap;
pub mod config;
pub mod core_state;
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
