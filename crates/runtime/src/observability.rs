//! Process-level observability root (debug crate Rustdoc).
//!
//! * scoped `tracing::Dispatch` established in every runtime-owned execution
//!   root (`scsp_start` body, bootstrap worker, outer scheduler frame, plugin
//!   system calls, drain worker, debug worker) — never a global subscriber;
//! * Apple Unified Logging output via `tracing-os-layer` (subsystem
//!   `com.shinysongtools.runtime`), falling back to a stderr `fmt` layer when
//!   the OS layer cannot initialize;
//! * a dedicated drain worker converting callback/scheduler hot-path
//!   [`CompactEvent`]s from the process-level queue back into normal tracing
//!   events. Hot paths only ever submit compact events; they never call the
//!   tracing facade, never allocate strings, and never block.

use corelib::{CallbackObservability, CompactEvent, CompactEventCode, CompactLevel};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tracing::{Dispatch, Level};
use tracing_os_layer::OsLogLayer;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;

/// Apple Unified Logging subsystem (fixed by design; v1 has no file sink and
/// no dynamic reload).
pub const SUBSYSTEM: &str = "com.shinysongtools.runtime";

/// v1 uses one os_log category for all events: `OsLogLayer` binds its
/// category at construction, and per-event routing to `plugin`/`hook`/
/// `debug` categories would need per-layer target filtering. Domains remain
/// distinguishable through the tracing `target` field (see [`code_category`]).
/// Per-category handles are a documented polishing item.
pub const CATEGORY: &str = "runtime";

/// Stable compact event codes (v1: core/runtime predefined only).
pub mod compact_codes {
    pub const SCHED_FRAME_ENTERED: u16 = 1;
    pub const SCHED_ORIGINAL_CALLED: u16 = 2;
    pub const SCHED_ORIGINAL_RETURNED: u16 = 3;
    pub const SCHED_NO_PUBLISHED_CONTEXT: u16 = 4;
    pub const SCHED_GLOBAL_FAILURE: u16 = 5;
    pub const SCHED_THREAD_MISMATCH: u16 = 6;
    pub const FFI_ENTRY_PANICKED: u16 = 7;
    pub const SCHED_FRAME_DONE: u16 = 8;
}

/// (domain, stable message) for a compact code. `domain` travels as a field (the
/// Unknown codes degrade to a generic record — emitting must never fail.
#[must_use]
pub fn code_category(code: CompactEventCode) -> (&'static str, &'static str) {
    match code.0 {
        compact_codes::SCHED_FRAME_ENTERED => ("scheduler", "scheduler frame entered"),
        compact_codes::SCHED_ORIGINAL_CALLED => ("hook", "original LateUpdate entered"),
        compact_codes::SCHED_ORIGINAL_RETURNED => ("hook", "original LateUpdate returned"),
        compact_codes::SCHED_FRAME_DONE => ("scheduler", "scheduler frame done"),
        compact_codes::SCHED_NO_PUBLISHED_CONTEXT => {
            ("scheduler", "callback reached without published context")
        }
        compact_codes::SCHED_GLOBAL_FAILURE => ("scheduler", "scheduler global failure published"),
        compact_codes::SCHED_THREAD_MISMATCH => ("scheduler", "scheduler thread identity mismatch"),
        compact_codes::FFI_ENTRY_PANICKED => ("runtime", "entry panicked; startup aborted"),
        _ => ("runtime", "unclassified compact event"),
    }
}

/// Re-emit one drained compact event as a normal tracing event. The tracing
/// target is a fixed literal (the event macro embeds it into a static
/// callsite, so it cannot be a runtime value); the domain and stable message
/// travel as fields. Never allocates beyond the fixed fields; never panics.
fn emit_compact_event(event: &CompactEvent) {
    let code = event.code.0;
    let owner = event.owner.0;
    let site = event.site.0;
    let (arg0, arg1) = (event.arg0, event.arg1);
    let (domain, message) = code_category(event.code);
    match event.level {
        CompactLevel::Info => {
            tracing::event!(target: "scsp.compact", Level::INFO, code = code, owner = owner, site = site, arg0 = arg0, arg1 = arg1, domain = %domain, message = %message)
        }
        CompactLevel::Warn => {
            tracing::event!(target: "scsp.compact", Level::WARN, code = code, owner = owner, site = site, arg0 = arg0, arg1 = arg1, domain = %domain, message = %message)
        }
        CompactLevel::Error => {
            tracing::event!(target: "scsp.compact", Level::ERROR, code = code, owner = owner, site = site, arg0 = arg0, arg1 = arg1, domain = %domain, message = %message)
        }
    }
}

