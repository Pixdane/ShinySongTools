//! Plugin-facing API layer.
//!
//! This layer lives in the core crate so functional plugins share one stable
//! dependency boundary with the platform primitives. Runtime owns the host
//! implementation; plugins only consume the facade and typed capabilities.

pub mod config;
pub mod context;
pub mod debug;
pub mod hook;
pub mod host;
pub mod phase;
pub mod plugin;
pub mod route;

pub use config::{DebugConfig, FpsConfig, RuntimeConfig};
pub use context::AppCtx;
pub use debug::{
    CallbackDebugEndpoints, CallbackDebugTopic, DebugDecodeFn, DebugHandlerError,
    DebugIntrospection, DebugResponse, DebugServerError, DebugTopicChannel, DebugTopicLookup,
    DebugTopicRegistration, DebugTopicView, DebugWireError, DebugWireErrorCode, MainDebugTopic,
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
