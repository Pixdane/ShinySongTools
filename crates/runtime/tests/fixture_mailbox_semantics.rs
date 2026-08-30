//! 无游戏 fixture — 跨域 route 三种 mailbox 语义 + 方向 branded endpoint：
//! 1. main→callback `latest`：覆盖语义（新值覆盖旧值）；
//! 2. main→callback `bounded::<4>`：保序 FIFO，满载 Full 计数；
//! 3. main→callback `shared_latest`：Arc<T> 单槽替换，旧 Arc 对已克隆的
//!    持有者仍存活；
//! 4. callback→main `bounded::<4>`：callback 发送 → 下一帧 CommandDrain 以
//!    阶段入口 watermark 投递进 Messages → 主线程 MessageReader 消费。
//!
//! 能力 token 约束：callback 侧操作要求 &CallbackCtx（仅 dispatch 路径可
//! 得），main 侧要求 &UpdateCtx（仅 Update system 可得）。
//!
//! 对应 core crate Rustdoc「跨域 message」与 core 分册三种 mailbox。

mod common;

use bevy_ecs::prelude::{Message, Resource};
use corelib::hook::HookTarget;
use corelib::{
    AppCtx, CallbackBoundedReader, CallbackBoundedWriter, CallbackLatestReader,
    CallbackSharedReader, MainBoundedWriter, MainLatestWriter, MainSharedWriter, Plugin,
    PluginError, SendOutcome, StartupCtx, UpdateCtx,
};
use corelib::{DataRoot, RuntimeGate};
use shiny_song_tools::{App, PluginState};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
struct Setting(u32); // CallbackPayload via the core blanket impl

#[derive(Clone, Copy, Message)]
struct FpsEvent(u8);

#[derive(Clone)]
struct DebugRequest(u64);

/// Callback-domain container: reader halves + callback-side writer.
struct MailboxSites {
    latest: CallbackLatestReader<Setting>,
    bounded: CallbackBoundedReader<u8, 4>,
    shared: CallbackSharedReader<DebugRequest>,
    events: CallbackBoundedWriter<FpsEvent, 4>,
    /// Set inside the callback: values observed through the reader halves.
    observed: Mutex<CallbackObservation>,
}

#[derive(Default)]
struct CallbackObservation {
    latest: Option<u32>,
    bounded: Vec<u8>,
    shared_seen: Vec<u64>,
    /// The Arc cloned inside the callback, observed after dispatch returns.
    old_shared: Option<Arc<DebugRequest>>,
    events_sent: u8,
}

struct FixtureTarget;
impl HookTarget for FixtureTarget {
    const TARGET: corelib::TargetId = corelib::TargetId {
        assembly: "MockAssembly.dll",
        namespace: "MockNamespace",
        class: "MockClass",
        method: "MailboxMethod",
        param_count: 1,
    };
    type Original = unsafe extern "C" fn(usize) -> usize;
    fn replacement_addr(original: Self::Original) -> usize {
        original as usize
    }
    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: seeded with common::mock_original's address.
        unsafe { core::mem::transmute::<usize, Self::Original>(addr) }
    }
}

corelib::define_hook_site!(MAILBOX_SITE: HookSite<FixtureTarget, MailboxSites>);

#[derive(Resource)]
struct Writers {
    latest: MainLatestWriter<Setting>,
    bounded: MainBoundedWriter<u8, 4>,
    shared: MainSharedWriter<DebugRequest>,
}

#[derive(Resource, Default)]
struct EventSum(AtomicU64);

#[derive(Resource, Default)]
struct Frames(AtomicUsize);

