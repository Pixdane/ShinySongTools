//! Unity main-thread scheduler: TLS five states, `SchedulerFrame`, the fixed
//! callback order, and the global-failure publication order.
//!
//! Invariants (docs/runtime-architecture.md, docs/runtime-crate.md):
//!
//! * `MainThreadToken` is constructed per frame only after the thread
//!   identity predicate passes, and only its short borrow enters systems;
//! * global failure publishes `RuntimeGate.close()` (Release) BEFORE
//!   `failed.store(true, Release)`;
//! * `original` is called exactly once per frame via [`OriginalGuard`];
//! * the frame's Drop is allocation-free and panic-free: an uncommitted
//!   frame closes the gate and leaks the App into a retention root so a
//!   still-reachable callback can only passthrough.
//!
//! The thread-identity predicate is injectable so no-game fixtures can drive
//! the full state machine off the process main thread; the production
//! predicate is fixed to `pthread_main_np() != 0`.

use crate::app::App;
use crate::handoff::{Handoff, HandoffTake};
use crate::observability::{self, compact_codes};
use scsp_core::{MainThreadToken, MethodPointerSlot, OriginalPhase, RuntimeGate, SlotMemory};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// Monotonic per-process frame sequence for compact events.
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

/// Hot-path-safe emission: the compact queue works on any thread, with or
/// without a scoped dispatch, never blocks and never allocates.
fn emit_compact(code: u16, level: scsp_core::CompactLevel, arg0: u64, arg1: u64) {
    let _ = scsp_core::process_event_queue().try_emit(
        scsp_core::CompactEvent::new(scsp_core::CompactEventCode(code), level).args(arg0, arg1),
    );
}

/// Opaque IL2CPP object / method types for the scheduler ABI.
#[repr(C)]
pub struct Il2CppObjectOpaque {
    _private: [u8; 0],
}

#[repr(C)]
pub struct MethodInfoOpaque {
    _private: [u8; 0],
}

/// Exact validated scheduler ABI: `UniRx.MainThreadDispatcher.LateUpdate()`.
pub type LateUpdateFn =
    unsafe extern "C" fn(this: *mut Il2CppObjectOpaque, method: *const MethodInfoOpaque);

/// Thread identity predicate (v1 production: `pthread_main_np() != 0`).
pub type ThreadIdentityCheck = Arc<dyn Fn() -> bool + Send + Sync>;

/// The scheduler hook: typed original captured before install, the
/// replacement, and the conservative installed flag over the owned slot.
pub struct SchedulerHook {
    pub(crate) slot: Arc<MethodPointerSlot>,
    pub(crate) original: LateUpdateFn,
    pub(crate) replacement: LateUpdateFn,
    pub(crate) installed: AtomicBool,
}

impl SchedulerHook {
    /// Bind a slot memory (whose captured original must be a live
    /// `LateUpdateFn`) and pair it with the process replacement body.
    ///
    /// # Panics
    ///
    /// Panics when the slot memory cannot be bound (null or unreadable):
    /// a reviewed backend contract violation that must surface at
    /// construction.
    #[must_use]
    pub fn new(slot_memory: Arc<dyn SlotMemory>, original: LateUpdateFn) -> Self {
        let slot = match MethodPointerSlot::bind(slot_memory) {
            Ok(slot) => Arc::new(slot),
            Err(err) => panic!("scheduler slot bind failed: {err}"),
        };
        Self {
            slot,
            original,
            replacement: scheduler_replacement,
            installed: AtomicBool::new(false),
        }
    }
}

/// Process-level scheduler context. Published before the hook becomes
/// reachable; a reachable callback without a published context is an
/// invariant break, not a startup branch.
static SCHEDULER: OnceLock<SchedulerContext> = OnceLock::new();

/// The published scheduler context, if this process's bootstrap reached
/// publication. One-shot: never replaced.
#[must_use]
pub fn scheduler_context() -> Option<&'static SchedulerContext> {
    SCHEDULER.get()
}

