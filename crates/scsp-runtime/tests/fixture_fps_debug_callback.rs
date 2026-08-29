//! 无游戏 fixture — callback 域 debug topic 端到端（docs/debug-diagnostics-logging.md
//! 「callback 域数据通路」）：
//! 1. `register_callback_debug` 自动登记 relay system + 容器端点；
//! 2. dispatch → relay 投递请求到容器 SharedSlot（每帧至多一个、不覆盖）；
//! 3. hook 自然进入时 `handle_pending` 处理（每次进入至多一个请求）；
//! 4. 响应经 relay 以 FIFO 配对 id 回到 wire；
//! 5. 两个排队请求由两次进入分别消化（预算与不覆盖语义）。
//!
//! 需要 `--features debug`；无 feature 时本文件整体跳过。

#![cfg(feature = "debug")]

mod common;

use scsp_core::RuntimeGate;
use scsp_core::{DataRoot, TargetId};
use scsp_plugin_api::debug::{CallbackDebugEndpoints, CallbackDebugTopic};
use scsp_plugin_api::hook::HookTarget;
use scsp_plugin_api::{AppCtx, Plugin, PluginError};
use shiny_song_tools::App;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

// ---------------------------------------------------------------------------
// Probe plugin (mock target, callback-domain debug topic)
// ---------------------------------------------------------------------------

const PROBE_TARGET: TargetId = TargetId {
    assembly: "MockAssembly.dll",
    namespace: "UnityEngine",
    class: "QualitySettings",
    method: "set_targetFrameRate",
    param_count: 1,
};

struct FpsProbe;
impl CallbackDebugTopic for FpsProbe {
    const NAME: &'static str = "fps.probe";
    type Request = FpsProbeRequest;
    type Response = FpsProbeResponse;
}

#[derive(serde::Deserialize)]
struct FpsProbeRequest {
    scale: i32,
}

#[derive(serde::Serialize)]
struct FpsProbeResponse {
    value: i32,
}

/// Test-visible count of probe requests the callback actually handled
/// (shared with the plugin build via the OnceLock below).
fn handled_counter() -> Arc<AtomicUsize> {
    static HANDLED: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    HANDLED
        .get_or_init(|| Arc::new(AtomicUsize::new(0)))
        .clone()
}

/// The probe plugin's callback-visible container.
struct ProbeSites {
    probe: CallbackDebugEndpoints<FpsProbeRequest, FpsProbeResponse>,
    /// Current baseline (callback-safe atom; stands in for live state).
    baseline: Arc<AtomicI32>,
}

struct ProbeTargetMarker;

impl HookTarget for ProbeTargetMarker {
    const TARGET: TargetId = PROBE_TARGET;
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

scsp_plugin_api::define_hook_site!(PROBE_TARGET_SITE: HookSite<ProbeTargetMarker, ProbeSites>);

unsafe extern "C" fn probe_setter_replacement(arg: usize) -> usize {
    PROBE_TARGET_SITE.dispatch(
        |original| {
            // SAFETY: real function.
            unsafe { original(arg) }
        },
        || 0,
        |cb| {
            // The callback-domain debug work: at most one request per entry.
            let baseline = cb.container().baseline.load(Ordering::Acquire);
            let handled = handled_counter();
            cb.container().probe.handle_pending(cb.cap(), |request| {
                handled.fetch_add(1, Ordering::AcqRel);
                Ok(FpsProbeResponse {
                    value: baseline * request.scale,
                })
            });
            cb.call_original(|original| {
                // SAFETY: real function.
                unsafe { original(arg) }
            })
            .unwrap_or(arg)
        },
    )
}

struct ProbePlugin;

impl Plugin for ProbePlugin {
    fn name(&self) -> &'static str {
        "fps"
    }