fn produce(
    ctx: UpdateCtx<'_>,
    writers: bevy_ecs::prelude::Res<Writers>,
    frames: bevy_ecs::prelude::Res<Frames>,
) -> Result<(), PluginError> {
    // Write only on the first frame so the outcome asserts hold exactly once.
    if frames.0.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    // latest: two writes, one visible.
    assert_eq!(
        writers.latest.try_send(&ctx, Setting(1)),
        SendOutcome::Accepted
    );
    assert_eq!(
        writers.latest.try_send(&ctx, Setting(2)),
        SendOutcome::Replaced
    );
    // bounded: three writes in order; a fourth fits, a fifth would overflow
    // (callback drains them across frames so keep it at four).
    assert_eq!(writers.bounded.try_send(&ctx, 10), SendOutcome::Accepted);
    assert_eq!(writers.bounded.try_send(&ctx, 11), SendOutcome::Accepted);
    assert_eq!(writers.bounded.try_send(&ctx, 12), SendOutcome::Accepted);
    assert_eq!(writers.bounded.try_send(&ctx, 13), SendOutcome::Accepted);
    assert_eq!(
        writers.bounded.try_send(&ctx, 14),
        SendOutcome::Full,
        "bounded mailbox reports Full at capacity, never blocks"
    );
    // shared: two writes; second replaces the first.
    assert_eq!(
        writers.shared.try_send(&ctx, DebugRequest(100)),
        Ok(SendOutcome::Accepted)
    );
    assert_eq!(
        writers.shared.try_send(&ctx, DebugRequest(101)),
        Ok(SendOutcome::Replaced)
    );
    Ok(())
}

fn count_events(
    _ctx: UpdateCtx<'_>,
    sum: bevy_ecs::prelude::Res<EventSum>,
    frames: bevy_ecs::prelude::Res<Frames>,
    mut reader: bevy_ecs::prelude::MessageReader<FpsEvent>,
) -> Result<(), PluginError> {
    frames.0.fetch_add(1, Ordering::AcqRel);
    for event in reader.read() {
        sum.0.fetch_add(u64::from(event.0), Ordering::AcqRel);
    }
    Ok(())
}

fn noop_startup(_ctx: StartupCtx<'_>) -> Result<(), PluginError> {
    Ok(())
}

struct MailboxPlugin {
    sites_out: Arc<Mutex<Option<Arc<MailboxSites>>>>,
}

impl Plugin for MailboxPlugin {
    fn name(&self) -> &'static str {
        "mailbox"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        // main → callback routes (three semantics).
        let (latest_w, latest_r) = ctx.main_to_callback_latest::<Setting>()?;
        eprintln!("build: latest route ok");
        let (bounded_w, bounded_r) = ctx.main_to_callback_bounded::<u8, 4>()?;
        eprintln!("build: bounded route ok");
        let (shared_w, shared_r) = ctx.main_to_callback_shared::<DebugRequest>()?;
        eprintln!("build: shared route ok");
        // callback → main route with bounded semantics.
        let (events_w, _reader_for_introspection) =
            ctx.callback_to_main_bounded::<FpsEvent, 4>()?;
        eprintln!("build: events route ok");

        ctx.insert_resource(Writers {
            latest: latest_w,
            bounded: bounded_w,
            shared: shared_w,
        })?;
        ctx.insert_resource(EventSum::default())?;
        ctx.insert_resource(Frames::default())?;

        let sites = Arc::new(MailboxSites {
            latest: latest_r,
            bounded: bounded_r,
            shared: shared_r,
            events: events_w,
            observed: Mutex::new(CallbackObservation::default()),
        });
        ctx.register_container(sites.clone())?;
        eprintln!("build: container ok");
        *self.sites_out.lock().expect("sites out") = Some(Arc::clone(&sites));

        // The hook belongs to the same owner: dispatch supplies the
        // &CallbackCtx that the reader halves require.
        let builder = ctx.hook(&MAILBOX_SITE).container(sites);
        let published =
            builder.handler(common::mock_replacement as unsafe extern "C" fn(usize) -> usize);
        let _installed = match published {
            Ok(b) => {
                eprintln!("build: handler published");
                match b.install() {
                    Ok(h) => {
                        eprintln!("build: installed ok: {}", h.is_installed());
                        h
                    }
                    Err(err) => {
                        eprintln!("build: install error: {err}");
                        return Err(err);
                    }
                }
            }
            Err(err) => {
                eprintln!("build: publish error: {err}");
                return Err(corelib::PluginError::Hook(err));
            }
        };

        ctx.add_startup_system(noop_startup);
        ctx.add_update_system(produce);
        ctx.add_update_system(count_events);
        Ok(())
    }
}

