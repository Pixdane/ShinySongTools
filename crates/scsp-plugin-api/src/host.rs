//! Internal host facade and boxed-system plumbing.
//!
//! [`AppCtx`] is a thin wrapper over `&mut dyn PluginHost`; the runtime
//! implements [`PluginHost`] once per owner scope. Generic operations are
//! materialized into boxed closures by `AppCtx` so the host stays
//! object-safe.

use bevy_ecs::message::Message;
use bevy_ecs::world::World;
use scsp_core::{DataRoot, GateReader, MethodResolver, OwnerId, PluginError, RouteId};
use std::any::{Any, TypeId};
use std::sync::Arc;

use crate::config::RuntimeConfig;
use crate::phase::{RestoreAction, SystemResult};

/// Boxed insert operation: performs the conflict check and the insert on the
/// shared AppWorld.
pub type ResourceInsertFn = Box<dyn FnOnce(&mut World) -> Result<(), PluginError> + Send>;
/// Boxed removal operation recorded in the owner ledger for LIFO rollback.
pub type ResourceRemoveFn = Box<dyn FnOnce(&mut World) + Send>;

/// One resource insertion request (build facade or Startup context).
///
/// The insert closure performs the conflict check and the insert on the
/// shared AppWorld. `StartupCtx` queues these and the runner applies them
/// right after the system function returns, so later systems in the same
/// Startup pass see them (no end-of-pass staging). The remove closure is
/// recorded in the owner ledger and runs only if the owner rolls back.
pub struct ResourceInsert {
    pub type_id: TypeId,
    pub type_name: &'static str,
    pub insert: ResourceInsertFn,
    pub remove: ResourceRemoveFn,
}

/// Registers one `Messages<T>` type with the world's `MessageRegistry`
/// (insert if missing + maintenance registration).
pub struct MessageRegister {
    pub register: Box<dyn FnOnce(&mut World) + Send>,
}

/// Direction of a cross-domain route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteDirection {
    /// Update systems write, hook callbacks read.
    MainToCallback,
    /// Hook callbacks write; the `CommandDrain` driver stage delivers into
    /// the main-side receiver.
    CallbackToMain,
}

/// Delivers drained callback→main mailbox contents into the main-side
/// receiver. Implemented by the main-side reader handles; the runtime's
/// `CommandDrain` stage drives it through the route table with the
/// phase-entry watermark.
pub trait MainRouteDrain: Send + Sync {
    /// Drain at most `watermark` items and write them into the receiver in
    /// `world`. Returns the number of items delivered.
    fn drain(&self, world: &mut World, watermark: usize) -> usize;
    /// Items currently visible in the mailbox.
    fn depth(&self) -> usize;
}

/// One route registration request.
pub struct RouteRegistration {
    pub direction: RouteDirection,
    /// Payload type name, for introspection only.
    pub payload: &'static str,
    /// Mailbox semantics label (`latest` / `bounded` / `shared_latest`).
    pub mailbox: &'static str,
    /// Present for callback→main routes: drains the mailbox into the
    /// main-side receiver.
    pub drain: Option<Arc<dyn MainRouteDrain>>,
    /// Present for callback→main routes: registers the `Messages<T>`
    /// receiver in the world.
    pub ensure_receiver: Option<MessageRegister>,
}

/// Borrow bundle handed to a boxed startup system for one run.
pub struct StartupRun<'a> {
    pub world: &'a mut World,
    pub main: &'a crate::MainThreadToken,
    pub pending: &'a mut Vec<ResourceInsert>,
    pub restore_sink: &'a mut Vec<RestoreAction>,
}

pub trait StartupSystemRunner: Send {
    fn run(&mut self, run: StartupRun<'_>) -> SystemResult;
}

pub trait UpdateSystemRunner: Send {
    fn run(&mut self, world: &mut World, main: &crate::MainThreadToken) -> SystemResult;
}

/// A startup system ready for the driver, already lazy: its param state is
/// created on first run (the driver's lazy-initialize rule).
pub struct BoxedStartupSystem(Box<dyn StartupSystemRunner>);

/// An update system ready for the driver.
pub struct BoxedUpdateSystem(Box<dyn UpdateSystemRunner>);

impl BoxedStartupSystem {
    pub fn new(runner: Box<dyn StartupSystemRunner>) -> Self {
        Self(runner)
    }

    pub fn run(
        &mut self,
        world: &mut World,
        main: &crate::MainThreadToken,
        pending: &mut Vec<ResourceInsert>,
        restore_sink: &mut Vec<RestoreAction>,
    ) -> SystemResult {
        self.0.run(StartupRun {
            world,
            main,
            pending,
            restore_sink,
        })
    }
}

impl BoxedUpdateSystem {
    pub fn new(runner: Box<dyn UpdateSystemRunner>) -> Self {
        Self(runner)
    }

    pub fn run(&mut self, world: &mut World, main: &crate::MainThreadToken) -> SystemResult {
        self.0.run(world, main)
    }
}

/// Envelope written into `Messages` for `shared_latest` callback→main
/// routes: the payload is owned structured data, carried by `Arc`.
pub struct SharedEnvelope<T: Send + Sync + 'static>(pub Arc<T>);

impl<T: Send + Sync + 'static> Message for SharedEnvelope<T> {}

/// The owner-scoped host the runtime provides to `AppCtx`.
pub trait PluginHost {
    fn owner_id(&mut self) -> OwnerId;
    fn runtime_gate_reader(&mut self) -> GateReader;
    fn owner_gate_reader(&mut self) -> GateReader;
    fn config(&mut self) -> RuntimeConfig;
    fn data_root(&mut self) -> DataRoot;
    /// Method resolver for hook installation; absent until the runtime
    /// backend is ready (fixtures install mocks).
    fn method_resolver(&mut self) -> Option<Arc<dyn MethodResolver>>;

    /// Conflict-checked direct insert into the shared AppWorld, recorded in
    /// the owner ledger on success.
    fn insert_resource_dyn(&mut self, insert: ResourceInsert) -> Result<(), PluginError>;
    fn register_message_dyn(&mut self, register: MessageRegister) -> Result<(), PluginError>;
    fn add_startup_system_dyn(&mut self, system: BoxedStartupSystem);
    fn add_update_system_dyn(&mut self, system: BoxedUpdateSystem);
    /// At most one container per owner.
    fn register_container_dyn(
        &mut self,
        container: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), PluginError>;
    fn register_route_dyn(&mut self, route: RouteRegistration) -> Result<RouteId, PluginError>;
    /// Build-phase restore registration (AnyThread only; Startup systems may
    /// additionally register MainThread actions through `StartupRegistrar`).
    fn register_restore_any_thread(&mut self, action: RestoreAction);
    /// Register one typed debug topic (name duplicates fail the build).
    fn register_debug_topic_dyn(
        &mut self,
        registration: crate::debug::DebugTopicRegistration,
    ) -> Result<(), PluginError>;
    /// Live lookup over registered topics (for the DebugPlugin dispatch).
    fn topic_registry_handle(&mut self) -> Arc<dyn crate::debug::DebugTopicLookup>;
    /// Runtime introspection data for the built-in `runtime.*` topics.
    fn introspection_handle(&mut self) -> Option<Arc<dyn crate::debug::DebugIntrospection>>;
}
