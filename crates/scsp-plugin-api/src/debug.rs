//! Typed debug topics (docs/debug-diagnostics-logging.md).
//!
//! A plugin registers a main-domain topic with a typed handler; the
//! registration wires the topic registry entry (name → owner gate readers,
//! request inbox, response outbox, decode vtable) and registers the owner's
//! handler system as an ordinary Update system. The plugin author writes
//! only the handler.
//!
//! Dispatch flow (main domain): DebugPlugin's Update system decodes wire
//! requests, checks the runtime + owner gates, and queues typed payloads in
//! the topic channel; the owner's handler system (this module's auto system)
//! drains the channel, calls the handler with its own `SystemParam`s, and
//! writes encoded responses to the channel outbox; the DebugPlugin drains
//! the outbox back to the wire. The callback domain (SharedSlot relay) is
//! layered on the same channel type later.

use bevy_ecs::system::{SystemParam, SystemState};
use bevy_ecs::world::World;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any::{Any, TypeId};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::PluginError;
use crate::context::AppCtx;
use crate::host::BoxedUpdateSystem;
use crate::phase::{SystemResult, UpdateCtx};

/// A typed main-domain debug topic.
pub trait MainDebugTopic: 'static {
    /// Wire method name (e.g. `fps.set`).
    const NAME: &'static str;
    type Request: DeserializeOwned + Send + Sync + 'static;
    type Response: Serialize + Send + 'static;
}

/// Handler failure vocabulary (maps to `handler_error`).
#[derive(Debug)]
pub struct DebugHandlerError(pub String);

/// One wire request envelope; `id` travels with the typed payload through
/// the type-erased channel.
pub struct DebugQueuedRequest {
    pub id: serde_json::Value,
    pub payload: Arc<dyn Any + Send + Sync>,
}

/// Wire response envelope fragment owned by the registry plumbing.
pub struct DebugResponse {
    pub id: serde_json::Value,
    pub result: Result<serde_json::Value, DebugWireError>,
}

/// Server-side error codes (docs wire table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugWireErrorCode {
    MethodNotFound,
    InvalidParams,
    ServerError(DebugServerError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugServerError {
    RuntimeUnavailable,
    QueueFull,
    PayloadTooLarge,
    PluginUnavailable,
    HandlerError,
    InternalError,
}

impl DebugServerError {
    #[must_use]
    pub fn code_name(self) -> &'static str {
        match self {
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::QueueFull => "queue_full",
            Self::PayloadTooLarge => "payload_too_large",
            Self::PluginUnavailable => "plugin_unavailable",
            Self::HandlerError => "handler_error",
            Self::InternalError => "internal_error",
        }
    }
}

#[derive(Debug)]
pub struct DebugWireError {
    pub code: DebugWireErrorCode,
    pub message: String,
}

/// Registry-side state shared between the DebugPlugin dispatch and the
/// owner's handler system. Request payloads are type-erased on the wire side
/// and downcast inside the handler system (which knows `T`).
pub struct DebugTopicChannel {
    /// Wire-side queue: decoded requests awaiting the owner handler system.
    pub inbox: Mutex<VecDeque<DebugQueuedRequest>>,
    /// Handler-side queue: encoded responses awaiting the wire.
    pub outbox: Mutex<Vec<DebugResponse>>,
    /// Pending requests awaiting a response (bounded).
    pub pending: AtomicUsize,
    /// Owner gate reader, checked by the DebugPlugin dispatch.
    pub owner_gate: scsp_core::GateReader,
    /// Runtime gate reader, checked by the DebugPlugin dispatch.
    pub runtime_gate: scsp_core::GateReader,
}

/// Bounded pending ceiling per topic (docs: pending bounded).
pub const DEBUG_MAX_PENDING: usize = 16;

