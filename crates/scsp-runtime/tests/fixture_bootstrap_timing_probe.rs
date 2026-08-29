//! No-game fixture for the diagnostic startup timing probe.
//!
//! The probe must perform exactly the intended one-variable experiment:
//! image/export checks, a bounded delay, and one `domain_get` call. It must
//! stop before attach, cache hydration, target resolution, App publication,
//! or hook installation.

#![cfg(feature = "bootstrap-timing-probe")]

mod common;

use common::{MockIl2Cpp, MockReadiness, MockResolver};
use scsp_core::DataRoot;
use shiny_song_tools::bootstrap::{BootstrapDeps, run_bootstrap_timing_probe_with_delay};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn deps(api: Arc<MockIl2Cpp>) -> BootstrapDeps {
    BootstrapDeps {
        api,
        readiness: Arc::new(MockReadiness::new()),
        resolver: Arc::new(MockResolver::new()),
        data_root: DataRoot::new(std::env::temp_dir().join("scsp-bootstrap-timing-probe")),
        config: scsp_plugin_api::RuntimeConfig::default(),
        thread_check: Arc::new(|| true),
    }
}

#[test]
fn probe_calls_domain_once_and_stops_before_attach() {
    let api = Arc::new(MockIl2Cpp::new());

    assert!(run_bootstrap_timing_probe_with_delay(
        deps(api.clone()),
        Duration::ZERO,
    ));
    assert_eq!(api.domain_get_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        api.rung.load(Ordering::Acquire),
        3,
        "the diagnostic probe must stop before attach"
    );
    assert!(shiny_song_tools::scheduler::scheduler_context().is_none());
}

#[test]
fn null_domain_terminates_after_one_call() {
    let api = Arc::new(MockIl2Cpp {
        null_domain: true,
        ..MockIl2Cpp::new()
    });

    assert!(!run_bootstrap_timing_probe_with_delay(
        deps(api.clone()),
        Duration::ZERO,
    ));
    assert_eq!(api.domain_get_calls.load(Ordering::Acquire), 1);
    assert_eq!(api.rung.load(Ordering::Acquire), 2);
}
