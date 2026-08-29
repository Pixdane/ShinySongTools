//! 无游戏 fixture — hook typestate 全链（mock target）：
//! 发布（publish）→ install（CAS + readback）→ gate 打开后的 dispatch
//! （handler 可达、exactly-once original）→ restore（slot 回到 original）→
//! quiescence（静态 site 保活、恢复后 dispatch 走 fallback、
//! 重复 install 与重复 restore 被拒）。
//! 另有 install/restore 过渡窗口测试：dispatch 以 slot 实际值为唯一事实
//! 来源，CAS→flag 窗口内的并发调用必须透传 original（不得跳过）。
//! 对应 docs/plugin-api.md「Hook typestate」与验证顺序 §2.12 第 3 条。

mod common;

use bevy_ecs::prelude::Resource;
use corelib::{DataRoot, HookError, MethodRef, MethodResolver, RuntimeGate, SlotMemory, TargetId};
use plugins::define_hook_site;
use plugins::hook::HookTarget;
use plugins::{AppCtx, MainLatestWriter, Plugin, PluginError, StartupCtx, UpdateCtx};
use shiny_song_tools::{App, PluginState};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use common::{MockResolver, MockSlotMemory, mock_replacement};

pub const MOCK_TARGET: TargetId = TargetId {
    assembly: "MockAssembly.dll",
    namespace: "MockNamespace",
    class: "MockClass",
    method: "MockMethod",
    param_count: 1,
};

#[derive(Clone, Copy)]
pub struct FpsSetting(pub u32); // CallbackPayload via the core blanket impl

/// Callback-domain container shared with the hook.
pub struct MockSites {
    pub setting: plugins::CallbackLatestReader<FpsSetting>,
    pub hits: Arc<AtomicUsize>,
}

pub struct MockTargetMarker;

impl HookTarget for MockTargetMarker {
    const TARGET: TargetId = MOCK_TARGET;
    type Original = unsafe extern "C" fn(usize) -> usize;

    fn replacement_addr(original: Self::Original) -> usize {
        original as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: fixture boundary — the slot was seeded with the address of
        // `common::mock_original`, whose ABI exactly matches `Self::Original`.
        unsafe { core::mem::transmute::<usize, Self::Original>(addr) }
    }
}

define_hook_site!(MOCK_SITE: HookSite<MockTargetMarker, MockSites>);

/// World resource so the test can send values from an Update system.
#[derive(Resource)]
struct SettingWriter(MainLatestWriter<FpsSetting>);

/// World resource carrying the install handle for restore assertions.
#[derive(Resource)]
struct HookHandle(plugins::hook::InstalledHook<MockTargetMarker, MockSites>);

fn send_setting(
    ctx: UpdateCtx<'_>,
    writer: bevy_ecs::prelude::Res<SettingWriter>,
) -> Result<(), PluginError> {
    let _ = writer.0.try_send(&ctx, FpsSetting(60));
    Ok(())
}

fn noop_startup(_ctx: StartupCtx<'_>) -> Result<(), PluginError> {
    Ok(())
}

struct HookPlugin {
    container_hits: Arc<AtomicUsize>,
}

impl Plugin for HookPlugin {
    fn name(&self) -> &'static str {
        "hook-plugin"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        // main → callback latest route; the reader half lives in the
        // container the callback will see.
        let (writer, reader) = ctx.main_to_callback_latest::<FpsSetting>()?;
        ctx.insert_resource(SettingWriter(writer))?;

        let sites = Arc::new(MockSites {
            setting: reader,
            hits: Arc::clone(&self.container_hits),
        });
        ctx.register_container(sites.clone())?;

        // typestate: publish → install. `install` only exists on Published.
        let installed = ctx
            .hook(&MOCK_SITE)
            .container(sites)
            .handler(mock_replacement as unsafe extern "C" fn(usize) -> usize)?
            .install()?;
        ctx.insert_resource(HookHandle(installed))?;

        ctx.add_startup_system(noop_startup);
        ctx.add_update_system(send_setting);
        Ok(())
    }
}

/// Attempts to publish + install the SAME static site again: publish must
/// fail with SiteAlreadyRegistered, which retires this plugin.
struct DuplicateProbe;

impl Plugin for DuplicateProbe {
    fn name(&self) -> &'static str {
        "duplicate-probe"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        let (_w, reader) = ctx.main_to_callback_latest::<FpsSetting>()?;
        let sites = Arc::new(MockSites {
            setting: reader,
            hits: Arc::new(AtomicUsize::new(0)),
        });
        match ctx
            .hook(&MOCK_SITE)
            .container(sites)
            .handler(mock_replacement as unsafe extern "C" fn(usize) -> usize)
        {
            Ok(_) => panic!("republish of a published static site must fail"),
            Err(err) => Err(err.into()),
        }
    }
}

