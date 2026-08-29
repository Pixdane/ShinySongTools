//! Debug control plane as an ordinary plugin crate.
//!
//! JSON-RPC 2.0 over a Unix domain socket (docs/debug-diagnostics-logging.md):
//! length-prefixed frames (4-byte big-endian + UTF-8 JSON), single client,
//! `0600` socket permissions, stale-socket unlink at build, transport
//! lifecycle to process exit. Requests decode and route through the typed
//! topic registry (main domain); built-in `runtime.*` introspection topics
//! read the runtime snapshot.
//!
//! Session hygiene: every request carries the connection generation it
//! arrived on and every response carries it back — a request or answer whose
//! connection is gone is dropped instead of being mixed into the next
//! session. The transport queues are bounded (overflow answers `queue_full`
//! on the request side, drops the newest response on the write side), so a
//! stalled or malicious client cannot grow runtime memory without bound.
//!
//! Dispatch flow: this plugin's Update system (registered like any plugin
//! system) routes wire requests into topic channels and drains responses
//! back to the wire; the owner's handler systems do the typed work. Gates:
//! every route requires the runtime gate AND the owner plugin gate; after a
//! global failure all new requests answer `runtime_unavailable`.

use corelib::debug::{
    DebugIntrospection, DebugResponse, DebugServerError, DebugTopicLookup, DebugWireErrorCode,
};
use corelib::{AppCtx, Plugin, PluginError};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod protocol;
pub use protocol::{
    MAX_FRAME_BYTES, MAX_INBOX_REQUESTS, MAX_OUTBOX_RESPONSES, WireRequest, WireResponse,
};

/// Shared transport state between the I/O worker and the dispatch system.
pub struct DebugTransport {
    inbox: Mutex<Vec<WireRequest>>,
    outbox: Mutex<Vec<WireResponse>>,

    /// The runtime gate reader: after a global failure, new requests answer
    /// `runtime_unavailable`.
    runtime_gate: corelib::GateReader,

    /// Active (delivered, unanswered) request ids of the CURRENT connection,
    /// serialized for comparison. Docs: 同一连接内 active `id` 必须唯一，
    /// 重复回 `invalid_request`；response 写回后 id 可复用。 Cleared at
    /// connection start; `id: null` is not trackable and never recorded.
    active_ids: Mutex<Vec<String>>,

    /// Monotonic connection counter: bumped once per accepted connection.
    generation: AtomicU64,
}

/// Serialized form of a trackable (non-null) request id; `None` for `null`.
fn trackable_id(id: &serde_json::Value) -> Option<String> {
    if id.is_null() {
        return None;
    }
    serde_json::to_string(id).ok()
}

impl DebugTransport {
    fn bind(socket_path: PathBuf, runtime_gate: corelib::GateReader) -> std::io::Result<Arc<Self>> {
        // Stale-socket cleanup before bind (docs: unlink 前置清理).
        let _ = std::fs::remove_file(&socket_path);
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&socket_path)?;
        // 0600: only the game's own user may talk to the debug plane.
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
        // Nonblocking accept: while one connection is being served, the
        // worker can still poll the queue and close further clients
        // immediately (docs: 已有连接时新连接直接关闭).
        listener.set_nonblocking(true)?;
        let transport = Arc::new(Self {
            inbox: Mutex::new(Vec::new()),
            outbox: Mutex::new(Vec::new()),
            runtime_gate,
            active_ids: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
        });
        let worker_transport = Arc::clone(&transport);
        std::thread::Builder::new()
            .name("scsp-debug-io".to_owned())
            .spawn(move || io_worker(listener, worker_transport))?;
        Ok(transport)
    }

    /// Generation of the connection being accepted (monotonic, starts at 1).
    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn current_generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn push_response(&self, body: serde_json::Value, generation: u64) {
        let mut outbox = self.outbox.lock().expect("outbox lock");
        if outbox.len() >= MAX_OUTBOX_RESPONSES {
            // The client stopped reading; drop the newest answer instead of
            // growing without bound. The affected request hangs on the
            // client side — a stalled client must not grow runtime memory.
            drop(outbox);
            tracing::warn!(target: "debug", "debug outbox capacity reached; dropping a response");
            return;
        }
        outbox.push(WireResponse { body, generation });
    }

    fn respond_ok(&self, id: serde_json::Value, result: serde_json::Value, generation: u64) {
        let mut body = serde_json::Map::new();
        body.insert("jsonrpc".into(), serde_json::Value::String("2.0".into()));
        body.insert("id".into(), id);
        body.insert("result".into(), result);
        self.push_response(serde_json::Value::Object(body), generation);
    }

    fn respond_error(
        &self,
        id: serde_json::Value,
        code: i64,
        data_code: Option<&str>,
        message: &str,
        generation: u64,
    ) {
        let mut body = serde_json::Map::new();
        body.insert("jsonrpc".into(), serde_json::Value::String("2.0".into()));
        body.insert("id".into(), id);
        let mut error = serde_json::Map::new();
        error.insert("code".into(), serde_json::Value::from(code));
        if let Some(data_code) = data_code {
            error.insert(
                "data".into(),
                serde_json::json!({ "code": data_code, "message": message }),
            );
        } else {
            error.insert("message".into(), serde_json::Value::String(message.into()));
        }
        body.insert("error".into(), serde_json::Value::Object(error));
        self.push_response(serde_json::Value::Object(body), generation);
    }
}

