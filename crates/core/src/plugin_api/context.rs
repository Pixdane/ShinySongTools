//! `AppCtx`: the owner-scoped facade plugins configure themselves through.
//!
//! Every operation carries the current owner implicitly; plugin code cannot
//! reach the `World`, `PluginManager`, or slot write access directly. The
//! facade forwards to the internal host trait (implemented by the runtime)
//! after materializing generic operations into boxed closures.

use bevy_ecs::message::Message;
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use corelib::{DataRoot, GateReader, OwnerId, PluginError};
use std::any::Any;
use std::sync::Arc;

use crate::RestoreError;
use crate::plugin_api::config::RuntimeConfig;
use crate::plugin_api::debug::{DebugIntrospection, DebugTopicLookup};
use crate::plugin_api::hook::{HookBuilder, HookSite, Unpublished};
use crate::plugin_api::host::{
    BoxedStartupSystem, BoxedUpdateSystem, MessageRegister, PluginHost, ResourceInsert,
    RouteDirection, RouteRegistration,
};
use crate::plugin_api::phase::{
    LazyStartupSystem, LazyUpdateSystem, RestoreAction, StartupFunction, UpdateFunction,
};
use crate::plugin_api::route::{
    CallbackBoundedReader, CallbackBoundedWriter, CallbackLatestReader, CallbackLatestWriter,
    CallbackSharedReader, CallbackSharedWriter, MainBoundedReader, MainBoundedWriter,
    MainLatestReader, MainLatestWriter, MainSharedReader, MainSharedWriter,
};
use std::marker::PhantomData;

/// Restricted facade for one plugin's build / startup operations.
///
/// The `host` seam is `pub(crate)`: plugin code cannot bypass [`AppCtx`]
/// methods, so it cannot register raw boxed runners (direct `&mut World`
/// access), reach the method resolver outside the hook typestate, or touch
/// the owner ledger. The runtime crate drives the facade through the public
/// constructor and methods only.
pub struct AppCtx<'host> {
    pub(crate) host: &'host mut dyn PluginHost,
}

impl<'host> AppCtx<'host> {
    pub fn new(host: &'host mut dyn PluginHost) -> Self {
        Self { host }
    }

    /// Owner id of the plugin this context belongs to.
    #[must_use]
    pub fn owner_id(&mut self) -> OwnerId {
        self.host.owner_id()
    }

    /// Reader side of the process runtime gate. Infrastructure handle for
    /// the built-in DebugPlugin transport, not a plugin message surface.
    #[must_use]
    pub fn runtime_gate_reader(&mut self) -> GateReader {
        self.host.runtime_gate_reader()
    }

    /// Live lookup over registered debug topics. Infrastructure handle for
    /// the built-in DebugPlugin dispatch.
    #[must_use]
    pub fn debug_topic_registry(&mut self) -> Arc<dyn DebugTopicLookup> {
        self.host.topic_registry_handle()
    }

    /// Runtime introspection snapshots for the built-in `runtime.*` topics.
    #[must_use]
    pub fn runtime_introspection(&mut self) -> Option<Arc<dyn DebugIntrospection>> {
        self.host.introspection_handle()
    }

    /// Typed configuration snapshot (already fail-closed by the runtime).
    #[must_use]
    pub fn config(&mut self) -> RuntimeConfig {
        self.host.config()
    }

    /// The game sandbox Documents root.
    #[must_use]
    pub fn data_root(&mut self) -> DataRoot {
        self.host.data_root()
    }

    /// Directly insert a typed resource into the shared AppWorld and record
    /// it in the owner ledger. A conflicting type (already present, from any
    /// owner including this one) fails with
    /// [`PluginError::ResourceConflict`] — never overwritten.
    pub fn insert_resource<T: Resource>(&mut self, value: T) -> Result<(), PluginError> {
        let type_name = core::any::type_name::<T>();
        self.host.insert_resource_dyn(ResourceInsert {
            type_id: core::any::TypeId::of::<T>(),
            type_name,
            insert: Box::new(move |world: &mut World| {
                if world.contains_resource::<T>() {
                    return Err(PluginError::ResourceConflict(type_name));
                }
                world.insert_resource(value);
                Ok(())
            }),
            remove: Box::new(|world: &mut World| {
                world.remove_resource::<T>();
            }),
        })
    }