impl DebugTopicChannel {
    /// Queue a decoded request. `QueueFull` when the pending ceiling is
    /// reached.
    pub fn enqueue(
        &self,
        id: serde_json::Value,
        payload: Arc<dyn Any + Send + Sync>,
    ) -> Result<(), DebugWireError> {
        if self.pending.load(Ordering::Acquire) >= DEBUG_MAX_PENDING {
            return Err(DebugWireError {
                code: DebugWireErrorCode::ServerError(DebugServerError::QueueFull),
                message: "topic pending capacity reached".to_owned(),
            });
        }
        self.inbox
            .lock()
            .expect("inbox lock")
            .push_back(DebugQueuedRequest { id, payload });
        self.pending.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Whether new requests may be dispatched (both gates open).
    #[must_use]
    pub fn dispatchable(&self) -> bool {
        self.runtime_gate.is_open() && self.owner_gate.is_open()
    }

    /// Release one pending slot (called when a response was produced).
    pub fn leave_pending(&self) {
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Decode a wire params value into the topic's typed request, boxed for the
/// type-erased channel.
pub type DebugDecodeFn =
    Arc<dyn Fn(&serde_json::Value) -> Result<Arc<dyn Any + Send + Sync>, String> + Send + Sync>;

/// Registration request handed to the host.
pub struct DebugTopicRegistration {
    pub name: &'static str,
    pub channel: Arc<DebugTopicChannel>,
    pub decode: DebugDecodeFn,
}

/// One topic view handed to the DebugPlugin dispatch (type-erased).
#[derive(Clone)]
pub struct DebugTopicView {
    pub name: &'static str,
    pub owner: scsp_core::OwnerId,
    pub channel: Arc<DebugTopicChannel>,
    pub decode: DebugDecodeFn,
}

/// Live lookup over registered topics (the registry grows as plugins build).
pub trait DebugTopicLookup: Send + Sync {
    fn topics(&self) -> Vec<DebugTopicView>;
}

/// Runtime introspection data for the built-in `runtime.*` topics. The
/// runtime supplies the implementation; snapshots are read-only.
pub trait DebugIntrospection: Send + Sync {
    /// `runtime.plugins` / `runtime.gates` / `runtime.info` payloads.
    fn introspect(&self, method: &str) -> Option<serde_json::Value>;
}

/// Typed handler over one topic: capability context, request, and its own
/// `SystemParam`s (a single param — use a tuple for several). The `Marker`
/// parameter (a `fn(...)` type) distinguishes arities.
pub trait MainDebugHandler<T: MainDebugTopic, Marker>: Send + Sync + 'static {
    type Param: SystemParam + 'static;
    fn handle(
        &self,
        ctx: UpdateCtx<'_>,
        request: T::Request,
        params: bevy_ecs::system::SystemParamItem<'_, '_, Self::Param>,
    ) -> Result<T::Response, DebugHandlerError>;
}

impl AppCtx<'_> {
    /// Register a main-domain debug topic with a typed handler.
    ///
    /// Auto-wires: the registry entry (name → owner, gate readers, channel,
    /// decode vtable) and this topic's handler system as an ordinary Update
    /// system of this plugin. Re-registering a name fails the build.
    pub fn register_main_debug<Marker: 'static, T, H>(
        &mut self,
        handler: H,
    ) -> Result<(), PluginError>
    where
        T: MainDebugTopic,
        H: MainDebugHandler<T, Marker>,
    {
        let channel = Arc::new(DebugTopicChannel {
            inbox: Mutex::new(VecDeque::new()),
            outbox: Mutex::new(Vec::new()),
            pending: AtomicUsize::new(0),
            owner_gate: self.host.owner_gate_reader(),
            runtime_gate: self.host.runtime_gate_reader(),
        });
        let decode: DebugDecodeFn = Arc::new(|params: &serde_json::Value| {
            serde_json::from_value::<T::Request>(params.clone())
                .map(|request| {
                    let boxed: Arc<dyn Any + Send + Sync> = Arc::new(request);
                    boxed
                })
                .map_err(|e| e.to_string())
        });

        self.host.register_debug_topic_dyn(DebugTopicRegistration {
            name: T::NAME,
            channel: Arc::clone(&channel),
            decode,
        })?;

        // The auto-registered handler system: lazily initialized by the
        // driver like every other boxed system.
        let system = DebugHandlerSystem::<T, Marker, H> {
            handler,
            channel,
            state: None,
            marker: core::marker::PhantomData,
        };
        self.host
            .add_update_system_dyn(BoxedUpdateSystem::new(Box::new(system)));
        Ok(())
    }
}

