//! Hook typestate: publish-before-install is a compile-time fact.
//!
//! A hook target is abstracted by [`HookTarget`]; the ABI and validation
//! predicates are bound to the type and owned by the plugin author (trusted
//! boundary: plugins and runtime compile in the same repository and are
//! reviewed by the same person). Each target has exactly one process-lifetime
//! static site created by [`define_hook_site!`]. The builder typestate
//! guarantees that a [`HookSite`] is fully published (typed original slot,
//! both gate readers, container `Arc`, replacement pointer) before
//! `install` — the only installation path — can run.
//!
//! The static site is a retention root: once published it occupies the
//! target until process exit, even if installation fails, the hook is
//! restored, or the plugin retires. One target, one site: no multi-instance,
//! re-install, slot chaining, or physical unload.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use scsp_core::{
    GateReader, HookError, MethodPointerSlot, MethodRef, OriginalGuard, OriginalPhase, TargetId,
};

use crate::PluginError;
use crate::context::AppCtx;

/// The declared identity and ABI of one hook target.
///
/// `Original` is the typed original function pointer (including the implicit
/// `MethodInfo` parameter where the target's calling convention requires
/// it). The raw→typed conversion lives in the author-owned unsafe boundary
/// below; the framework never transmutes on its own.
pub trait HookTarget: 'static {
    /// Assembly / namespace / class / method / parameter count identity.
    const TARGET: TargetId;
    /// Typed original function pointer type (carries the whole ABI).
    type Original: Copy;

    /// Validate the resolved method against this target. The default
    /// implementation rejects identity drift; authors may add stricter
    /// checks (return type, instance-ness) but never weaker ones.
    fn validate(method: &MethodRef) -> Result<(), HookError> {
        if method.matches_target(&Self::TARGET) {
            Ok(())
        } else {
            Err(HookError::SignatureMismatch)
        }
    }

    /// Address of the replacement function. Safe: casting a function pointer
    /// to its address cannot dereference anything.
    fn replacement_addr(original: Self::Original) -> usize;

    /// Convert a slot address into the typed original.
    ///
    /// # Safety
    ///
    /// `addr` must be a live code pointer whose calling convention exactly
    /// matches `Self::Original`. Called by the framework only with the
    /// original pointer captured at bind time by the backend.
    unsafe fn original_from_raw(addr: usize) -> Self::Original;
}

/// Capability token carried into hook callbacks. Endpoint operations on the
/// callback side require it; it is constructed by the site's dispatch path
/// only, never by plugin code.
pub struct CallbackCtx {
    _private: (),
}

impl CallbackCtx {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

impl core::fmt::Debug for CallbackCtx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("CallbackCtx")
    }
}

/// Gate readers stored in a published site.
#[derive(Clone)]
struct SiteGates {
    runtime: GateReader,
    plugin: GateReader,
}

struct SiteInner<T: HookTarget, C> {
    container: Arc<C>,
    gates: SiteGates,
    replacement: T::Original,
    /// Typed-original slot address; `0` until installation captured it.
    original_addr: AtomicUsize,
    /// The single conservative installed flag, shared with the install
    /// handle and the ledger restore action: set once installation is
    /// confirmed by readback, cleared once restore is confirmed.
    installed: Arc<AtomicBool>,
}

/// The process-lifetime static site of one hook target (retention root).
pub struct HookSite<T: HookTarget, C> {
    inner: std::sync::OnceLock<SiteInner<T, C>>,
}

// Safety: the site is shared across the callback domain and the main
// domain. `SiteInner` only holds `Arc<C>` (C: Send + Sync), gate readers
// (atomics), a copyable function pointer, and atomics.
unsafe impl<T: HookTarget + Sync, C: Send + Sync> Sync for HookSite<T, C> {}
unsafe impl<T: HookTarget + Send, C: Send + Sync> Send for HookSite<T, C> {}