/// Drain everything currently queued (bounded per call so a producer that
/// keeps outrunning the worker cannot turn this into an unbounded loop).
#[must_use]
pub fn drain_available(queue: &CallbackObservability) -> usize {
    let mut drained = 0;
    for event in queue.drain(256) {
        emit_compact_event(&event);
        drained += 1;
    }
    drained
}

/// Build the scoped dispatch: Apple Unified Logging layer when the OS log
/// client initializes, stderr `fmt` layer otherwise. Never fails.
fn build_dispatch() -> Dispatch {
    if let Some(os_log) = OsLogLayer::try_new(SUBSYSTEM, CATEGORY) {
        Dispatch::new(Registry::default().with(os_log))
    } else {
        Dispatch::new(
            tracing_subscriber::fmt()
                .with_target(true)
                .with_writer(std::io::stderr)
                .finish(),
        )
    }
}

/// Spawn the process-lifetime drain worker. The thread owns the dispatch and
/// reads the process compact queue; it never touches the gate, App, or any
/// lifecycle object, so it keeps recording across global failure and exit.
fn spawn_drain_worker(dispatch: Dispatch) {
    let _ = std::thread::Builder::new()
        .name("scsp-observability-drain".to_owned())
        .spawn(move || {
            let _scope = tracing::dispatcher::set_default(&dispatch);
            let queue = corelib::process_event_queue();
            loop {
                if drain_available(&queue) == 0 {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        });
}

/// Process-lifetime observability root.
pub struct ObservabilityRoot {
    created_at: Instant,
    dispatch: Dispatch,
}

impl ObservabilityRoot {
    fn new() -> Self {
        let dispatch = build_dispatch();
        spawn_drain_worker(dispatch.clone());
        Self {
            created_at: Instant::now(),
            dispatch,
        }
    }

    #[must_use]
    pub fn uptime(&self) -> Duration {
        self.created_at.elapsed()
    }
}

static ROOT: OnceLock<ObservabilityRoot> = OnceLock::new();

/// The process-wide root, created on first call and reused forever. Building
/// it also starts the drain worker; duplicate entries only reuse the root to
/// record events.
pub fn root() -> &'static ObservabilityRoot {
    ROOT.get_or_init(ObservabilityRoot::new)
}

/// Establish the scoped dispatch for one runtime-owned execution root. The
/// returned guard must stay alive for the root's whole execution; callers
/// that can run outside any root (FFI recovery, hook callbacks) emit compact
/// events via `corelib::process_event_queue()` instead of tracing.
#[must_use]
pub fn scope() -> tracing::dispatcher::DefaultGuard {
    tracing::dispatcher::set_default(&root().dispatch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use corelib::{CompactOwnerId, CompactSiteId};
    use std::sync::{Arc, Mutex};

    #[test]
    fn drained_events_reach_the_installed_dispatch_with_stable_fields() {
        let captured: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_target(true)
            .with_ansi(false)
            .with_writer(move || LogWriter(Arc::clone(&writer)))
            .finish();

        let queue = CallbackObservability::with_capacity(8);
        let event = CompactEvent::new(
            CompactEventCode(compact_codes::SCHED_GLOBAL_FAILURE),
            CompactLevel::Error,
        )
        .owner(CompactOwnerId(3))
        .site(CompactSiteId(1))
        .args(9, 0);
        assert!(queue.try_emit(event));

        let dispatch = Dispatch::new(subscriber);
        let _scope = tracing::dispatcher::set_default(&dispatch);
        let drained = drain_available(&queue);
        drop(_scope);

        assert_eq!(drained, 1);
        let buffer = captured.lock().unwrap();
        let text = String::from_utf8_lossy(&buffer);
        assert!(
            text.contains("scheduler global failure published"),
            "{text}"
        );
        assert!(text.contains("code=5"), "{text}");
        assert!(text.contains("owner=3"), "{text}");
    }

    #[test]
    fn unknown_codes_degrade_without_failing() {
        let (target, message) = code_category(CompactEventCode(u16::MAX));
        assert_eq!(target, "runtime");
        assert_eq!(message, "unclassified compact event");
    }

    /// `tracing_subscriber::fmt::MakeWriter` into a shared buffer.
    struct LogWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
}