    fn build(&self, ctx: &mut AppCtx<'_>) -> Result<(), PluginError> {
        // Callback-domain topic FIRST (returns the endpoints), then the
        // container that holds them.
        let probe = ctx.register_callback_debug::<FpsProbe>()?;
        let sites = Arc::new(ProbeSites {
            probe,
            baseline: Arc::new(AtomicI32::new(30)),
        });
        ctx.register_container(sites.clone())?;

        ctx.hook(&PROBE_TARGET_SITE)
            .container(sites)
            .handler(probe_setter_replacement as unsafe extern "C" fn(usize) -> usize)?
            .install()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The fixture: wire → dispatch → relay → callback → relay → wire
// ---------------------------------------------------------------------------

fn send_frame(stream: &mut UnixStream, body: &serde_json::Value) {
    let json = serde_json::to_vec(body).expect("serialize");
    let mut frame = (json.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&json);
    stream.write_all(&frame).expect("write frame");
    stream.flush().expect("flush");
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

/// One pump step: drive a frame and enter the hook (as the game would), then
/// poll the wire for a response.
fn pump(app: &mut App, client: &mut UnixStream) -> Option<serde_json::Value> {
    // SAFETY: mock target; the wrapper is the author-owned ABI entry.
    let _ = unsafe { probe_setter_replacement(7) };
    app.run_update(&common::fixture_main_token());
    read_pending(client)
}

#[test]
fn callback_domain_debug_round_trip_over_uds() {
    let gate = RuntimeGate::new();
    let data_root = std::env::temp_dir().join(format!("scsp-probe-{}", std::process::id()));
    let mut app = App::new(
        scsp_plugin_api::RuntimeConfig {
            debug: scsp_plugin_api::DebugConfig { enabled: true },
        },
        DataRoot::new(data_root.clone()),
        gate.reader(),
    );
    let resolver = Arc::new(common::MockResolver::new());
    let _fps_slot = resolver.register(&PROBE_TARGET);
    app.set_method_resolver(resolver);

    // DebugPlugin first (production plugin list order), then the functional
    // plugin.
    app.add_plugin(shiny_song_tools::debug::DebugPlugin);
    app.add_plugin(ProbePlugin);

    let token = common::fixture_main_token();
    let startup = app.run_startup(&token);
    assert!(startup.retired.is_empty(), "startup must succeed");
    gate.open(); // the runtime layer opens it after Startup

    let socket_path = data_root.join("shiny-song-tools").join("debug.sock");
    let mut client = UnixStream::connect(&socket_path).expect("debug client connects");

    // --- single request: dispatch → relay slot → callback entry → wire ---
    send_frame(
        &mut client,
        &serde_json::json!({"jsonrpc": "2.0", "id": "c1", "method": "fps.probe", "params": {"scale": 2}}),
    );
    let mut response = None;
    for _ in 0..500 {
        if let Some(got) = pump(&mut app, &mut client) {
            response = Some(got);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let response = response.expect("probe response arrived");
    assert_eq!(response["id"], "c1");
    assert_eq!(response["result"]["value"], 60, "baseline 30 × scale 2");

    // --- budget + no-overwrite: two queued requests, one per entry ---
    send_frame(
        &mut client,
        &serde_json::json!({"jsonrpc": "2.0", "id": "c2", "method": "fps.probe", "params": {"scale": 3}}),
    );
    send_frame(
        &mut client,
        &serde_json::json!({"jsonrpc": "2.0", "id": "c3", "method": "fps.probe", "params": {"scale": 4}}),
    );

    let mut got: Vec<serde_json::Value> = Vec::new();
    for _ in 0..500 {
        if let Some(response) = pump(&mut app, &mut client) {
            got.push(response);
            if got.len() == 2 {
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(got.len(), 2, "both requests answered");
    // FIFO pairing: c2 (scale 3) answered before c3 (scale 4).
    assert_eq!(got[0]["id"], "c2");
    assert_eq!(got[0]["result"]["value"], 90, "baseline 30 × scale 3");
    assert_eq!(got[1]["id"], "c3");
    assert_eq!(got[1]["result"]["value"], 120, "baseline 30 × scale 4");

    // Handled count == sent count (c1 + c2 + c3): the single-slot relay
    // delivers one request at a time and `handle_pending` takes at most one
    // per hook entry — no duplication, no loss, even with two requests
    // queued at once.
    assert_eq!(
        handled_counter().load(Ordering::Acquire),
        3,
        "each request handled exactly once"
    );
}