/// I/O worker: single connection, framed reads, response writes.
fn io_worker(listener: UnixListener, transport: Arc<DebugTransport>) {
    loop {
        match listener.accept() {
            Ok((stream, _addr)) => {
                // Peer hangup or IO error: close and take the next one.
                let _ = handle_connection(&stream, &listener, &transport);
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                // Nothing waiting; keep the loop cheap.
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => {
                // Transient accept failure: back off and retry; the transport
                // lives to process exit.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}

fn handle_connection(
    stream: &UnixStream,
    listener: &UnixListener,
    transport: &DebugTransport,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_millis(20)))?;
    // Session scope: everything this connection produces carries its
    // generation; leftovers of earlier connections are dropped below and
    // late answers of THIS connection are dropped once it is replaced.
    let generation = transport.next_generation();
    transport
        .outbox
        .lock()
        .expect("outbox lock")
        .retain(|response| response.generation == generation);
    // Fresh connection scope: id uniqueness applies per connection; a
    // still-pending request of a previous connection does not block reuse.
    transport
        .active_ids
        .lock()
        .expect("active ids lock")
        .clear();
    let mut length_buffer = [0u8; 4];
    let mut length_read = 0usize;
    let mut body: Vec<u8> = Vec::new();
    loop {
        // A connection arriving while this one is served is closed
        // immediately (docs: 已有连接时新连接直接关闭，不维护连接集合).
        // The nonblocking listener makes that real during the read/write
        // waits of the served connection.
        if let Ok((incoming, _)) = listener.accept() {
            drop(incoming);
        }
        // Responses first (bounded drain); answers of dead connections are
        // dropped instead of being mixed into this session.
        let responses: Vec<WireResponse> = {
            let mut outbox = transport.outbox.lock().expect("outbox lock");
            outbox.drain(..).collect()
        };
        for response in responses {
            if response.generation != generation {
                continue;
            }
            // Response written → the id becomes reusable on this connection.
            if let Some(key) = response.body.get("id").and_then(trackable_id) {
                transport
                    .active_ids
                    .lock()
                    .expect("active ids lock")
                    .retain(|active| *active != key);
            }
            let frame = encode_frame(&response.body)?;
            (&*stream).write_all(&frame)?;
        }
        (&*stream).flush()?;

        // Read whatever arrived (timeout keeps the outbox flowing).
        match (&*stream).read(&mut length_buffer[length_read..]) {
            Ok(0) => break, // peer closed
            Ok(n) => {
                length_read += n;
                if length_read == 4 {
                    let length = u32::from_be_bytes(length_buffer);
                    length_read = 0;
                    if length > MAX_FRAME_BYTES {
                        transport.respond_error(
                            serde_json::Value::Null,
                            -32000,
                            Some(DebugServerError::PayloadTooLarge.code_name()),
                            "frame exceeds the payload limit",
                            generation,
                        );
                        // Discard the oversized payload to stay framed.
                        discard(stream, u64::from(length))?;
                        continue;
                    }
                    body.resize(length as usize, 0);
                    read_exact_resilient(stream, &mut body)?;
                    transport.handle_frame(&body, generation);
                    body.clear();
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn discard(stream: &UnixStream, mut remaining: u64) -> std::io::Result<()> {
    let mut sink = [0u8; 4096];
    while remaining > 0 {
        let chunk = remaining.min(sink.len() as u64) as usize;
        let n = (&*stream).read(&mut sink[..chunk])?;
        if n == 0 {
            break;
        }
        remaining -= n as u64;
    }
    Ok(())
}

fn read_exact_resilient(stream: &UnixStream, buffer: &mut [u8]) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buffer.len() {
        let n = (&*stream).read(&mut buffer[done..])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "peer closed",
            ));
        }
        done += n;
    }
    Ok(())
}

/// 4-byte big-endian length prefix + UTF-8 JSON body.
fn encode_frame(body: &serde_json::Value) -> std::io::Result<Vec<u8>> {
    let json = serde_json::to_vec(body)?;
    let length = u32::try_from(json.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "frame too large"))?;
    let mut frame = Vec::with_capacity(4 + json.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&json);
    Ok(frame)
}

impl DebugTransport {
    fn handle_frame(&self, bytes: &[u8], generation: u64) {
        let parsed: Result<serde_json::Value, _> = serde_json::from_slice(bytes);
        let value = match parsed {
            Ok(value) => value,
            Err(_) => {
                self.respond_error(
                    serde_json::Value::Null,
                    -32700,
                    None,
                    "parse error",
                    generation,
                );
                return;
            }
        };
        // Batch and envelope validation → -32600.
        if value.is_array() {
            self.respond_error(
                serde_json::Value::Null,
                -32600,
                None,
                "batch not supported",
                generation,
            );
            return;
        }
        let Some(object) = value.as_object() else {
            self.respond_error(
                serde_json::Value::Null,
                -32600,
                None,
                "invalid request",
                generation,
            );
            return;
        };
        let version_ok = object
            .get("jsonrpc")
            .and_then(|v| v.as_str())
            .is_some_and(|v| v == "2.0");
        let method = object
            .get("method")
            .and_then(|m| m.as_str())
            .map(str::to_owned);
        let id = object.get("id").cloned();
        let params = object
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if !version_ok {
            self.respond_error(
                id.unwrap_or(serde_json::Value::Null),
                -32600,
                None,
                "invalid request",
                generation,
            );
            return;
        }
        let Some(method) = method else {
            self.respond_error(
                serde_json::Value::Null,
                -32600,
                None,
                "invalid request",
                generation,
            );
            return;
        };
        let Some(id) = id else {
            // Notifications are not supported (method without id).
            self.respond_error(
                serde_json::Value::Null,
                -32600,
                None,
                "notifications not supported",
                generation,
            );
            return;
        };
        // Same-connection active ids must be unique (docs: 重复回
        // invalid_request，response 写回后 id 可复用).
        if let Some(key) = trackable_id(&id) {
            let mut active = self.active_ids.lock().expect("active ids lock");
            if active.contains(&key) {
                drop(active);
                self.respond_error(id, -32600, None, "duplicate active id", generation);
                return;
            }
            active.push(key);
        }
        // Bounded inbox: a client flooding frames faster than the main
        // thread dispatches gets `queue_full` instead of unbounded growth.
        if self.inbox.lock().expect("inbox lock").len() >= MAX_INBOX_REQUESTS {
            self.respond_error(
                id,
                -32000,
                Some(DebugServerError::QueueFull.code_name()),
                "transport inbox capacity reached",
                generation,
            );
            return;
        }
        self.inbox.lock().expect("inbox lock").push(WireRequest {
            id,
            method,
            params,
            generation,
        });
    }
}

/// The DebugPlugin: registers the transport (worker phase), the dispatch
/// Update system, and serves the built-in `runtime.*` introspection topics.
pub struct DebugPlugin;

impl Plugin for DebugPlugin {
    fn name(&self) -> &'static str {
        "debug"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        let socket_path = ctx.data_root().join("shiny-song-tools").join("d.sock");
        let runtime_gate = ctx.runtime_gate_reader();
        let transport = DebugTransport::bind(socket_path, runtime_gate).map_err(PluginError::Io)?;
        let topics = ctx.debug_topic_registry();
        let introspection = ctx.runtime_introspection();

        // The dispatch system is registered through the ordinary public
        // phase API as a closure over its state — no boxed-runner seam.
        let mut dispatch = DebugDispatchSystem {
            transport,
            topics,
            introspection,
        };
        ctx.add_update_system(
            move |_ctx: corelib::UpdateCtx<'_>| -> corelib::SystemResult {
                dispatch.run();
                Ok(())
            },
        );
        tracing::info!(target: "debug", "DebugPlugin registered with UDS transport");
        Ok(())
    }
}

/// The DebugPlugin's Update system: routes wire requests into topic channels
/// and drains responses back to the wire.
struct DebugDispatchSystem {
    transport: Arc<DebugTransport>,
    topics: Arc<dyn DebugTopicLookup>,
    introspection: Option<Arc<dyn DebugIntrospection>>,
}

impl DebugDispatchSystem {
    fn run(&mut self) {
        let current = self.transport.current_generation();
        let requests: Vec<WireRequest> = {
            let mut inbox = self.transport.inbox.lock().expect("inbox lock");
            // Requests of replaced connections are dropped here: they were
            // never enqueued into topics, so no pending accounting is owed.
            inbox
                .drain(..)
                .filter(|request| request.generation == current)
                .collect()
        };
        let topic_views = self.topics.topics();
        for request in requests {
            // Built-in introspection topics first (main domain, world-free).
            if let Some(payload) = self
                .introspection
                .as_ref()
                .and_then(|i| i.introspect(&request.method))
            {
                self.transport
                    .respond_ok(request.id, payload, request.generation);
                continue;
            }
            // Runtime gate: after a global failure, no new dispatch at all.
            if !self.transport.runtime_gate.is_open() {
                self.transport.respond_error(
                    request.id,
                    -32000,
                    Some(DebugServerError::RuntimeUnavailable.code_name()),
                    "runtime unavailable",
                    request.generation,
                );
                continue;
            }
            let Some(topic) = topic_views.iter().find(|view| view.name == request.method) else {
                self.transport.respond_error(
                    request.id,
                    -32601,
                    None,
                    "method not found",
                    request.generation,
                );
                continue;
            };
            if !topic.channel.dispatchable() {
                self.transport.respond_error(
                    request.id,
                    -32000,
                    Some(DebugServerError::PluginUnavailable.code_name()),
                    "plugin unavailable",
                    request.generation,
                );
                continue;
            }
            // Typed decode at the dispatch boundary.
            let decoded = match (topic.decode)(&request.params) {
                Ok(payload) => payload,
                Err(message) => {
                    self.transport.respond_error(
                        request.id,
                        -32602,
                        None,
                        &format!("invalid params: {message}"),
                        request.generation,
                    );
                    continue;
                }
            };
            if topic
                .channel
                .enqueue(request.id.clone(), decoded, request.generation)
                .is_err()
            {
                self.transport.respond_error(
                    request.id,
                    -32000,
                    Some(DebugServerError::QueueFull.code_name()),
                    "topic pending capacity reached",
                    request.generation,
                );
            }
        }

        // Drain topic outboxes → wire. Answers of replaced connections are
        // dropped here: their session is gone.
        for topic in topic_views {
            let responses: Vec<DebugResponse> = {
                let mut outbox = topic.channel.outbox.lock().expect("outbox lock");
                outbox.drain(..).collect()
            };
            for response in responses {
                if response.generation != current {
                    continue;
                }
                match response.result {
                    Ok(value) => self
                        .transport
                        .respond_ok(response.id, value, response.generation),
                    Err(err) => match err.code {
                        DebugWireErrorCode::ServerError(server) => self.transport.respond_error(
                            response.id,
                            -32000,
                            Some(server.code_name()),
                            &err.message,
                            response.generation,
                        ),
                        other => self.transport.respond_error(
                            response.id,
                            map_wire_code(other),
                            None,
                            &err.message,
                            response.generation,
                        ),
                    },
                }
            }
        }
    }
}

fn map_wire_code(code: DebugWireErrorCode) -> i64 {
    match code {
        DebugWireErrorCode::MethodNotFound => -32601,
        DebugWireErrorCode::InvalidParams => -32602,
        DebugWireErrorCode::ServerError(_) => -32000,
    }
}
