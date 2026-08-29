//! 无游戏 fixture — FPS 解锁插件（v1 测试插件）+ DebugPlugin 全链：
//! 1. 作者自定义 HookTarget + typestate hook（publish → install，gate 关闭）；
//! 2. main→callback latest 路由：setter 替身读容器最新值；
//! 3. fps.set（main 域 debug topic）：dispatch → owner handler → FpsState
//!    更新 + MainWriter 写 latest → 下一帧 setter 生效；
//! 4. fps.get：读 FpsState 返回当前值；
//! 5. runtime.plugins 自省：插件列表含 debug 与 fps；
//! 6. 全链对 mock target 完成（无游戏接触）。
//!
//! 需要 `--features debug`；无 feature 时本文件整体跳过。
//! 对应 docs/plugin-api.md「功能模式示例：FPS 解锁」与 debug 分册。

#![cfg(feature = "debug")]

mod common;

use bevy_ecs::prelude::Resource;
use scsp_core::RuntimeGate;
use scsp_core::{DataRoot, TargetId};
use scsp_plugin_api::debug::{DebugHandlerError, MainDebugTopic};
use scsp_plugin_api::hook::HookTarget;
use scsp_plugin_api::{
    AppCtx, CallbackLatestReader, MainLatestWriter, Plugin, PluginError, StartupCtx, UpdateCtx,
};
use shiny_song_tools::App;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// FPS plugin (mock target)
// ---------------------------------------------------------------------------

const FPS_TARGET: TargetId = TargetId {
    assembly: "MockAssembly.dll",
    namespace: "UnityEngine",
    class: "QualitySettings",
    method: "set_targetFrameRate",
    param_count: 1,
};

#[derive(Clone, Copy)]
struct FpsSetting(i32); // CallbackPayload via the core blanket impl

/// The FPS plugin's callback-visible container.
struct FpsSites {
    setting: CallbackLatestReader<FpsSetting>,
    setter_hits: Arc<AtomicUsize>,
}

struct FpsTargetMarker;

impl HookTarget for FpsTargetMarker {
    const TARGET: TargetId = FPS_TARGET;
    type Original = unsafe extern "C" fn(usize) -> usize;

    fn replacement_addr(original: Self::Original) -> usize {
        original as *const () as usize
    }

    unsafe fn original_from_raw(addr: usize) -> Self::Original {
        // SAFETY: fixture boundary — the slot was seeded with the address of
        // `common::mock_original`, whose ABI exactly matches `Self::Original`.
        unsafe { core::mem::transmute::<usize, Self::Original>(addr) }
    }
}

scsp_plugin_api::define_hook_site!(FPS_TARGET_SITE: HookSite<FpsTargetMarker, FpsSites>);

/// The setter's observable effect: the latest setting it dispatched.
static SETTER_LAST: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn fps_setter_replacement(arg: usize) -> usize {
    // Author-owned wrapper: gate-checked dispatch; the handler reads the
    // latest route value and calls the original with (possibly overridden)
    // arguments.
    FPS_TARGET_SITE.dispatch(
        |original| {
            // SAFETY: real function.
            unsafe { original(arg) }
        },
        || 0,
        |cb| {
            cb.container().setter_hits.fetch_add(1, Ordering::AcqRel);
            let latest = cb.container().setting.try_read(cb.cap());
            let effective = latest.map_or(arg, |setting| setting.0 as usize);
            SETTER_LAST.store(effective, Ordering::Release);
            cb.call_original(|original| {
                // SAFETY: real function.
                unsafe { original(effective) }
            })
            .unwrap_or(effective)
        },
    )
}

#[derive(Resource)]
struct SettingWriter(MainLatestWriter<FpsSetting>);

#[derive(Resource, Default)]
struct FpsState {
    target: Mutex<i32>,
    updates: Arc<AtomicUsize>,
}

// ---------------------------------------------------------------------------
// Debug topics (main domain)
// ---------------------------------------------------------------------------

struct FpsGet;
impl MainDebugTopic for FpsGet {
    const NAME: &'static str = "fps.get";
    type Request = FpsGetRequest;
    type Response = FpsGetResponse;
}

