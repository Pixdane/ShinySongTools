//! Attach failure: ladder 5 rejects a null thread attach and terminates.

#[path = "bridge_fake/common/mod.rs"]
mod common;

#[test]
fn attach_failure_terminates_before_hydration() {
    // SAFETY: single-threaded test process; the fake dylib reads this env at
    // call time and no other thread exists yet.
    unsafe { std::env::set_var("SCSP_FAKE_ATTACH_FAIL", "1") };

    let path = common::fake_dylib();
    let handle = common::fake_handle(&path);
    assert!(
        !shiny_song_tools::bootstrap::run_bootstrap(common::fake_deps(&handle)),
        "attach failure must terminate the bootstrap"
    );
    assert!(shiny_song_tools::scheduler::scheduler_context().is_none());

    // Ladder 4 probe only: attach failed before hydration could re-read.
    assert_eq!(common::domain_get_count(&handle), 1);
    // Nothing was attached, so nothing may be detached.
    assert_eq!(common::detach_count(&handle), 0);
}