/// Publish the context (once per process; a second call is refused).
pub fn publish_context(context: SchedulerContext) -> bool {
    SCHEDULER.set(context).is_ok()
}

/// The replacement body installed into the slot: reads the static context
/// and runs one frame. Panics are bounded by `run_frame`'s inner catch.
unsafe extern "C" fn scheduler_replacement(
    this: *mut Il2CppObjectOpaque,
    method: *const MethodInfoOpaque,
) {
    if let Some(ctx) = SCHEDULER.get() {
        ctx.run_frame();
    } else {
        // Construct invariant break: callback reachable without a published
        // context. Nothing safe to call (no captured original here); the
        // observation goes through the compact queue because this is a hot
        // callback path without a scoped dispatch. Never panics across FFI.
        emit_compact(
            observability::compact_codes::SCHED_NO_PUBLISHED_CONTEXT,
            scsp_core::CompactLevel::Error,
            0,
            0,
        );
        let _ = (this, method);
    }
}

/// Process-stable scheduler context (published into a `OnceLock` by the
/// bootstrap before the hook is CAS-installed).
pub struct SchedulerContext {
    pub handoff: Handoff,
    pub hook: SchedulerHook,
    pub runtime_gate: RuntimeGate,
    pub failed: AtomicBool,
    pub main_thread_check: ThreadIdentityCheck,
}

impl SchedulerContext {
    /// The replacement body: one full frame of the fixed callback order.
    pub fn run_frame(&self) {
        // Scoped dispatch for this execution root (plugin systems inside use
        // normal tracing macros; the hot emits below go through the compact
        // queue regardless).
        let _obs = crate::observability::scope();
        let seq = FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
        emit_compact(
            compact_codes::SCHED_FRAME_ENTERED,
            scsp_core::CompactLevel::Info,
            seq,
            0,
        );
        // The frame is built BEFORE the execution catch so the recovery path
        // below can inspect the phase and compensate the original exactly
        // once; the frame's Drop remains the bottom-line ownership guard.
        let mut frame = SchedulerFrame {
            context: self,
            app: None,
            phase: OriginalPhase::BeforeOriginal,
            tls_committed: false,
        };

        let outcome = catch_unwind(AssertUnwindSafe(|| frame.execute()));
        let done_phase = frame.phase as u64;
        if outcome.is_ok() {
            emit_compact(
                compact_codes::SCHED_FRAME_DONE,
                scsp_core::CompactLevel::Info,
                seq,
                done_phase,
            );
            return;
        }
        // Panic inside the frame body: global failure, then recovery only
        // through reviewed non-panic operations; no plugin restore runs here.
        self.publish_global_failure();
        // `BeforeOriginal` panic: compensate the original exactly once. A
        // second panic inside this recovery is swallowed by this catch and
        // the frame's Drop keeps the App alive — never a second original
        // entry.
        if frame.phase == OriginalPhase::BeforeOriginal {
            let _ = catch_unwind(AssertUnwindSafe(|| frame.call_original_once()));
        }
        drop(catch_unwind(AssertUnwindSafe(|| drop(frame))));
        emit_compact(
            compact_codes::SCHED_FRAME_DONE,
            scsp_core::CompactLevel::Error,
            seq,
            done_phase,
        );
    }

    /// Global failure publication order (fixed): gate close (Release) first,
    /// then `failed` (Release). Callbacks read `failed` with Acquire and can
    /// therefore never observe `failed == true` while the gate still reads
    /// open. Reported through the compact queue: this runs on hot callback
    /// paths that have no scoped dispatch.
    pub fn publish_global_failure(&self) {
        self.runtime_gate.close();
        self.failed.store(true, Ordering::Release);
        emit_compact(
            observability::compact_codes::SCHED_GLOBAL_FAILURE,
            scsp_core::CompactLevel::Error,
            0,
            0,
        );
    }
}

/// TLS five states (docs/runtime-crate.md 主线程 TLS).
enum AppSlot {
    AwaitingHandoff,
    Running(Box<App>),
    Busy,
    Exited(Box<App>),
    Unavailable,
}