#[derive(serde::Deserialize)]
struct FpsGetRequest {}

#[derive(serde::Serialize)]
struct FpsGetResponse {
    target: i32,
}

struct FpsSet;
impl MainDebugTopic for FpsSet {
    const NAME: &'static str = "fps.set";
    type Request = FpsSetRequest;
    type Response = FpsSetResponse;
}

#[derive(serde::Deserialize)]
struct FpsSetRequest {
    target: i32,
}

#[derive(serde::Serialize)]
struct FpsSetResponse {
    applied: bool,
}

// ---------------------------------------------------------------------------
// FpsPlugin
// ---------------------------------------------------------------------------

struct FpsPlugin;

impl Plugin for FpsPlugin {
    fn name(&self) -> &'static str {
        "fps"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        // State (main domain).
        let updates = Arc::new(AtomicUsize::new(0));
        ctx.insert_resource(FpsState {
            target: Mutex::new(30),
            updates: Arc::clone(&updates),
        })?;

        // main → callback latest route.
        let (writer, reader) = ctx.main_to_callback_latest::<FpsSetting>()?;
        ctx.insert_resource(SettingWriter(writer))?;

        // Container (callback domain).
        let sites = Arc::new(FpsSites {
            setting: reader,
            setter_hits: Arc::new(AtomicUsize::new(0)),
        });
        ctx.register_container(sites.clone())?;

        // Hook: publish → install (gate closed at install time).
        ctx.hook(&FPS_TARGET_SITE)
            .container(sites)
            .handler(fps_setter_replacement as unsafe extern "C" fn(usize) -> usize)?
            .install()?;

        // Debug topics (main domain): the handlers run inside the owner's
        // auto-registered system with their own system params.
        ctx.register_main_debug::<fn(bevy_ecs::prelude::Res<'static, FpsState>), FpsGet, _>(
            fps_get_handler,
        )?;
        ctx.register_main_debug::<fn(
            (
                bevy_ecs::prelude::ResMut<'static, FpsState>,
                bevy_ecs::prelude::Res<'static, SettingWriter>,
            ),
        ), FpsSet, _>(fps_set_handler)?;

        // Startup: applies the initial setting on the first frame (mock:
        // records through the writer is not needed; the state already holds
        // the configured value).
        ctx.add_startup_system(fps_startup);
        Ok(())
    }
}

fn fps_startup(_ctx: StartupCtx<'_>) -> Result<(), PluginError> {
    Ok(())
}

fn fps_get_handler(
    _ctx: UpdateCtx<'_>,
    _request: FpsGetRequest,
    state: bevy_ecs::prelude::Res<FpsState>,
) -> Result<FpsGetResponse, DebugHandlerError> {
    let target = *state.target.lock().expect("state");
    Ok(FpsGetResponse { target })
}

fn fps_set_handler(
    ctx: UpdateCtx<'_>,
    request: FpsSetRequest,
    params: (
        bevy_ecs::prelude::ResMut<FpsState>,
        bevy_ecs::prelude::Res<SettingWriter>,
    ),
) -> Result<FpsSetResponse, DebugHandlerError> {
    let (state, writer) = params;
    *state.target.lock().expect("state") = request.target;
    state.updates.fetch_add(1, Ordering::AcqRel);
    // The next setter invocation uses the new value via the latest route.
    let _ = writer.0.try_send(&ctx, FpsSetting(request.target));
    Ok(FpsSetResponse { applied: true })
}

// ---------------------------------------------------------------------------
// The fixture: full debug round trip against the mock target.
// ---------------------------------------------------------------------------

fn send_frame(stream: &mut UnixStream, body: &serde_json::Value) {
    let json = serde_json::to_vec(body).expect("serialize");
    let mut frame = (json.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&json);
    stream.write_all(&frame).expect("write frame");
    stream.flush().expect("flush");
}