    /// Register a Startup system (runs once, first outer LateUpdate).
    /// Cross-phase registration is a compile error: only functions whose
    /// first parameter is `StartupCtx<'_>` implement the accepted bound.
    pub fn add_startup_system<Marker: 'static, F: StartupFunction<Marker>>(&mut self, f: F) {
        self.host
            .add_startup_system_dyn(BoxedStartupSystem::new(Box::new(LazyStartupSystem {
                f,
                state: None,
                marker: PhantomData,
            })));
    }

    /// Register an Update system (runs every outer LateUpdate).
    pub fn add_update_system<Marker: 'static, F: UpdateFunction<Marker>>(&mut self, f: F) {
        self.host
            .add_update_system_dyn(BoxedUpdateSystem::new(Box::new(LazyUpdateSystem {
                f,
                state: None,
                marker: PhantomData,
            })));
    }

    /// Register the plugin's single `CallbackSiteContainer`. Its `Arc`
    /// becomes the callback-visible state; a second registration fails.
    pub fn register_container<C: Send + Sync + 'static>(
        &mut self,
        container: C,
    ) -> Result<Arc<C>, PluginError> {
        let arc: Arc<C> = Arc::new(container);
        self.host
            .register_container_dyn(arc.clone() as Arc<dyn Any + Send + Sync>)?;
        Ok(arc)
    }

    /// Register an `AnyThread` restore action during build (Startup systems
    /// may also register `MainThread` actions through `StartupRegistrar`).
    pub fn register_restore_any_thread(
        &mut self,
        action: impl FnOnce() -> Result<(), RestoreError> + Send + 'static,
    ) {
        self.host
            .register_restore_any_thread(RestoreAction::AnyThread(Box::new(action)));
    }

    // -- cross-domain routes ------------------------------------------------

    /// main → callback, `latest` semantics (payload: `Copy`).
    pub fn main_to_callback_latest<T: crate::CallbackPayload>(
        &mut self,
    ) -> Result<(MainLatestWriter<T>, CallbackLatestReader<T>), PluginError> {
        let core = Arc::new(corelib::LatestCell::new());
        self.host.register_route_dyn(RouteRegistration {
            direction: RouteDirection::MainToCallback,
            payload: core::any::type_name::<T>(),
            mailbox: "latest",
            drain: None,
            ensure_receiver: None,
        })?;
        Ok((
            MainLatestWriter::new(Arc::clone(&core)),
            CallbackLatestReader::new(core),
        ))
    }

    /// main → callback, `bounded::<N>` semantics (payload: `Copy`).
    pub fn main_to_callback_bounded<T: crate::CallbackPayload, const N: usize>(
        &mut self,
    ) -> Result<(MainBoundedWriter<T, N>, CallbackBoundedReader<T, N>), PluginError> {
        let core = Arc::new(corelib::BoundedQueue::new());
        self.host.register_route_dyn(RouteRegistration {
            direction: RouteDirection::MainToCallback,
            payload: core::any::type_name::<T>(),
            mailbox: "bounded",
            drain: None,
            ensure_receiver: None,
        })?;
        Ok((
            MainBoundedWriter::new(Arc::clone(&core)),
            CallbackBoundedReader::new(core),
        ))
    }

    /// main → callback, `shared_latest` semantics (owned data, no `Copy`).
    pub fn main_to_callback_shared<T: Send + Sync + 'static>(
        &mut self,
    ) -> Result<(MainSharedWriter<T>, CallbackSharedReader<T>), PluginError> {
        let core = Arc::new(corelib::SharedSlot::new());
        self.host.register_route_dyn(RouteRegistration {
            direction: RouteDirection::MainToCallback,
            payload: core::any::type_name::<T>(),
            mailbox: "shared_latest",
            drain: None,
            ensure_receiver: None,
        })?;
        Ok((
            MainSharedWriter::new(Arc::clone(&core)),
            CallbackSharedReader::new(core),
        ))
    }

    /// callback → main, `latest` semantics. Drained by the `CommandDrain`
    /// stage into `Messages<T>`; consume on the main side with the standard
    /// `MessageReader<T>` system param.
    pub fn callback_to_main_latest<T: crate::CallbackPayload + Message>(
        &mut self,
    ) -> Result<(CallbackLatestWriter<T>, MainLatestReader<T>), PluginError> {
        let core = Arc::new(corelib::LatestCell::new());
        self.host.register_route_dyn(RouteRegistration {
            direction: RouteDirection::CallbackToMain,
            payload: core::any::type_name::<T>(),
            mailbox: "latest",
            drain: Some(Arc::new(MainLatestReader::<T>::new(Arc::clone(&core)))),
            ensure_receiver: Some(MessageRegister {
                register: Box::new(|world: &mut World| {
                    bevy_ecs::message::MessageRegistry::register_message::<T>(world);
                }),
            }),
        })?;
        Ok((
            CallbackLatestWriter::new(Arc::clone(&core)),
            MainLatestReader::new(core),
        ))
    }

    /// callback → main, `bounded::<N>` semantics.
    pub fn callback_to_main_bounded<T: crate::CallbackPayload + Message, const N: usize>(
        &mut self,
    ) -> Result<(CallbackBoundedWriter<T, N>, MainBoundedReader<T, N>), PluginError> {
        let core = Arc::new(corelib::BoundedQueue::new());
        self.host.register_route_dyn(RouteRegistration {
            direction: RouteDirection::CallbackToMain,
            payload: core::any::type_name::<T>(),
            mailbox: "bounded",
            drain: Some(Arc::new(MainBoundedReader::<T, N>::new(Arc::clone(&core)))),
            ensure_receiver: Some(MessageRegister {
                register: Box::new(|world: &mut World| {
                    bevy_ecs::message::MessageRegistry::register_message::<T>(world);
                }),
            }),
        })?;
        Ok((
            CallbackBoundedWriter::new(Arc::clone(&core)),
            MainBoundedReader::new(core),
        ))
    }

    /// callback → main, `shared_latest` semantics. Delivered as
    /// [`crate::plugin_api::host::SharedEnvelope<T>`] messages.
    pub fn callback_to_main_shared<T: Send + Sync + 'static + Message>(
        &mut self,
    ) -> Result<(CallbackSharedWriter<T>, MainSharedReader<T>), PluginError> {
        let core = Arc::new(corelib::SharedSlot::new());
        self.host.register_route_dyn(RouteRegistration {
            direction: RouteDirection::CallbackToMain,
            payload: core::any::type_name::<T>(),
            mailbox: "shared_latest",
            drain: Some(Arc::new(MainSharedReader::<T>::new(Arc::clone(&core)))),
            ensure_receiver: Some(MessageRegister {
                register: Box::new(|world: &mut World| {
                    bevy_ecs::message::MessageRegistry::register_message::<
                        crate::plugin_api::host::SharedEnvelope<T>,
                    >(world);
                }),
            }),
        })?;
        Ok((
            CallbackSharedWriter::new(Arc::clone(&core)),
            MainSharedReader::new(core),
        ))
    }

    // -- hooks ---------------------------------------------------------------

    /// Start the publish → install chain for this target's static site.
    pub fn hook<T: crate::plugin_api::hook::HookTarget, C: Send + Sync + 'static>(
        &mut self,
        site: &'static HookSite<T, C>,
    ) -> HookBuilder<'_, 'host, T, C, Unpublished> {
        HookBuilder {
            ctx: self,
            site,
            container: None,
            state: core::marker::PhantomData,
        }
    }
}