/// Introspection for fixtures and the debug plane: (state name, whether a
/// retained App completed startup — `None` when no App is retained).
#[must_use]
pub fn tls_snapshot() -> (&'static str, Option<bool>) {
    APP_SLOT.with(|slot| match &*slot.borrow() {
        AppSlot::AwaitingHandoff => ("awaiting_handoff", None),
        AppSlot::Running(app) => ("running", Some(app.startup_completed())),
        AppSlot::Busy => ("busy", None),
        AppSlot::Exited(app) => ("exited", Some(app.startup_completed())),
        AppSlot::Unavailable => ("unavailable", None),
    })
}

thread_local! {
    static APP_SLOT: core::cell::RefCell<AppSlot> =
        const { core::cell::RefCell::new(AppSlot::AwaitingHandoff) };
}

/// One callback frame: execution and App-ownership guard. Exists only on the
/// callback stack; never stored in App, TLS, or any lifecycle enum.
struct SchedulerFrame<'a> {
    context: &'a SchedulerContext,
    app: Option<Box<App>>,
    phase: OriginalPhase,
    tls_committed: bool,
}

impl SchedulerFrame<'_> {
    fn execute(&mut self) {
        // 1. Global failure observed before anything else: no driver, no
        //    token, no debug dispatch. Only the App disposition + original.
        if self.context.failed.load(Ordering::Acquire) {
            self.claim_app();
            if let Some(app) = self.app.take() {
                commit_tls(app, true);
            }
            self.call_original_once();
            self.commit_phase_after_failed_frame();
            return;
        }

        // 2. Thread identity. A mismatch is a scheduler core fault: publish
        //    global failure, mark TLS Unavailable (enter or keep), then
        //    passthrough only — no token, no plugin rollback.
        if !(self.context.main_thread_check)() {
            emit_compact(
                compact_codes::SCHED_THREAD_MISMATCH,
                scsp_core::CompactLevel::Error,
                0,
                0,
            );
            self.context.publish_global_failure();
            APP_SLOT.with(|slot| {
                let mut slot = slot.borrow_mut();
                if matches!(*slot, AppSlot::AwaitingHandoff) {
                    *slot = AppSlot::Unavailable;
                }
            });
            self.call_original_once();
            self.commit_phase_after_failed_frame();
            return;
        }

        // 3. Claim the App according to TLS and set Busy (very short RefCell
        //    borrow, released immediately).
        self.claim_app();
        let Some(mut app) = self.app.take() else {
            // Nested callback (Busy), Pending handoff, Exited, or
            // Unavailable: passthrough only.
            self.call_original_once();
            self.commit_phase_after_failed_frame();
            return;
        };

        // 4. Run the driver: first frame Startup only, later frames the
        //    fixed Update driver. The RuntimeGate is opened by the startup
        //    driver's caller — inside the App, after all owners complete.
        let token: MainThreadToken = unsafe {
            // SAFETY: reviewed scheduler boundary — the identity predicate
            // passed in step 2 for this frame.
            MainThreadToken::assume_main_thread()
        };
        let mut startup_ok = true;
        if !app.startup_completed() {
            let report = app.run_startup(&token);
            startup_ok = report.retired.is_empty();
            // First Startup driver fully completed and the App is still
            // runnable: the runtime opens the RuntimeGate LAST.
            if startup_ok {
                self.context.runtime_gate.open();
            }
        } else {
            let _ = app.run_update(&token);
        }

        // 5. Call original while TLS stays Busy (App still on this stack).
        self.call_original_once();

        // 6. Commit the App back: Exited on global failure or scheduler-level
        //    startup fault, otherwise Running.
        let exit = self.context.failed.load(Ordering::Acquire) || !startup_ok;
        commit_tls(app, exit);
        self.tls_committed = true;
    }

    /// Claim the App per the TLS state and mark Busy (docs step 2). On
    /// Pending the TLS stays AwaitingHandoff; on Failed it becomes
    /// Unavailable with the global-failure publication.
    fn claim_app(&mut self) {
        APP_SLOT.with(|slot| {
            let mut slot = slot.borrow_mut();
            match &mut *slot {
                AppSlot::AwaitingHandoff => match self.context.handoff.try_take() {
                    HandoffTake::Ready(app) => {
                        self.app = Some(app);
                        *slot = AppSlot::Busy;
                    }
                    HandoffTake::Pending => {}
                    HandoffTake::Failed => {
                        *slot = AppSlot::Unavailable;
                        self.context.publish_global_failure();
                    }
                },
                AppSlot::Running(_) => {
                    let old = core::mem::replace(&mut *slot, AppSlot::Busy);
                    if let AppSlot::Running(app) = old {
                        self.app = Some(app);
                    }
                }
                AppSlot::Busy | AppSlot::Exited(_) | AppSlot::Unavailable => {}
            }
        });
    }

    /// Exactly-once original via the shared [`OriginalGuard`] phase machine.
    fn call_original_once(&mut self) {
        let guard = scsp_core::OriginalGuard::new();
        if !guard.begin_call() {
            return;
        }
        self.phase = OriginalPhase::CallingOriginal;
        emit_compact(
            compact_codes::SCHED_ORIGINAL_CALLED,
            scsp_core::CompactLevel::Info,
            0,
            0,
        );
        // SAFETY: the original is the typed pointer captured from the slot
        // at bind time; the replacement passes `this`/`method` through. The
        // frame uses null stand-ins only where no game object is involved.
        unsafe {
            (self.context.hook.original)(core::ptr::null_mut(), core::ptr::null());
        }
        emit_compact(
            compact_codes::SCHED_ORIGINAL_RETURNED,
            scsp_core::CompactLevel::Info,
            0,
            0,
        );
        guard.end_call();
        self.phase = OriginalPhase::AfterOriginal;
    }

    /// After a frame that observed (or published) global failure: if the
    /// panic recovery path reaches this, the original must not be called a
    /// second time — the phase machine guarantees that.
    fn commit_phase_after_failed_frame(&mut self) {
        self.phase = OriginalPhase::AfterOriginal;
    }
}

