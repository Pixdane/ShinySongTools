//! ABI drift: the hydrated scheduler method changes from instance to static
//! while its name and parameter count stay unchanged. Complete signature
//! validation must reject it before the scheduler MethodPointer is written.

#[path = "bridge_fake/common/mod.rs"]
mod common;

#[test]
fn signature_drift_fails_before_hook_install() {
    // SAFETY: each integration test is its own process; the fake dylib reads
    // this environment variable during the one-shot bootstrap.
    unsafe { std::env::set_var("SCSP_FAKE_SIGNATURE_DRIFT", "1") };

    let path = common::fake_dylib();
    let handle = common::fake_handle(&path);
    assert!(
        !shiny_song_tools::bootstrap::run_bootstrap(common::fake_deps(&handle)),
        "static/instance drift must terminate the bootstrap"
    );
    assert!(shiny_song_tools::scheduler::scheduler_context().is_none());
    assert_eq!(common::domain_get_count(&handle), 2);
    assert_eq!(common::detach_count(&handle), 1);
}
