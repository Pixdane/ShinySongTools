//! 无游戏 fixture — bootstrap readiness 阶梯与一次性语义：
//! 1. 成功路径：`domain_get` 恰好调用 1 次；hook 已 CAS 安装；Handoff 已
//!    publish；RuntimeGate 仍关闭（由首帧 Startup 结束后最后开启）；
//! 2. domain 返回 null：一次性 bootstrap 终止，不轮询不重试；
//! 3. 阶梯顺序被 mock 强制（越级 → NotReady）；
//! 4. 阶梯 5 身份不匹配 / attach 失败 → 终止；
//! 5. scheduler 目标缺失 → 整个 bootstrap 失败（不是插件退役）。
//!
//! 注意：bootstrap 的进程级静态 context 只能发布一次（OnceLock），因此本
//! 文件内只有一个测试走到发布；其余测试全部在发布之前失败。
//! 对应 docs/runtime-crate.md「readiness 阶梯」「启动失败与保活」与验证顺序
//! §2.12 第 4 条。

mod common;

use common::{MockIl2Cpp, MockResolver};
use scsp_core::{DataRoot, Il2CppApi, Il2CppError};
use shiny_song_tools::bootstrap::{BootstrapDeps, SCHEDULER_TARGET, run_bootstrap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

fn deps(api: Arc<MockIl2Cpp>, resolver: Arc<MockResolver>) -> BootstrapDeps {
    BootstrapDeps {
        api,
        resolver,
        data_root: DataRoot::new(std::env::temp_dir().join("scsp-fixture-bootstrap")),
        config: scsp_plugin_api::RuntimeConfig::default(),
        thread_check: Arc::new(|| true), // fixture thread stands in for main
    }
}

fn resolver_with_target() -> (Arc<MockResolver>, Arc<AtomicUsize>) {
    let resolver = Arc::new(MockResolver::new());
    let slot = resolver.register(&SCHEDULER_TARGET);
    (resolver, slot)
}

static RESOLVER: OnceLock<Arc<MockResolver>> = OnceLock::new();
static SLOT: OnceLock<Arc<AtomicUsize>> = OnceLock::new();

#[test]
fn bootstrap_success_calls_domain_get_exactly_once_and_publishes() {
    let (resolver, slot) = resolver_with_target();
    RESOLVER
        .set(Arc::clone(&resolver))
        .expect("once per process");
    SLOT.set(Arc::clone(&slot)).expect("once per process");
    let api = Arc::new(MockIl2Cpp::new());

    assert!(
        run_bootstrap(deps(api.clone(), resolver)),
        "bootstrap published"
    );

    assert_eq!(
        api.domain_get_calls.load(Ordering::Acquire),
        1,
        "domain_get called exactly once (experiment decision)"
    );
    // Hook CAS-installed: the slot holds the replacement, not the original.
    let original_addr = common::mock_lateupdate as *const () as usize;
    let slot_value = slot.load(Ordering::Acquire);
    assert_ne!(slot_value, 0);
    assert_ne!(slot_value, original_addr, "replacement installed");

    // Published context: handoff holds the App; the gate is still closed
    // (the first Startup frame opens it last); not failed.
    let ctx = shiny_song_tools::scheduler::scheduler_context().expect("published");
    assert!(matches!(
        ctx.handoff.try_take(),
        shiny_song_tools::HandoffTake::Ready(_)
    ));
    assert!(!ctx.runtime_gate.reader().is_open());
    assert!(!ctx.failed.load(Ordering::Acquire));
}

#[test]
fn bootstrap_null_domain_terminates_without_retry() {
    let api = Arc::new(MockIl2Cpp {
        null_domain: true,
        ..MockIl2Cpp::new()
    });
    let (resolver, _slot) = resolver_with_target();
    assert!(!run_bootstrap(deps(api.clone(), resolver)));
    assert_eq!(
        api.domain_get_calls.load(Ordering::Acquire),
        1,
        "null domain terminates: exactly one call, no polling"
    );
}

#[test]
fn bootstrap_ladder_order_is_enforced() {
    // The mock refuses calls beyond the reached rung: the ladder's order is
    // load-bearing and every bootstrap failure path relies on it.
    let api = MockIl2Cpp::new();
    assert!(matches!(api.domain_get(), Err(Il2CppError::NotReady)));
    assert!(api.load_exports().is_ok());
    assert!(api.domain_get().is_ok());
    assert_eq!(api.domain_get_calls.load(Ordering::Acquire), 1);
    assert!(matches!(api.runtime_identity(), Err(Il2CppError::NotReady)));
}

#[test]
fn bootstrap_identity_mismatch_terminates() {
    let api = Arc::new(MockIl2Cpp {
        identity_mismatch: true,
        ..MockIl2Cpp::new()
    });
    let (resolver, _slot) = resolver_with_target();
    assert!(!run_bootstrap(deps(api, resolver)));
}

#[test]
fn bootstrap_attach_failure_terminates() {
    let api = Arc::new(MockIl2Cpp {
        attach_fails: true,
        ..MockIl2Cpp::new()
    });
    let (resolver, _slot) = resolver_with_target();
    assert!(!run_bootstrap(deps(api, resolver)));
}

#[test]
fn bootstrap_scheduler_target_missing_fails_everything() {
    // No target registered in the resolver: resolution fails and the whole
    // bootstrap fails (scheduler targets are not plugin-scoped).
    let api = Arc::new(MockIl2Cpp::new());
    let resolver = Arc::new(MockResolver::new()); // empty
    assert!(!run_bootstrap(deps(api, resolver)));
}