impl Drop for SchedulerFrame<'_> {
    /// Bottom-line guard: allocation-free, panic-free, plugin-free. An
    /// uncommitted frame closes the gate and leaks the App into the TLS
    /// (Busy) so later callbacks only passthrough. Prefer losing reclamation
    /// over dropping an App during unwind.
    fn drop(&mut self) {
        if self.tls_committed {
            return;
        }
        if let Some(app) = self.app.take() {
            self.context.publish_global_failure();
            // Retention root: leak the App (never drop during unwind).
            std::mem::forget(app);
        }
    }
}

fn commit_tls(app: Box<App>, exit: bool) {
    APP_SLOT.with(|slot| {
        let mut slot = slot.borrow_mut();
        // The TLS must be Busy while the frame holds the App.
        if !matches!(*slot, AppSlot::Busy) {
            // Invariant break: global failure, keep the App alive in a
            // retention root and never run business logic again.
            std::mem::forget(app);
            *slot = AppSlot::Unavailable;
            return;
        }
        *slot = if exit {
            AppSlot::Exited(app)
        } else {
            AppSlot::Running(app)
        };
    });
}

/// Install the scheduler hook into the target slot (CAS + readback, then the
/// conservative installed flag). Called once by the bootstrap; a second call
/// is refused.
pub fn install_hook(hook: &SchedulerHook) -> Result<(), scsp_core::HookError> {
    if hook.installed.swap(true, Ordering::AcqRel) {
        return Err(scsp_core::HookError::SiteAlreadyRegistered);
    }
    let replacement_addr = hook.replacement as *const () as usize;
    hook.slot.install(replacement_addr)
}

/// Production thread predicate (v1 platform judgement).
pub fn pthread_main_check() -> ThreadIdentityCheck {
    Arc::new(|| unsafe { libc::pthread_main_np() != 0 })
}