#[test]
fn hook_typestate_publish_install_dispatch_restore_quiescence() {
    let resolver = Arc::new(MockResolver::new());
    let slot = resolver.register(&MOCK_TARGET);
    let original_addr = common::mock_original as *const () as usize;
    let replacement_addr = mock_replacement as *const () as usize;
    assert_ne!(original_addr, replacement_addr);

    let gate = RuntimeGate::new();
    let hits = Arc::new(AtomicUsize::new(0));
    let mut app = App::new(
        plugins::RuntimeConfig::default(),
        DataRoot::new(std::env::temp_dir().join("scsp-fixture-hook")),
        gate.reader(),
    );
    app.set_method_resolver(resolver);
    app.add_plugin(HookPlugin {
        container_hits: Arc::clone(&hits),
    });
    app.add_plugin(DuplicateProbe);

    // 1. Publish happened in build; the site is a process-lifetime retention
    //    root and install CAS'd the replacement in with readback.
    assert!(MOCK_SITE.is_published(), "publish happened in build");
    assert_eq!(
        slot.load(Ordering::Acquire),
        replacement_addr,
        "install confirmed by readback"
    );
    assert!(
        !MOCK_SITE.is_dispatchable(),
        "not dispatchable while gates are closed"
    );

    let records = app.plugins().records();
    assert_eq!(records[0].state, PluginState::Active, "hook plugin active");
    assert_eq!(
        records[1].state,
        PluginState::Retired,
        "duplicate install rejected: plugin retired"
    );

    // 2. Startup opens the plugin gate; the fixture (standing in for the
    //    runtime layer) opens the RuntimeGate after Startup completes.
    let token = common::fixture_main_token();
    let report = app.run_startup(&token);
    assert!(report.retired.is_empty(), "startup must succeed");
    gate.open();
    assert!(MOCK_SITE.is_dispatchable(), "both gates open");

    // 3. Frame: the Update system (capability-checked writer) publishes a
    //    latest value toward the callback domain.
    app.run_update(&token);

    // 4. Dispatch with gates open: handler reachable, container readable,
    //    original called exactly once through the typed capture.
    let passthrough_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let pt = Arc::clone(&passthrough_calls);
    let fb = Arc::clone(&fallback_calls);
    let result = MOCK_SITE.dispatch(
        |original| {
            pt.fetch_add(1, Ordering::AcqRel);
            // SAFETY: mock original is a real callable function.
            unsafe { original(7) }
        },
        || {
            fb.fetch_add(1, Ordering::AcqRel);
            0
        },
        |cb| {
            cb.container().hits.fetch_add(1, Ordering::AcqRel);
            let read = cb.container().setting.try_read(cb.cap());
            assert_eq!(
                read.map(|s| s.0),
                Some(60),
                "main→callback latest value visible in the callback"
            );
            cb.call_original(|original| {
                // SAFETY: real function.
                unsafe { original(100) }
            })
            .expect("exactly-once original available")
        },
    );
    assert_eq!(
        result, 42,
        "typed original captured at bind time was invoked"
    );
    assert_eq!(hits.load(Ordering::Acquire), 1, "container reached");
    assert_eq!(passthrough_calls.load(Ordering::Acquire), 0);
    assert_eq!(fallback_calls.load(Ordering::Acquire), 0);

    // 5. Restore through the install handle (the ledger holds the same
    //    ownership-aware action): slot back to the original, confirmed.
    let handle = app
        .world_mut()
        .remove_resource::<HookHandle>()
        .expect("handle resource present");
    handle.0.restore().expect("first restore succeeds");
    assert_eq!(
        slot.load(Ordering::Acquire),
        original_addr,
        "restore confirmed by readback"
    );
    // 6. Duplicate restore rejected.
    assert!(
        matches!(handle.0.restore(), Err(corelib::HookError::OwnershipDrift)),
        "second restore must be rejected"
    );

    // 7. Quiescence: the static site stays published (never cleared, never
    //    reused), but dispatch now takes the fallback path — no handler, no
    //    original call, no use-after-restore.
    assert!(
        MOCK_SITE.is_published(),
        "site survives restore until process exit"
    );
    assert!(!MOCK_SITE.is_dispatchable());
    let result = MOCK_SITE.dispatch(
        |original| {
            pt.fetch_add(1, Ordering::AcqRel);
            // SAFETY: real function.
            unsafe { original(7) }
        },
        || {
            fb.fetch_add(1, Ordering::AcqRel);
            0
        },
        |_cb| panic!("handler must be unreachable after restore"),
    );
    assert_eq!(result, 0, "fallback produced the return value");
    assert_eq!(
        passthrough_calls.load(Ordering::Acquire),
        0,
        "original not called after restore"
    );
    assert_eq!(fallback_calls.load(Ordering::Acquire), 1);
}

// ---------------------------------------------------------------------------
// 过渡窗口：CAS→flag 之间 replacement 已在 slot 中，dispatch 必须透传
// original（而不是 fallback 跳过 original——那会违反 exactly-once）。
// ---------------------------------------------------------------------------

/// SlotMemory whose restore-direction CAS (replacement → original) can be
/// armed to fail WITHOUT writing: it reproduces the exact restore-window
/// state — `installed` already false, slot still holding the replacement.
struct PickySlot {
    inner: MockSlotMemory,
    original_addr: usize,
    fail_restore_cas: AtomicBool,
}

