//! Shared no-game fixture helpers: mock method resolver, mock slot memory,
//! and a callable mock original/replacement pair. Nothing here touches a
//! game process or real IL2CPP metadata.

#![allow(dead_code)]

use scsp_core::{HookError, MainThreadToken, MethodRef, MethodResolver, SlotMemory, TargetId};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A process-safe fake slot word backed by an `AtomicUsize` (never
/// dereferenced as memory).
#[derive(Debug)]
pub struct MockSlotMemory(pub Arc<AtomicUsize>);

impl SlotMemory for MockSlotMemory {
    fn read(&self) -> Option<usize> {
        Some(self.0.load(Ordering::Acquire))
    }

    fn compare_exchange(&self, expected: usize, new: usize) -> Result<(), usize> {
        self.0
            .compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire)
            .map(drop)
    }
}

/// Real callable stand-ins: the slot's captured "original" is the address of
/// [`mock_original`], so the exactly-once paths can actually call it.
pub unsafe extern "C" fn mock_original(_arg: usize) -> usize {
    42
}

pub unsafe extern "C" fn mock_replacement(arg: usize) -> usize {
    arg + 1
}

/// Resolver over a fixed table of fake slots. `resolve` succeeds only for
/// targets registered via [`MockResolver::register`], producing a
/// [`MethodRef`] whose slot memory is the registered mock.
#[derive(Default, Debug)]
pub struct MockResolver {
    slots: Mutex<HashMap<&'static str, Arc<AtomicUsize>>>,
    next_addr: AtomicUsize,
}

impl MockResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one fake method slot for `target.method`, seeded with the
    /// mock original's address.
    pub fn register(&self, target: &TargetId) -> Arc<AtomicUsize> {
        let mut slots = self.slots.lock().expect("slots lock");
        slots
            .entry(target.method)
            .or_insert_with(|| Arc::new(AtomicUsize::new(mock_original as *const () as usize)))
            .clone()
    }

    /// Overwrite the current slot value of one registered method (drift
    /// simulation).
    pub fn force_slot(&self, method: &'static str, value: usize) {
        let slots = self.slots.lock().expect("slots lock");
        if let Some(slot) = slots.get(method) {
            slot.store(value, Ordering::Release);
        }
    }
}

impl MethodResolver for MockResolver {
    fn resolve(&self, target: &TargetId) -> Result<MethodRef, HookError> {
        let slots = self.slots.lock().expect("slots lock");
        if !slots.contains_key(target.method) {
            return Err(HookError::TargetUnavailable);
        }
        drop(slots);
        // Fake, unique, never dereferenced metadata address.
        let addr = self.next_addr.fetch_add(0x10, Ordering::AcqRel).max(0x1000);
        Ok(MethodRef {
            assembly: target.assembly.to_owned(),
            namespace: target.namespace.to_owned(),
            class: target.class.to_owned(),
            method: target.method.to_owned(),
            param_count: target.param_count,
            method_info: addr,
            method_pointer_slot: addr,
        })
    }

    fn slot_memory(&self, method: &MethodRef) -> Arc<dyn SlotMemory> {
        let slots = self.slots.lock().expect("slots lock");
        let slot = slots
            .get(method.method.as_str())
            .expect("slot registered in resolve");
        Arc::new(MockSlotMemory(Arc::clone(slot)))
    }
}

/// Fixture token. Constructing it here stands in for the runtime scheduler's
/// reviewed boundary; no driver in these fixtures performs thread checks
/// (that is the scheduler phase's responsibility).
pub fn fixture_main_token() -> MainThreadToken {
    // SAFETY: fixture-only stand-in for the scheduler's verified boundary.
    unsafe { MainThreadToken::assume_main_thread() }
}

// ---------------------------------------------------------------------------
// Phase C: mock IL2CPP backend with ladder-order enforcement.
// ---------------------------------------------------------------------------

use scsp_core::{
    AttachGuard, DomainHandle, Il2CppApi, Il2CppError, ImageHandle, ImageIdentity, RuntimeIdentity,
};
use std::sync::atomic::AtomicU32;

/// Mock backend driving the readiness ladder. Enforces ladder order: any
/// call beyond the reached rung returns `NotReady`. Counts `domain_get`
/// calls so fixtures can assert the exactly-once experiment decision.
#[derive(Debug, Default)]
pub struct MockIl2Cpp {
    pub rung: AtomicU32,
    pub domain_get_calls: AtomicU32,
    /// `domain_get` returns null (terminates the one-shot bootstrap).
    pub null_domain: bool,
    /// `runtime_identity` reports a mismatch.
    pub identity_mismatch: bool,
    /// Attach fails.
    pub attach_fails: bool,
}

