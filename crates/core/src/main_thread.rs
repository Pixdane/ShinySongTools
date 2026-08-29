//! The main-thread capability token.
//!
//! `MainThreadToken` proves "the caller has verified the current thread is
//! the process main thread" at the type level. It is `!Send + !Sync`, has no
//! public constructor, and is not `Clone`/`Copy`, so safe plugin code cannot
//! manufacture or cache it. The runtime constructs one token per scheduler
//! frame (after re-verifying the platform predicate) and only passes short
//! borrows into phase contexts; the borrow lifetime is the capability
//! boundary of a single system call.
//!
//! The `!Send`/`!Sync` property is guarded by a compile-fail fixture
//! (`tests/ui` in the workspace root crate) rather than an in-type trick,
//! because stable Rust has no negative-bound assertion.

use core::marker::PhantomData;
use std::rc::Rc;

/// Capability for executing Unity main-thread operations.
///
/// The `Rc` phantom makes the type `!Send + !Sync`; the private unit field
/// prevents construction outside this module's unsafe boundary.
pub struct MainThreadToken {
    _not_send_sync: PhantomData<Rc<()>>,
    _private: (),
}

impl core::fmt::Debug for MainThreadToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("MainThreadToken")
    }
}

impl MainThreadToken {
    /// Construct a capability token for the current thread.
    ///
    /// # Safety
    ///
    /// The caller must have verified, in the same call frame, that the
    /// current thread is the process main thread (`pthread_main_np() != 0`
    /// in the v1 platform predicate). Swift main-queue scheduling, worker
    /// thread identity, or "has run on the right thread before" does *not*
    /// satisfy this contract. The runtime calls this only inside its
    /// reviewed scheduler frame construction; any other caller is outside
    /// the design's trust boundary.
    #[must_use]
    pub unsafe fn assume_main_thread() -> Self {
        Self {
            _not_send_sync: PhantomData,
            _private: (),
        }
    }
}
