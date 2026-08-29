//! Production readiness gate: a real exact-handle CRIWARE predicate that
//! remains false must terminate before the first IL2CPP domain probe.

#[path = "bridge_fake/common/mod.rs"]
mod common;

#[test]
fn criware_not_ready_fails_closed_before_domain_get() {
    // SAFETY: this integration test is single-threaded and the fake reads the
    // environment at predicate-call time.
    unsafe { std::env::set_var("SCSP_FAKE_CRIWARE_NOT_READY", "1") };

    let path = common::fake_dylib();
    let handle = common::fake_handle(&path);
    assert!(
        !shiny_song_tools::bootstrap::run_bootstrap_with_readiness_wait(
            common::fake_deps(&handle),
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        ),
        "CRIWARE false must terminate the one-shot bootstrap"
    );
    assert_eq!(common::criware_ready_count(&handle), 1);
    assert_eq!(common::domain_get_count(&handle), 0);
}
