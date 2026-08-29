//! Plugin-author-facing API for the Shiny Song Tools plugin platform.
//!
//! This crate is the only surface functional plugins depend on. It never
//! exposes `PluginManager`, the driver, `Handoff`, owner ledgers, or the
//! runtime bootstrap; every registration goes through the [`AppCtx`]
//! facade, which is backed by an internal host trait implemented by the
//! runtime. Hook targets (ABI wrappers) are authored and reviewed by plugin
//! authors themselves — personal use, trusted boundary, no semver promise.

pub mod config;
pub mod context;
pub mod debug;
pub mod hook;
pub mod host;
pub mod phase;
pub mod plugin;
pub mod route;

pub use config::{DebugConfig, RuntimeConfig};
pub use context::AppCtx;
pub use debug::{
    DebugHandlerError, DebugResponse, DebugServerError, DebugTopicChannel, DebugWireError,
    DebugWireErrorCode, MainDebugTopic,
};
pub use hook::{
    Callback, CallbackCtx, HookBuilder, HookSite, HookTarget, InstalledHook, Published, Unpublished,
};
pub use host::{
    BoxedStartupSystem, BoxedUpdateSystem, MainRouteDrain, MessageRegister, PluginHost,
    ResourceInsert, RouteDirection, RouteRegistration, SharedEnvelope, StartupRun,
};
pub use phase::{
    StartupCtx, StartupFunction, StartupRegistrar, SystemResult, UpdateCtx, UpdateFunction,
};
pub use plugin::Plugin;
pub use route::{
    CallbackBoundedReader, CallbackBoundedWriter, CallbackLatestReader, CallbackLatestWriter,
    CallbackSharedReader, CallbackSharedWriter, MainBoundedReader, MainBoundedWriter,
    MainLatestReader, MainLatestWriter, MainSharedReader, MainSharedWriter,
};

// The single error chain visible to plugin authors.
pub use scsp_core::{
    CallbackPayload, GateReader, HookError, Il2CppError, MainThreadToken, PluginError,
    RestoreError, SendOutcome,
};