impl MockIl2Cpp {
    pub fn new() -> Self {
        Self {
            rung: AtomicU32::new(1),
            domain_get_calls: AtomicU32::new(0),
            null_domain: false,
            identity_mismatch: false,
            attach_fails: false,
        }
    }

    fn require_rung(&self, rung: u32) -> Result<(), Il2CppError> {
        if self.rung.load(Ordering::Acquire) >= rung {
            Ok(())
        } else {
            Err(Il2CppError::NotReady)
        }
    }

    fn advance(&self, rung: u32) {
        self.rung.store(
            rung.max(self.rung.load(Ordering::Acquire)),
            Ordering::Release,
        );
    }
}

impl Il2CppApi for MockIl2Cpp {
    fn unity_framework_image(&self) -> Result<ImageIdentity, Il2CppError> {
        self.advance(1);
        Ok(ImageIdentity {
            name: "UnityFramework".to_owned(),
            handle: ImageHandle(0x1),
        })
    }

    fn load_exports(&self) -> Result<(), Il2CppError> {
        self.require_rung(1)?;
        self.advance(2);
        Ok(())
    }

    fn domain_get(&self) -> Result<DomainHandle, Il2CppError> {
        self.require_rung(2)?;
        self.domain_get_calls.fetch_add(1, Ordering::AcqRel);
        if self.null_domain {
            return Err(Il2CppError::DomainUnavailable);
        }
        self.advance(3);
        Ok(DomainHandle(0x42))
    }

    fn attach_current_thread(&self) -> Result<AttachGuard, Il2CppError> {
        self.require_rung(3)?;
        if self.attach_fails {
            return Err(Il2CppError::AttachFailed);
        }
        self.advance(4);
        Ok(AttachGuard::new(|| {}))
    }

    fn hydrate_metadata(&self) -> Result<(), Il2CppError> {
        self.require_rung(4)?;
        self.advance(5);
        Ok(())
    }

    fn runtime_identity(&self) -> Result<RuntimeIdentity, Il2CppError> {
        self.require_rung(5)?;
        if self.identity_mismatch {
            return Err(Il2CppError::IdentityMismatch);
        }
        Ok(RuntimeIdentity {
            unity_version: "mock".to_owned(),
            il2cpp_variant: "mock".to_owned(),
        })
    }
}

// A real callable mock LateUpdate for the scheduler slot's captured
// original. Counting is thread-local: parallel test threads never pollute
// each other, and every scheduler frame runs on one thread.
thread_local! {
    static ORIGINAL_CALLS: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
    static ORIGINAL_THIS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    static ORIGINAL_METHOD: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

pub fn reset_original_calls() {
    ORIGINAL_CALLS.with(|c| c.set(0));
    ORIGINAL_THIS.with(|c| c.set(0));
    ORIGINAL_METHOD.with(|c| c.set(0));
}

pub fn original_calls() -> u32 {
    ORIGINAL_CALLS.with(|c| c.get())
}

/// `this` pointer the mock original last received (0 = never called).
pub fn original_last_this() -> usize {
    ORIGINAL_THIS.with(|c| c.get())
}

/// `method` pointer the mock original last received.
pub fn original_last_method() -> usize {
    ORIGINAL_METHOD.with(|c| c.get())
}

pub unsafe extern "C" fn mock_lateupdate(
    this: *mut shiny_song_tools::Il2CppObjectOpaque,
    method: *const shiny_song_tools::MethodInfoOpaque,
) {
    ORIGINAL_CALLS.with(|c| c.set(c.get() + 1));
    ORIGINAL_THIS.with(|c| c.set(this as usize));
    ORIGINAL_METHOD.with(|c| c.set(method as usize));
}

/// Shared counting plugin: an Update system that runs once per frame.
#[derive(Default)]
pub struct CountingUpdatePlugin;

impl scsp_plugin_api::Plugin for CountingUpdatePlugin {
    fn name(&self) -> &'static str {
        "counting"
    }

    fn build(&self, ctx: &mut scsp_plugin_api::AppCtx<'_>) -> Result<(), scsp_core::PluginError> {
        ctx.insert_resource(FrameCounter::default())?;
        ctx.add_update_system(counting_update);
        Ok(())
    }
}

#[derive(Default, bevy_ecs::prelude::Resource)]
pub struct FrameCounter(AtomicUsize);

fn counting_update(
    _ctx: scsp_plugin_api::UpdateCtx<'_>,
    counter: bevy_ecs::prelude::Res<FrameCounter>,
) -> Result<(), scsp_core::PluginError> {
    counter.0.fetch_add(1, Ordering::AcqRel);
    Ok(())
}