impl<T: HookTarget, C> Default for HookSite<T, C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: HookTarget, C> HookSite<T, C> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            inner: std::sync::OnceLock::new(),
        }
    }

    /// `true` once the site has been published (permanently, to process
    /// exit).
    #[must_use]
    pub fn is_published(&self) -> bool {
        self.inner.get().is_some()
    }

    /// `true` while this site's hook is installed and both gates are open.
    #[must_use]
    pub fn is_dispatchable(&self) -> bool
    where
        T: Sync,
        C: Send + Sync,
    {
        match self.inner.get() {
            Some(inner) => {
                inner.installed.load(Ordering::Acquire)
                    && inner.gates.runtime.is_open()
                    && inner.gates.plugin.is_open()
            }
            None => false,
        }
    }
}

impl<T: HookTarget, C: Send + Sync> HookSite<T, C> {
    /// Dispatch entry for the author's replacement wrapper.
    ///
    /// Order of resolution:
    /// 1. Site not published / not installed → `fallback` (never call the
    ///    original: its address was never captured here).
    /// 2. Either gate closed → `passthrough` with the typed original,
    ///    exactly once; the handler is unreachable.
    /// 3. Gates open → run the handler with a [`Callback`]. If the handler
    ///    panics before invoking the original, the original is called once
    ///    via `passthrough`; if it panics during or after the original
    ///    call, `fallback` produces the return value (the original is never
    ///    retried). If the handler returns normally without calling the
    ///    original, the exactly-once contract forces one passthrough call.
    ///
    /// # Panic contract (author-owned `extern "C"` boundary)
    ///
    /// The handler's panic is contained here, but the recovery paths call
    /// `passthrough`/`fallback` OUTSIDE that containment: a panic raised by
    /// the original call itself (or by the fallback) escapes `dispatch`. The
    /// author's replacement wrapper is the `extern "C"` boundary and must
    /// wrap the whole `dispatch` call in its own `catch_unwind` so no panic
    /// ever crosses FFI — an unwind reaching `extern "C"` aborts the
    /// process.
    pub fn dispatch<R>(
        &'static self,
        passthrough: impl FnOnce(T::Original) -> R,
        fallback: impl FnOnce() -> R,
        handler: impl FnOnce(&Callback<'_, T, C>) -> R,
    ) -> R {
        let Some(inner) = self.inner.get() else {
            return fallback();
        };
        if !inner.installed.load(Ordering::Acquire) {
            return fallback();
        }
        let addr = inner.original_addr.load(Ordering::Acquire);
        if addr == 0 {
            return fallback();
        }
        // The reviewed raw→typed boundary of this target.
        // SAFETY: the address came from the backend's bind-time capture of
        // the original pointer for this exact target.
        let original = unsafe { T::original_from_raw(addr) };
        if !inner.gates.runtime.is_open() || !inner.gates.plugin.is_open() {
            return passthrough(original);
        }
        let guard = OriginalGuard::new();
        let cap = CallbackCtx::new();
        let callback = Callback {
            inner,
            guard: &guard,
            cap: &cap,
            _marker: PhantomData,
        };
        match catch_unwind(AssertUnwindSafe(|| handler(&callback))) {
            Ok(result) => {
                if guard.needs_original() {
                    // Exactly-once contract: the handler must call the
                    // original exactly once; enforce it defensively.
                    passthrough(original)
                } else {
                    result
                }
            }
            Err(_) => match guard.phase() {
                OriginalPhase::BeforeOriginal => passthrough(original),
                // The original's effect is unknown; never retry it.
                OriginalPhase::CallingOriginal | OriginalPhase::AfterOriginal => fallback(),
            },
        }
    }
}

/// Callback context handed to the handler when both gates are open.
pub struct Callback<'a, T: HookTarget, C> {
    inner: &'a SiteInner<T, C>,
    guard: &'a OriginalGuard,
    cap: &'a CallbackCtx,
    _marker: PhantomData<*mut ()>,
}

