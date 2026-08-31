//! Typed debug topics (debug crate Rustdoc).
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
//! the outbox back to the wire.
//!
//! Dispatch flow (callback domain): the same channel carries wire↔owner
//! traffic; the owner's auto-registered relay system forwards one queued
//! request at a time into a `SharedSlot` inside the owner's
//! `CallbackSiteContainer` (never overwriting an unconsumed request), the
//! owner's hook callback handles it on natural entry via
//! [`CallbackDebugEndpoints::handle_pending`] (one request per entry), and
//! the relay picks the response up and pairs it FIFO with the delivered
//! request ids.

use bevy_ecs::system::{SystemParam, SystemState};
use bevy_ecs::world::World;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::PluginError;
use crate::context::AppCtx;
use crate::plugin_api::hook::CallbackCtx;
use crate::plugin_api::host::BoxedUpdateSystem;
use crate::plugin_api::phase::{SystemResult, UpdateCtx};

/// A typed main-domain debug topic.
pub trait MainDebugTopic: 'static {
    /// Wire method name (e.g. `unlock_fps.set`).
    const NAME: &'static str;
    type Request: DeserializeOwned + Send + Sync + 'static;
    type Response: Serialize + Send + 'static;
}

/// Handler failure vocabulary (maps to `handler_error`).
#[derive(Debug)]
pub struct DebugHandlerError(pub String);

/// One wire request envelope; `id` travels with the typed payload through
/// the type-erased channel. `generation` identifies the connection the
/// request arrived on: responses carry it back so the transport can drop
/// answers whose connection is gone instead of feeding them to the next
/// client.
pub struct DebugQueuedRequest {
    pub id: serde_json::Value,
    pub payload: Arc<dyn Any + Send + Sync>,
    pub generation: u64,
}

/// Wire response envelope fragment owned by the registry plumbing.
pub struct DebugResponse {
    pub id: serde_json::Value,
    pub result: Result<serde_json::Value, DebugWireError>,
    /// Connection generation of the request this answer belongs to.
    pub generation: u64,
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
    pub owner_gate: corelib::GateReader,
    /// Runtime gate reader, checked by the DebugPlugin dispatch.
    pub runtime_gate: corelib::GateReader,
}

/// Bounded pending ceiling per topic (docs: pending bounded).
pub const DEBUG_MAX_PENDING: usize = 16;

