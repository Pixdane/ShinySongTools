//! Target drift: the hydrated method name diverges from the validated
//! scheduler target and the bootstrap refuses to install.

#[path = "bridge_fake/common/mod.rs"]
mod common;

#[test]
fn target_drift_fails_the_bootstrap_before_hook_install() {
    // SAFETY: single-threaded test process; the fake dylib reads this env at
    // call time and no other thread exists yet.
    unsafe { std::env::set_var("SCSP_FAKE_TARGET_DRIFT", "1") };

    let path = common::fake_dylib();
    let handle = common::fake_handle(&path);
    assert!(
        !shiny_song_tools::bootstrap::run_bootstrap(common::fake_deps(&handle)),
        "target drift must terminate the bootstrap"
    );
    assert!(shiny_song_tools::scheduler::scheduler_context().is_none());
    assert_eq!(common::domain_get_count(&handle), 2);
    assert_eq!(common::detach_count(&handle), 1);
}