impl<T: HookTarget, C> Callback<'_, T, C> {
    /// Capability token for cross-domain endpoint operations.
    #[must_use]
    pub fn cap(&self) -> &CallbackCtx {
        self.cap
    }

    /// The plugin's registered container state.
    #[must_use]
    pub fn container(&self) -> &C {
        &self.inner.container
    }

    /// Invoke the typed original exactly once. The `call` closure receives
    /// the typed original and invokes it with the handler's own arguments,
    /// keeping ABI handling inside the author-owned wrapper. Returns `None`
    /// when the original was already invoked through this guard.
    pub fn call_original<R>(&self, call: impl FnOnce(T::Original) -> R) -> Option<R> {
        if !self.guard.begin_call() {
            return None;
        }
        let addr = self.inner.original_addr.load(Ordering::Acquire);
        // SAFETY: same bind-time capture contract as `dispatch`.
        let original = unsafe { T::original_from_raw(addr) };
        let result = call(original);
        self.guard.end_call();
        Some(result)
    }
}

/// Builder state: site content not yet published.
pub struct Unpublished;
/// Builder state: site published, `install` available.
pub struct Published;

/// Typestate builder over one static hook site.
pub struct HookBuilder<'ctx, 'host, T: HookTarget, C: 'static, S> {
    pub(crate) ctx: &'ctx mut AppCtx<'host>,
    pub(crate) site: &'static HookSite<T, C>,
    pub(crate) container: Option<Arc<C>>,
    pub(crate) state: PhantomData<S>,
}

impl<'ctx, 'host, T: HookTarget, C: Send + Sync + 'static>
    HookBuilder<'ctx, 'host, T, C, Unpublished>
{
    /// Attach the plugin's container state (shared with the callback
    /// domain).
    pub fn container(mut self, container: Arc<C>) -> Self {
        self.container = Some(container);
        self
    }

    /// Publish the full site (typed-original slot, both gate readers,
    /// container, replacement pointer) into the static `OnceLock`.
    ///
    /// Fails with [`HookError::SiteAlreadyRegistered`] when the target's
    /// static site was already published: one target, one site, process
    /// lifetime.
    pub fn handler(
        self,
        replacement: T::Original,
    ) -> Result<HookBuilder<'ctx, 'host, T, C, Published>, HookError> {
        let container = self
            .container
            .expect("container must be set before handler");
        let inner = SiteInner {
            container,
            gates: SiteGates {
                runtime: self.ctx.host.runtime_gate_reader(),
                plugin: self.ctx.host.owner_gate_reader(),
            },
            replacement,
            original_addr: AtomicUsize::new(0),
            installed: Arc::new(AtomicBool::new(false)),
        };
        self.site
            .inner
            .set(inner)
            .map_err(|_| HookError::SiteAlreadyRegistered)?;
        Ok(HookBuilder {
            ctx: self.ctx,
            site: self.site,
            container: None,
            state: PhantomData,
        })
    }
}

