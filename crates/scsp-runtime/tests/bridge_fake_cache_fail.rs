//! Cache hydration failure: the one-shot bootstrap terminates fail-closed
//! after the ladder-4 hydration error.

#[path = "bridge_fake/common/mod.rs"]
mod common;

#[test]
fn cache_failure_terminates_the_one_shot_bootstrap() {
    // SAFETY: single-threaded test process; the fake dylib reads this env at
    // call time and no other thread exists yet.
    unsafe { std::env::set_var("SCSP_FAKE_CACHE_FAIL", "1") };

    let path = common::fake_dylib();
    let handle = common::fake_handle(&path);
    assert!(
        !shiny_song_tools::bootstrap::run_bootstrap(common::fake_deps(&handle)),
        "cache failure must terminate the bootstrap"
    );
    assert!(shiny_song_tools::scheduler::scheduler_context().is_none());

    // The ladder-4 probe ran once; the cache-internal re-read also ran before the
    // (failing) assembly enumeration.
    assert_eq!(common::domain_get_count(&handle), 2);
    assert_eq!(
        common::detach_count(&handle),
        1,
        "the worker's own attach is undone"
    );
}