impl SlotMemory for PickySlot {
    fn read(&self) -> Option<usize> {
        self.inner.read()
    }

    fn compare_exchange(&self, expected: usize, new: usize) -> Result<(), usize> {
        // The restore CAS is the one writing the original back.
        if self.fail_restore_cas.load(Ordering::Acquire) && new == self.original_addr {
            return Err(self.inner.0.load(Ordering::Acquire));
        }
        self.inner.compare_exchange(expected, new)
    }
}

/// Resolver delegating resolution to a [`MockResolver`] but handing out the
/// picky slot memory, so the fixture controls the CAS outcome.
struct PickyResolver {
    inner: MockResolver,
    picky: Mutex<Option<Arc<PickySlot>>>,
}

impl MethodResolver for PickyResolver {
    fn resolve(&self, target: &TargetId) -> Result<MethodRef, HookError> {
        self.inner.resolve(target)
    }

    fn slot_memory(&self, _method: &MethodRef) -> Arc<dyn SlotMemory> {
        self.picky
            .lock()
            .expect("picky lock")
            .clone()
            .expect("picky slot registered")
    }
}

define_hook_site!(WINDOW_SITE: HookSite<MockTargetMarker, MockSites>);

#[derive(Resource)]
struct WindowHandle(plugins::hook::InstalledHook<MockTargetMarker, MockSites>);

struct WindowPlugin;

impl Plugin for WindowPlugin {
    fn name(&self) -> &'static str {
        "window-plugin"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        let (_writer, reader) = ctx.main_to_callback_latest::<FpsSetting>()?;
        let sites = Arc::new(MockSites {
            setting: reader,
            hits: Arc::new(AtomicUsize::new(0)),
        });
        ctx.register_container(sites.clone())?;
        let installed = ctx
            .hook(&WINDOW_SITE)
            .container(sites)
            .handler(mock_replacement as unsafe extern "C" fn(usize) -> usize)?
            .install()?;
        ctx.insert_resource(WindowHandle(installed))?;
        Ok(())
    }
}

#[test]
fn restore_window_passthroughs_original_instead_of_skipping_it() {
    let original_addr = common::mock_original as *const () as usize;
    let slot_word = Arc::new(AtomicUsize::new(original_addr));
    let picky = Arc::new(PickySlot {
        inner: MockSlotMemory(Arc::clone(&slot_word)),
        original_addr,
        fail_restore_cas: AtomicBool::new(false),
    });

    let inner_resolver = MockResolver::new();
    inner_resolver.register(&MOCK_TARGET);
    let resolver = Arc::new(PickyResolver {
        inner: inner_resolver,
        picky: Mutex::new(Some(Arc::clone(&picky))),
    });

    let gate = RuntimeGate::new();
    let mut app = App::new(
        plugins::RuntimeConfig::default(),
        DataRoot::new(std::env::temp_dir().join("scsp-fixture-hook-window")),
        gate.reader(),
    );
    app.set_method_resolver(resolver);
    app.add_plugin(WindowPlugin);
    // RuntimeGate stays CLOSED: the passthrough path is what must run.

    // Install confirmed: the slot holds the replacement.
    assert_eq!(slot_word.load(Ordering::Acquire), replacement_addr_of());

    // Arm the picky CAS and restore: the flag clears, the CAS refuses to
    // write, the slot STILL holds the replacement — the restore window.
    let handle = app
        .world_mut()
        .remove_resource::<WindowHandle>()
        .expect("handle resource present");
    picky.fail_restore_cas.store(true, Ordering::Release);
    assert!(
        matches!(handle.0.restore(), Err(HookError::OwnershipDrift)),
        "armed CAS refusal must report drift"
    );
    assert_eq!(
        slot_word.load(Ordering::Acquire),
        replacement_addr_of(),
        "refused CAS must not write: replacement still live"
    );

    // A game call landing in this window must reach the original exactly
    // once (passthrough) — never the fallback (which would skip the
    // original the game expects) and never the handler.
    let passthrough_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::new(AtomicUsize::new(0));
    let pt = Arc::clone(&passthrough_calls);
    let fb = Arc::clone(&fallback_calls);
    let hc = Arc::clone(&handler_calls);
    let result = WINDOW_SITE.dispatch(
        |original| {
            pt.fetch_add(1, Ordering::AcqRel);
            // SAFETY: mock original is a real callable function.
            unsafe { original(7) }
        },
        || {
            fb.fetch_add(1, Ordering::AcqRel);
            0
        },
        |_cb| {
            hc.fetch_add(1, Ordering::AcqRel);
            0
        },
    );
    assert_eq!(result, 42, "original invoked through passthrough");
    assert_eq!(passthrough_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        fallback_calls.load(Ordering::Acquire),
        0,
        "fallback would skip the original"
    );
    assert_eq!(
        handler_calls.load(Ordering::Acquire),
        0,
        "handler unreachable while flag is false"
    );
}

fn replacement_addr_of() -> usize {
    mock_replacement as *const () as usize
}