impl<'ctx, 'host, T: HookTarget, C: Send + Sync + 'static>
    HookBuilder<'ctx, 'host, T, C, Published>
{
    /// The only installation path. Resolves and validates the target,
    /// binds the slot, CAS-installs the replacement, confirms by readback,
    /// and records an ownership-aware restore action in the owner ledger.
    ///
    /// A failed readback triggers exactly one ownership-aware rollback
    /// attempt; the conservative `installed` flag only clears when the
    /// original is confirmed back in the slot.
    pub fn install(self) -> Result<InstalledHook<T, C>, PluginError> {
        let Some(inner) = self.site.inner.get() else {
            // Typestate guarantees publish-before-install; this is defense
            // in depth for hand-assembled builders.
            return Err(HookError::SiteAlreadyRegistered.into());
        };
        let resolver = self
            .ctx
            .host
            .method_resolver()
            .ok_or(PluginError::Message("method resolver unavailable"))?;
        let method = resolver.resolve(&T::TARGET)?;
        T::validate(&method)?;
        let memory = resolver.slot_memory(&method);
        let slot = MethodPointerSlot::bind(memory)?;
        let replacement_addr = T::replacement_addr(inner.replacement);

        // Publish the original address before the CAS: once the replacement
        // is reachable, dispatch must always be able to call the original.
        inner
            .original_addr
            .store(slot.original(), Ordering::Release);

        let install_result = slot.install(replacement_addr);
        if let Err(HookError::InstallationFailed) = install_result {
            // Unconfirmed replacement: immediately attempt one
            // ownership-aware rollback. The flag stays false either way:
            // the site remains non-dispatchable.
            let _ = slot.restore(replacement_addr);
            return Err(HookError::InstallationFailed.into());
        }
        install_result?;

        let state = Arc::new(HookState {
            slot,
            replacement: replacement_addr,
            installed: Arc::clone(&inner.installed),
        });
        inner.installed.store(true, Ordering::Release);

        // Ownership-aware restore action; the slot is the final source of
        // truth, drift is never overwritten blindly.
        let restore_state = Arc::clone(&state);
        self.ctx
            .host
            .register_restore_any_thread(crate::phase::RestoreAction::AnyThread(Box::new(
                move || {
                    if !restore_state.installed.swap(false, Ordering::AcqRel) {
                        return Ok(());
                    }
                    match restore_state.slot.restore(restore_state.replacement) {
                        Ok(()) => Ok(()),
                        Err(HookError::OwnershipDrift) => {
                            Err(scsp_core::RestoreError::OwnershipLost)
                        }
                        // Unconfirmed restore: conservatively re-arm so a later
                        // audit can still observe the unowned state.
                        Err(_) => {
                            restore_state.installed.store(true, Ordering::Release);
                            Err(scsp_core::RestoreError::Failed)
                        }
                    }
                },
            )));

        Ok(InstalledHook {
            site: self.site,
            state,
            _marker: PhantomData,
        })
    }
}

/// Installation record shared between the returned handle, the static
/// site's dispatch flag, and the ledger restore action.
struct HookState {
    slot: MethodPointerSlot,
    replacement: usize,
    installed: Arc<AtomicBool>,
}

impl HookState {
    /// Ownership-aware restore: slot CAS + readback must both confirm the
    /// original before the flag clears. Drift or an unconfirmed outcome
    /// keeps the conservative state and is reported.
    fn restore(&self) -> Result<(), HookError> {
        if !self.installed.swap(false, Ordering::AcqRel) {
            return Err(HookError::OwnershipDrift);
        }
        match self.slot.restore(self.replacement) {
            Ok(()) => Ok(()),
            Err(err) => {
                if matches!(err, HookError::InstallationFailed) {
                    // Unconfirmed: conservatively re-arm so the state stays
                    // observable instead of silently claiming restoration.
                    self.installed.store(true, Ordering::Release);
                }
                Err(err)
            }
        }
    }
}

/// Handle returned by a successful install. Dropping it does nothing —
/// restore goes through the owner ledger; `restore` exists for tests and
/// explicit teardown paths and rejects duplicate restoration.
pub struct InstalledHook<T: HookTarget, C: 'static> {
    site: &'static HookSite<T, C>,
    state: Arc<HookState>,
    _marker: PhantomData<fn() -> (T, C)>,
}

impl<T: HookTarget, C> InstalledHook<T, C> {
    /// The static site this hook was installed through.
    #[must_use]
    pub fn site(&self) -> &'static HookSite<T, C> {
        self.site
    }

    /// Whether the conservative installed flag is still set.
    #[must_use]
    pub fn is_installed(&self) -> bool {
        self.state.installed.load(Ordering::Acquire)
    }

    /// Explicit restore. Rejects a second attempt (the first confirmed
    /// restore cleared ownership): the slot no longer holds the
    /// replacement, so a repeat would report drift.
    pub fn restore(&self) -> Result<(), HookError> {
        self.state.restore()
    }
}

/// Generate the unique process-lifetime static site for one target.
///
/// ```ignore
/// define_hook_site!(FPS_TARGET_RATE_SITE: HookSite<SetTargetFrameRateTarget, FpsSites>);
/// ```
#[macro_export]
macro_rules! define_hook_site {
    ($name:ident : HookSite<$target:ty, $container:ty>) => {
        pub static $name: $crate::hook::HookSite<$target, $container> =
            $crate::hook::HookSite::new();
    };
}
