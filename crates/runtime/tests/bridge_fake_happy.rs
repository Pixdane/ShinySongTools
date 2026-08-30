//! Happy path: the full bootstrap ladder over the fake UnityFramework,
//! target resolution, scheduler hook install, and App publication.

#[path = "bridge_fake/common/mod.rs"]
mod common;

#[test]
fn full_ladder_publishes_and_uses_the_validated_domain_pattern() {
    let path = common::fake_dylib();
    let handle = common::fake_handle(&path);
    assert!(
        shiny_song_tools::bootstrap::run_bootstrap(common::fake_deps(&handle)),
        "bootstrap must publish over the fake runtime"
    );

    // The scheduler hook is installed against the fake slot.
    assert!(shiny_song_tools::scheduler::scheduler_context().is_some());

    assert_eq!(
        common::criware_ready_count(&handle),
        1,
        "production readiness resolved and called the CRIWARE export"
    );

    // Real il2cpp_domain_get calls: exactly ONE bootstrap probe (ladder 4)
    // plus ONE inside the bridge crate's cache hydration — the experiment
    // rule forbids pre-init POLLING, not post-gate internal re-reads (see
    // runtime crate Rustdoc readiness invariant).
    assert_eq!(
        common::domain_get_count(&handle),
        2,
        "one ladder-4 probe + one cache-internal re-read"
    );

    // The worker's own attachment is detached exactly once (RAII guard).
    assert_eq!(common::detach_count(&handle), 1);
}