impl DebugTopicChannel {
    /// Queue a decoded request. `QueueFull` when the pending ceiling is
    /// reached.
    ///
    /// Single-producer by design: the only caller is the DebugPlugin's
    /// dispatch system on the main thread, so the check-then-increment below
    /// cannot interleave with another enqueue. Consumers only decrement
    /// (`leave_pending`), never raise the counter.
    pub fn enqueue(
        &self,
        id: serde_json::Value,
        payload: Arc<dyn Any + Send + Sync>,
        generation: u64,
    ) -> Result<(), DebugWireError> {
        if self.pending.load(Ordering::Acquire) >= DEBUG_MAX_PENDING {
            return Err(DebugWireError {
                code: DebugWireErrorCode::ServerError(DebugServerError::QueueFull),
                message: "topic pending capacity reached".to_owned(),
            });
        }
        self.inbox
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(DebugQueuedRequest {
                id,
                payload,
                generation,
            });
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
    /// `true` for callback-domain topics (handler runs on natural hook
    /// entry through the container's shared slots), `false` for main-domain
    /// topics (handler runs as an Update system).
    pub callback_domain: bool,
}

/// One topic view handed to the DebugPlugin dispatch (type-erased).
#[derive(Clone)]
pub struct DebugTopicView {
    pub name: &'static str,
    pub owner: corelib::OwnerId,
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
            callback_domain: false,
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

impl<T: MainDebugTopic, Marker, H: MainDebugHandler<T, Marker>>
    crate::plugin_api::host::UpdateSystemRunner for DebugHandlerSystem<T, Marker, H>
{
    fn run(&mut self, world: &mut World, main: &crate::MainThreadToken) -> SystemResult {
        // Fast path: nothing queued.
        let requests: Vec<DebugQueuedRequest> = {
            let mut inbox = self
                .channel
                .inbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut iter = requests.into_iter();
        while let Some(queued) = iter.next() {
            // Per-request panic boundary: a handler panic answers the
            // current request `plugin_unavailable`, returns the undelivered
            // remainder to the inbox, and fails this system run — the
            // driver retires the owner, whose retirement path answers the
            // re-queued requests (docs: handler panic → owner-local 禁用).
            // Nothing is lost and no pending slot leaks.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
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
            }));
            let result = match outcome {
                Ok(result) => result,
                Err(_) => {
                    {
                        let mut inbox = self
                            .channel
                            .inbox
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        // Push back in reverse so FIFO order is preserved.
                        for leftover in iter.by_ref().rev() {
                            inbox.push_front(leftover);
                        }
                    }
                    self.channel
                        .outbox
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(DebugResponse {
                            id: queued.id,
                            generation: queued.generation,
                            result: Err(DebugWireError {
                                code: DebugWireErrorCode::ServerError(
                                    DebugServerError::PluginUnavailable,
                                ),
                                message: "debug handler panicked".to_owned(),
                            }),
                        });
                    self.channel.leave_pending();
                    return Err(PluginError::Message(
                        "debug handler panicked; retiring owner debug routes",
                    ));
                }
            };
            self.channel
                .outbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(DebugResponse {
                    id: queued.id,
                    generation: queued.generation,
                    result,
                });
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

// ---------------------------------------------------------------------------
// callback domain: SharedSlot relay (docs: owner handler system → container
// SharedSlot → callback natural entry → response back through the relay)
// ---------------------------------------------------------------------------

/// A typed callback-domain debug topic. The request/response travel through
/// `SharedSlot`s inside the owner's `CallbackSiteContainer`; the owner's hook
/// callback handles requests on natural entry (at most one per entry).
pub trait CallbackDebugTopic: 'static {
    /// Wire method name (e.g. `fps.probe`).
    const NAME: &'static str;
    type Request: DeserializeOwned + Send + Sync + 'static;
    type Response: Serialize + Send + Sync + 'static;
}

/// Callback-side endpoints for one callback-domain topic. Put these into the
/// plugin's `CallbackSiteContainer`; the hook handler processes pending
/// requests on natural entry. `Send + Sync + 'static` so the container stays
/// process-live.
pub struct CallbackDebugEndpoints<Req, Res> {
    request: Arc<corelib::SharedSlot<Req>>,
    response: Arc<corelib::SharedSlot<Result<Res, DebugHandlerError>>>,
}

impl<Req, Res> Clone for CallbackDebugEndpoints<Req, Res> {
    fn clone(&self) -> Self {
        Self {
            request: Arc::clone(&self.request),
            response: Arc::clone(&self.response),
        }
    }
}

impl<Req, Res> CallbackDebugEndpoints<Req, Res>
where
    Req: Send + Sync + 'static,
    Res: Send + Sync + 'static,
{
    pub(crate) fn new(
        request: Arc<corelib::SharedSlot<Req>>,
        response: Arc<corelib::SharedSlot<Result<Res, DebugHandlerError>>>,
    ) -> Self {
        Self { request, response }
    }

    /// Callback side: handle at most one pending request per hook entry.
    /// Returns `true` when a request was handled. Never blocks; the handler
    /// runs on the hook's own thread with bounded work only.
    pub fn handle_pending(
        &self,
        _ctx: &CallbackCtx,
        handler: impl FnOnce(Req) -> Result<Res, DebugHandlerError>,
    ) -> bool {
        let Some(request) = self.request.take() else {
            return false;
        };
        // Sole ownership by construction: the relay moved the only Arc into
        // the slot. A stray extra reference is re-queued and retried on the
        // next entry instead of being dropped.
        let request = match Arc::try_unwrap(request) {
            Ok(value) => value,
            // Stray extra reference: re-queue and retry on the next entry.
            Err(arc) => {
                let _ = self.request.try_send(arc);
                return false;
            }
        };
        let outcome = handler(request);
        // The response slot is single-flight by construction (the relay
        // delivers one request at a time); a contended lock drops the
        // response, which the wire side eventually reports via pending
        // accounting — the callback itself must never block.
        let _ = self.response.try_send(Arc::new(outcome));
        true
    }
}

/// Owner-side relay system (auto-registered by `register_callback_debug`):
/// channel inbox → request slot (one per frame, no overwrite) and response
/// slot → channel outbox with FIFO id pairing.
pub(crate) struct CallbackRelaySystem<T: CallbackDebugTopic> {
    channel: Arc<DebugTopicChannel>,
    request_slot: Arc<corelib::SharedSlot<T::Request>>,
    response_slot: Arc<corelib::SharedSlot<Result<T::Response, DebugHandlerError>>>,
    /// Delivered-but-unanswered requests, FIFO: (id, connection generation).
    in_flight: std::collections::VecDeque<(serde_json::Value, u64)>,
}

impl<T: CallbackDebugTopic> crate::plugin_api::host::UpdateSystemRunner for CallbackRelaySystem<T> {
    fn run(&mut self, _world: &mut World, _main: &crate::MainThreadToken) -> SystemResult {
        // 1. Deliver one queued request when the slot is free (no overwrite
        //    of an unconsumed request — docs: 新 request 不覆盖旧 request).
        if !self.request_slot.is_set() {
            let mut inbox = self
                .channel
                .inbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(queued) = inbox.front() {
                match queued.payload.clone().downcast::<T::Request>() {
                    Ok(typed) => {
                        if self.request_slot.try_send(typed).is_ok() {
                            let queued = inbox.pop_front().expect("front checked above");
                            self.in_flight.push_back((queued.id, queued.generation));
                        }
                    }
                    Err(_) => {
                        let queued = inbox.pop_front().expect("front checked above");
                        let mut outbox = self
                            .channel
                            .outbox
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        outbox.push(DebugResponse {
                            id: queued.id,
                            generation: queued.generation,
                            result: Err(DebugWireError {
                                code: DebugWireErrorCode::ServerError(
                                    DebugServerError::InternalError,
                                ),
                                message: "queued request type mismatch".to_owned(),
                            }),
                        });
                        self.channel.leave_pending();
                    }
                }
            }
        }

        // 2. Collect responses; FIFO pairing with the delivered ids.
        while let Some(response) = self.response_slot.take() {
            let Some((id, generation)) = self.in_flight.pop_front() else {
                // A response without a delivered request cannot be
                // correlated: drop it (no id to report).
                break;
            };
            // Sole ownership by construction (single-flight relay); a stray
            // extra reference goes back into the slot for the next frame.
            let outcome = match Arc::try_unwrap(response) {
                Ok(value) => value,
                Err(arc) => {
                    let _ = self.response_slot.try_send(arc);
                    break;
                }
            };
            let result = match outcome {
                Ok(value) => serde_json::to_value(&value).map_err(|e| DebugWireError {
                    code: DebugWireErrorCode::ServerError(DebugServerError::InternalError),
                    message: e.to_string(),
                }),
                Err(DebugHandlerError(message)) => Err(DebugWireError {
                    code: DebugWireErrorCode::ServerError(DebugServerError::HandlerError),
                    message,
                }),
            };
            let mut outbox = self
                .channel
                .outbox
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            outbox.push(DebugResponse {
                id,
                generation,
                result,
            });
            self.channel.leave_pending();
        }
        Ok(())
    }
}

impl AppCtx<'_> {
    /// Register a callback-domain debug topic.
    ///
    /// Auto-wires: the registry entry (same channel plumbing as the main
    /// domain) and this topic's relay system as an ordinary Update system of
    /// this plugin. Returns the callback-side endpoints — put them into this
    /// plugin's `CallbackSiteContainer` and call
    /// [`CallbackDebugEndpoints::handle_pending`] from the hook handler. The
    /// callback handler runs when the corresponding hook naturally enters;
    /// until then the request stays pending (bounded, no overwrite).
    pub fn register_callback_debug<T>(
        &mut self,
    ) -> Result<CallbackDebugEndpoints<T::Request, T::Response>, PluginError>
    where
        T: CallbackDebugTopic,
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
            callback_domain: true,
        })?;

        let request = Arc::new(corelib::SharedSlot::new());
        let response = Arc::new(corelib::SharedSlot::new());
        let relay = CallbackRelaySystem::<T> {
            channel,
            request_slot: Arc::clone(&request),
            response_slot: Arc::clone(&response),
            in_flight: std::collections::VecDeque::new(),
        };
        self.host
            .add_update_system_dyn(BoxedUpdateSystem::new(Box::new(relay)));
        Ok(CallbackDebugEndpoints::new(request, response))
    }
}