fn wait_for<F: FnMut() -> Option<T>, T>(mut poll: F) -> T {
    for _ in 0..500 {
        if let Some(value) = poll() {
            return value;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("condition not reached");
}

#[test]
fn fps_plugin_debug_round_trip_over_uds() {
    let gate = RuntimeGate::new();
    let data_root = std::env::temp_dir().join(format!("scsp-fps-{}", std::process::id()));
    let mut app = App::new(
        scsp_plugin_api::RuntimeConfig {
            debug: scsp_plugin_api::DebugConfig { enabled: true },
        },
        DataRoot::new(data_root.clone()),
        gate.reader(),
    );
    // Mock resolver must be injected BEFORE plugin build: hooks install
    // during build and need the resolver.
    let resolver = Arc::new(common::MockResolver::new());
    let _fps_slot = resolver.register(&FPS_TARGET);
    app.set_method_resolver(resolver);

    // DebugPlugin first (production plugin list order), then the functional
    // plugin.
    app.add_plugin(shiny_song_tools::debug::DebugPlugin);
    app.add_plugin(FpsPlugin);

    let token = common::fixture_main_token();
    let startup = app.run_startup(&token);
    assert!(startup.retired.is_empty(), "startup must succeed");
    gate.open(); // the runtime layer opens it after Startup

    let socket_path = data_root.join("shiny-song-tools").join("debug.sock");
    let mut client = UnixStream::connect(&socket_path).expect("debug client connects");

    // --- runtime.plugins introspection ---
    send_frame(
        &mut client,
        &serde_json::json!({"jsonrpc": "2.0", "id": "p1", "method": "runtime.plugins", "params": {}}),
    );
    let response = wait_for(|| {
        app.run_update(&common::fixture_main_token());
        read_pending(&mut client)
    });
    assert_eq!(response["id"], "p1");
    let names: Vec<&str> = response["result"]["plugins"]
        .as_array()
        .expect("plugins array")
        .iter()
        .map(|p| p["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["debug", "fps"], "DebugPlugin registered first");
    let fps_row = &response["result"]["plugins"][1];
    assert_eq!(fps_row["state"], "active");
    assert_eq!(fps_row["topics"][0], "fps.get");
    assert_eq!(fps_row["topics"][1], "fps.set");

    // --- fps.get: initial value from config ---
    send_frame(
        &mut client,
        &serde_json::json!({"jsonrpc": "2.0", "id": "g1", "method": "fps.get", "params": {}}),
    );
    let response = wait_for(|| {
        app.run_update(&common::fixture_main_token());
        read_pending(&mut client)
    });
    assert_eq!(response["result"]["target"], 30, "initial config value");

    // --- fps.set: dispatch → handler → latest route → setter ---
    send_frame(
        &mut client,
        &serde_json::json!({"jsonrpc": "2.0", "id": "s1", "method": "fps.set", "params": {"target": 120}}),
    );
    let response = wait_for(|| {
        app.run_update(&common::fixture_main_token());
        read_pending(&mut client)
    });
    assert_eq!(response["result"]["applied"], true);

    // Drive one more frame; then the setter (mock dispatch through the
    // static site) observes 120 through the latest route.
    app.run_update(&common::fixture_main_token());

    // Invoke the plugin's own setter wrapper (as the game would): gates
    // open → handler reads latest (120) → original called with it.
    // SAFETY: mock target; the wrapper is the author-owned ABI entry.
    let setter_result = unsafe { fps_setter_replacement(999) };
    assert_eq!(setter_result, 42, "typed original returned");
    assert_eq!(
        SETTER_LAST.load(Ordering::Acquire),
        120,
        "setter used the route value (unlock semantics)"
    );
}

fn read_pending(client: &mut UnixStream) -> Option<serde_json::Value> {
    client.set_nonblocking(true).expect("nonblocking");
    let mut length = [0u8; 4];
    let read_result = client.read(&mut length);
    match read_result {
        Ok(4) => {
            client.set_nonblocking(false).expect("blocking");
            let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
            client.read_exact(&mut body).expect("body");
            Some(serde_json::from_slice(&body).expect("json"))
        }
        _ => {
            client.set_nonblocking(false).expect("blocking");
            None
        }
    }
}