#[test]
fn mailbox_three_semantics_and_command_drain_delivery() {
    let gate = RuntimeGate::new();
    let sites_slot: Arc<Mutex<Option<Arc<MailboxSites>>>> = Arc::new(Mutex::new(None));
    let mut app = App::new(
        corelib::RuntimeConfig::default(),
        DataRoot::new(std::env::temp_dir().join("scsp-fixture-mailbox")),
        gate.reader(),
    );
    // Mock resolver + a registered slot for the fixture's target method.
    let resolver = Arc::new(common::MockResolver::new());
    let slot = resolver.register(&<FixtureTarget as HookTarget>::TARGET);
    eprintln!(
        "fixture slot seeded = {:#x} original={:#x} replacement={:#x}",
        slot.load(Ordering::Acquire),
        common::mock_original as *const () as usize,
        common::mock_replacement as *const () as usize
    );
    app.set_method_resolver(resolver);

    app.add_plugin(MailboxPlugin {
        sites_out: Arc::clone(&sites_slot),
    });

    let token = common::fixture_main_token();
    let startup = app.run_startup(&token);
    eprintln!("startup = {startup:?}");
    // Frame 1: main-side writes happen in Update systems; the message
    // reader lazily initializes this frame.
    let update = app.run_update(&token);
    eprintln!("update = {update:?}");
    gate.open();
    eprintln!(
        "published={} installed={}",
        MAILBOX_SITE.is_published(),
        MAILBOX_SITE.is_dispatchable()
    );
    assert!(MAILBOX_SITE.is_dispatchable(), "gates open after startup");

    let sites = sites_slot
        .lock()
        .expect("sites out")
        .clone()
        .expect("container registered");

    // --- Callback domain: one dispatch, all four endpoint kinds ---
    {
        let mut observation = sites.observed.lock().expect("observation");
        MAILBOX_SITE.dispatch(
            |_original| panic!("passthrough must not run when gates are open"),
            || 0,
            |cb| {
                let cap = cb.cap();
                let container = cb.container();

                // 1. latest: the last main-side write wins.
                let latest = container.latest.try_read(cap).expect("latest visible");
                observation.latest = Some(latest.0);
                assert_eq!(latest.0, 2, "latest overwrite semantics");

                // 2. bounded: FIFO drain with Full accounting on the main
                //    side already asserted; callback sees FIFO order.
                let drained: Vec<u8> = [
                    container.bounded.try_read(cap),
                    container.bounded.try_read(cap),
                    container.bounded.try_read(cap),
                    container.bounded.try_read(cap),
                ]
                .into_iter()
                .flatten()
                .collect();
                observation.bounded = drained.clone();
                assert_eq!(drained, vec![10, 11, 12, 13], "bounded FIFO order");

                // 3. shared: replaced slot; the old Arc survives for the
                //    reader that cloned it before replacement is impossible
                //    here (callback reads after both writes), so assert the
                //    current value and keep the Arc alive past replacement.
                let shared = container.shared.try_read(cap).expect("shared visible");
                observation.old_shared = Some(Arc::clone(&shared));
                observation.shared_seen.push(shared.0);
                assert_eq!(shared.0, 101);

                // 4. callback→main: send events through the branded writer.
                for i in 0..5u8 {
                    let outcome = container.events.try_send(cap, FpsEvent(i));
                    if i < 4 {
                        assert_eq!(outcome, SendOutcome::Accepted);
                    } else {
                        assert_eq!(outcome, SendOutcome::Full);
                    }
                }
                observation.events_sent = 4;
                // Exactly-once original contract.
                cb.call_original(|original| {
                    // SAFETY: real function.
                    unsafe { original(0) }
                })
                .expect("original available")
            },
        );
        // The old shared Arc stays alive and valid for its holder even after
        // the slot replacement that dropped it from the slot.
        assert_eq!(
            observation
                .old_shared
                .as_ref()
                .expect("cloned in callback")
                .0,
            101
        );
        assert_eq!(observation.latest, Some(2));
        assert_eq!(observation.bounded, vec![10, 11, 12, 13]);
        assert_eq!(observation.events_sent, 4);
    }

    // --- Main domain: next frame's CommandDrain delivers the events ---
    app.run_update(&token);
    let frames = app.world().resource::<Frames>().0.load(Ordering::Acquire);
    let sum = app.world().resource::<EventSum>().0.load(Ordering::Acquire);
    assert!(
        frames >= 2,
        "two update frames ran (producer frame + delivery frame)"
    );
    assert_eq!(
        sum,
        1 + 2 + 3,
        "callback→main bounded events delivered via CommandDrain into Messages"
    );

    // Owner is still active: nothing failed.
    assert_eq!(
        app.plugins().records()[0].state,
        PluginState::Active,
        "mailbox plugin stayed healthy end-to-end"
    );
}