/// The owner-side handler system (auto-registered by
/// `register_main_debug`).
pub(crate) struct DebugHandlerSystem<T: MainDebugTopic, Marker, H: MainDebugHandler<T, Marker>> {
    handler: H,
    channel: Arc<DebugTopicChannel>,
    state: Option<SystemState<H::Param>>,
    marker: core::marker::PhantomData<fn() -> T>,
}

impl<T: MainDebugTopic, Marker, H: MainDebugHandler<T, Marker>> crate::host::UpdateSystemRunner
    for DebugHandlerSystem<T, Marker, H>
{
    fn run(&mut self, world: &mut World, main: &crate::MainThreadToken) -> SystemResult {
        // Fast path: nothing queued.
        let requests: Vec<DebugQueuedRequest> = {
            let mut inbox = self.channel.inbox.lock().expect("inbox lock");
            inbox.drain(..).collect()
        };
        if requests.is_empty() {
            return Ok(());
        }
        if self.state.is_none() {
            self.state = Some(SystemState::new(world));
        }
        let state = self.state.as_mut().expect("state initialized above");
        let ctx = UpdateCtx { main };
        for queued in requests {
            let result: Result<serde_json::Value, DebugWireError> = (|| {
                // Exactly one handler consumes the request: take the Arc
                // contents without requiring Clone.
                // The request was boxed on enqueue; this system is the sole
                // consumer, so the Arc unwraps without a Clone bound.
                let typed: T::Request = match queued.payload.downcast::<T::Request>() {
                    Ok(payload) => match Arc::try_unwrap(payload) {
                        Ok(value) => value,
                        Err(_) => {
                            return Err(DebugWireError {
                                code: DebugWireErrorCode::ServerError(
                                    DebugServerError::InternalError,
                                ),
                                message: "queued request has extra references".to_owned(),
                            });
                        }
                    },
                    Err(_) => {
                        return Err(DebugWireError {
                            code: DebugWireErrorCode::ServerError(DebugServerError::InternalError),
                            message: "queued request type mismatch".to_owned(),
                        });
                    }
                };
                let params = state.get_mut(world).map_err(|_| DebugWireError {
                    code: DebugWireErrorCode::ServerError(DebugServerError::PluginUnavailable),
                    message: "handler params no longer valid".to_owned(),
                })?;
                let response = self.handler.handle(ctx, typed, params);
                state.apply(world);
                match response {
                    Ok(value) => serde_json::to_value(&value).map_err(|e| DebugWireError {
                        code: DebugWireErrorCode::ServerError(DebugServerError::InternalError),
                        message: e.to_string(),
                    }),
                    Err(DebugHandlerError(message)) => Err(DebugWireError {
                        code: DebugWireErrorCode::ServerError(DebugServerError::HandlerError),
                        message,
                    }),
                }
            })();
            {
                let mut outbox = self.channel.outbox.lock().expect("outbox lock");
                outbox.push(DebugResponse {
                    id: queued.id,
                    result,
                });
            }
            self.channel.leave_pending();
        }
        Ok(())
    }
}

macro_rules! impl_main_debug_handler {
    ($($param: ident),*) => {
        #[allow(non_snake_case, reason = "generated generic parameter names come from the macro invocation")]
        impl<F, T, $($param: SystemParam + 'static),*> MainDebugHandler<T, fn($($param,)*)> for F
        where
            T: MainDebugTopic,
            F: Send + Sync + 'static,
            F: for<'a> Fn(
                UpdateCtx<'a>,
                T::Request,
                $(bevy_ecs::system::SystemParamItem<$param>),*
            ) -> Result<T::Response, DebugHandlerError>,
        {
            type Param = ($($param,)*);

            fn handle(
                &self,
                ctx: UpdateCtx<'_>,
                request: T::Request,
                params: bevy_ecs::system::SystemParamItem<'_, '_, Self::Param>,
            ) -> Result<T::Response, DebugHandlerError> {
                let ($($param,)*) = params;
                (self)(ctx, request, $($param,)*)
            }
        }
    };
}

impl_main_debug_handler!();
impl_main_debug_handler!(P0);
impl_main_debug_handler!(P0, P1);
impl_main_debug_handler!(P0, P1, P2);
impl_main_debug_handler!(P0, P1, P2, P3);

const _: Option<TypeId> = None;
