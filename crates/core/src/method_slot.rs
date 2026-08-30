//! MethodPointer slot primitives.
//!
//! The slot is the single pointer field of an IL2CPP `MethodInfo` that the
//! runtime replaces to install a hook. This module owns the CAS / readback /
//! ownership semantics; it deliberately knows nothing about function ABIs,
//! gates, or callbacks. Converting a raw address into a typed function
//! pointer happens only in the target-specific unsafe construction boundary
//! owned by the plugin author (see `plugins`).
//!
//! The physical memory access is abstracted behind [`SlotMemory`]. The
//! production implementation drives the real slot address; fixtures install
//! a mock and drive the same ownership protocol.

use crate::error::HookError;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Install mechanism of one hook target.
///
/// Both mechanisms expose identical CAS + readback + ownership semantics
/// through [`SlotMemory`]; the difference is the physical site:
///
/// * [`HookMechanism::MethodPointerSlot`] swaps the `MethodInfo.methodPointer`
///   word. It only intercepts callers that dispatch through the slot
///   (virtual/interface dispatch, delegates, reflection, Unity lifecycle
///   callbacks).
/// * [`HookMechanism::EntryPatch`] rewrites the function entry itself (inline
///   jump to the replacement). It intercepts every caller, including
///   AOT-compiled direct branches. See [`crate::entry_patch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookMechanism {
    MethodPointerSlot,
    EntryPatch,
}

/// Declared identity and complete managed signature of a hook target.
/// Compared against runtime metadata before the MethodPointer can be written,
/// rejecting name, overload, static/instance, type, or generic drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetId {
    pub assembly: &'static str,
    pub namespace: &'static str,
    pub class: &'static str,
    pub method: &'static str,
    pub param_count: u32,
    /// Whether the managed method is static. Instance methods carry the
    /// implicit `this` argument in their native ABI.
    pub is_static: bool,
    /// Canonical C#-style return type (`void`, `int`, `string`, ...).
    pub return_type: &'static str,
    /// Canonical C#-style explicit parameter types, in declaration order.
    pub parameter_types: &'static [&'static str],
}

impl TargetId {
    #[must_use]
    pub fn matches(&self, actual: &MethodRef) -> bool {
        self.assembly == actual.assembly
            && self.namespace == actual.namespace
            && self.class == actual.class
            && self.method == actual.method
            && self.param_count == actual.param_count
            && self.parameter_types.len() == self.param_count as usize
            && self.is_static == actual.is_static
            && self.return_type == actual.return_type
            && self
                .parameter_types
                .iter()
                .copied()
                .eq(actual.parameter_types.iter().map(String::as_str))
            && !actual.is_generic
            && !actual.is_inflated
    }
}

/// A method resolved through the IL2CPP backend: its runtime identity, the
/// `MethodInfo` address, and the address of its `methodPointer` slot.
///
/// Upper layers must not compute offsets themselves or dereference
/// `MethodInfo` directly; the slot address is only fed into
/// [`SlotMemory`] implementations created by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodRef {
    pub assembly: String,
    pub namespace: String,
    pub class: String,
    pub method: String,
    pub param_count: u32,
    pub is_static: bool,
    pub return_type: String,
    pub parameter_types: Vec<String>,
    pub is_generic: bool,
    pub is_inflated: bool,
    pub method_info: usize,
    pub method_pointer_slot: usize,
}

impl MethodRef {
    /// `true` when the resolved method matches the declared target exactly.
    #[must_use]
    pub fn matches_target(&self, target: &TargetId) -> bool {
        target.matches(self)
    }
}

/// Physical access to one method pointer slot.
///
/// Implementations must perform a real compare-and-swap on the slot word:
/// `Ok(())` means the slot held `expected` and now holds `new`; `Err(actual)`
/// reports the value observed instead of `expected` without writing.
pub trait SlotMemory: Send + Sync + 'static {
    /// Current slot value; `None` if the slot cannot be read.
    fn read(&self) -> Option<usize>;
    fn compare_exchange(&self, expected: usize, new: usize) -> Result<(), usize>;
}

/// Production [`SlotMemory`] over a real (naturally aligned, pointer-sized)
/// memory word: the `methodPointer` field of a live `MethodInfo`.
pub struct RawSlotMemory {
    slot: &'static AtomicUsize,
}

// The slot is a memory word inside live IL2CPP metadata that the game itself
// mutates concurrently; the atomic operations are the access discipline.
unsafe impl Send for RawSlotMemory {}
unsafe impl Sync for RawSlotMemory {}

impl RawSlotMemory {
    /// # Safety
    ///
    /// `addr` must be the address of a live, naturally aligned, pointer-sized
    /// word that remains mapped for the life of the process (an IL2CPP
    /// `MethodInfo.methodPointer` field produced by the exact-handle backend).
    ///
    /// # Panics
    ///
    /// Panics when `addr` is null or not `usize`-aligned: both are reviewed
    /// backend contract violations that must surface at construction, long
    /// before any hook install.
    #[must_use]
    pub unsafe fn from_addr(addr: usize) -> Self {
        assert!(
            addr != 0 && addr.is_multiple_of(core::mem::align_of::<usize>()),
            "method pointer slot address must be naturally aligned"
        );
        Self {
            // SAFETY: caller of `from_addr` guarantees the word is live and
            // aligned for the process lifetime; the unbounded lifetime of
            // `from_ptr` is pinned to 'static to encode that contract.
            slot: unsafe { AtomicUsize::from_ptr(addr as *mut usize) },
        }
    }
}

impl SlotMemory for RawSlotMemory {
    fn read(&self) -> Option<usize> {
        // SAFETY: caller of `from_addr` guarantees the word is live and
        // aligned; a normal load cannot fault for a mapped aligned word.
        Some(self.slot.load(Ordering::Acquire))
    }

    fn compare_exchange(&self, expected: usize, new: usize) -> Result<(), usize> {
        // SAFETY: same liveness/alignment contract as `read`.
        match self
            .slot
            .compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(()),
            Err(actual) => Err(actual),
        }
    }
}

/// Bound, ownership-aware wrapper around one method pointer slot.
///
/// The slot is the final source of truth for ownership; the `installed` flag
/// maintained by the typed hook layer only rejects duplicate install/restore
/// attempts and decides whether a restore should be attempted.
pub struct MethodPointerSlot {
    memory: Arc<dyn SlotMemory>,
    original: usize,
}

impl core::fmt::Debug for MethodPointerSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MethodPointerSlot")
            .field("bound", &true)
            .field("original_set", &(self.original != 0))
            .finish()
    }
}

impl MethodPointerSlot {
    /// Bind to a slot: read the non-null current pointer (the original). No
    /// write happens here. The slot *address* alignment is the backend's
    /// contract ([`RawSlotMemory::from_addr`]); the stored value is a code
    /// pointer (4-byte aligned on arm64) and is only required to be
    /// readable and non-null.
    pub fn bind(memory: Arc<dyn SlotMemory>) -> Result<Self, HookError> {
        let current = memory.read().ok_or(HookError::SlotMalformed)?;
        if current == 0 {
            return Err(HookError::SlotMalformed);
        }
        Ok(Self {
            memory,
            original: current,
        })
    }

    /// The original (pre-install) pointer read at bind time.
    #[must_use]
    pub fn original(&self) -> usize {
        self.original
    }

    /// CAS `original -> replacement`, then read back and require the
    /// replacement to be visible.
    ///
    /// * `Err(SlotConflict)`: the slot no longer held the original (another
    ///   owner or an unknown value); nothing was written.
    /// * `Err(InstallationFailed)`: the CAS succeeded but the readback did
    ///   not confirm the replacement. The caller must immediately attempt
    ///   one ownership-aware rollback via [`MethodPointerSlot::restore`];
    ///   only a confirmed rollback clears the installed flag.
    pub fn install(&self, replacement: usize) -> Result<(), HookError> {
        if self
            .memory
            .compare_exchange(self.original, replacement)
            .is_err()
        {
            return Err(HookError::SlotConflict);
        }
        if self.memory.read() == Some(replacement) {
            Ok(())
        } else {
            Err(HookError::InstallationFailed)
        }
    }

    /// CAS `replacement -> original`, then read back and require the
    /// original to be visible. Ownership-aware: if the slot no longer holds
    /// the replacement, nothing is written and drift is reported.
    pub fn restore(&self, replacement: usize) -> Result<(), HookError> {
        if self
            .memory
            .compare_exchange(replacement, self.original)
            .is_err()
        {
            return Err(HookError::OwnershipDrift);
        }
        if self.memory.read() == Some(self.original) {
            Ok(())
        } else {
            Err(HookError::InstallationFailed)
        }
    }

    /// Current slot value, for diagnostics and quiescence checks.
    #[must_use]
    pub fn current(&self) -> Option<usize> {
        self.memory.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRING_LOOKUP: TargetId = TargetId {
        assembly: "Game.dll",
        namespace: "Game",
        class: "Localizer",
        method: "GetText",
        param_count: 2,
        is_static: false,
        return_type: "string",
        parameter_types: &["string", "int"],
    };

    fn string_lookup_ref() -> MethodRef {
        MethodRef {
            assembly: "Game.dll".to_owned(),
            namespace: "Game".to_owned(),
            class: "Localizer".to_owned(),
            method: "GetText".to_owned(),
            param_count: 2,
            is_static: false,
            return_type: "string".to_owned(),
            parameter_types: vec!["string".to_owned(), "int".to_owned()],
            is_generic: false,
            is_inflated: false,
            method_info: 0x1000,
            method_pointer_slot: 0x1000,
        }
    }

    #[test]
    fn target_identity_includes_the_complete_managed_signature() {
        let exact = string_lookup_ref();
        assert!(exact.matches_target(&STRING_LOOKUP));

        let mut wrong_return = exact.clone();
        wrong_return.return_type = "object".to_owned();
        assert!(!wrong_return.matches_target(&STRING_LOOKUP));

        let mut wrong_static = exact.clone();
        wrong_static.is_static = true;
        assert!(!wrong_static.matches_target(&STRING_LOOKUP));

        let mut wrong_parameters = exact.clone();
        wrong_parameters.parameter_types.swap(0, 1);
        assert!(!wrong_parameters.matches_target(&STRING_LOOKUP));

        let mut generic = exact;
        generic.is_generic = true;
        assert!(!generic.matches_target(&STRING_LOOKUP));
    }

    /// In-memory slot used by core-level unit tests.
    #[derive(Default)]
    struct MockSlot(AtomicUsize);

    impl SlotMemory for MockSlot {
        fn read(&self) -> Option<usize> {
            Some(self.0.load(Ordering::Acquire))
        }
        fn compare_exchange(&self, expected: usize, new: usize) -> Result<(), usize> {
            self.0
                .compare_exchange(expected, new, Ordering::AcqRel, Ordering::Acquire)
                .map(drop)
        }
    }

    #[test]
    fn cas_install_readback_confirms_replacement() {
        let slot =
            MethodPointerSlot::bind(Arc::new(MockSlot(AtomicUsize::new(0x1000)))).expect("bind");
        let replacement = 0xfeed_face_usize;
        slot.install(replacement).expect("install");
        assert_eq!(slot.current(), Some(replacement));
    }

    #[test]
    fn install_conflict_reports_slot_conflict_without_write() {
        let mock = Arc::new(MockSlot(AtomicUsize::new(0x1000)));
        let slot = MethodPointerSlot::bind(mock.clone()).expect("bind");
        // Third party writes an unknown value first.
        mock.0.store(0x999, Ordering::Release);
        assert!(matches!(slot.install(0xabc), Err(HookError::SlotConflict)));
        assert_eq!(slot.current(), Some(0x999), "conflict must not write");
    }

    #[test]
    fn restore_is_ownership_aware() {
        let slot =
            MethodPointerSlot::bind(Arc::new(MockSlot(AtomicUsize::new(0x1000)))).expect("bind");
        let original = slot.original();
        slot.install(0xabc).expect("install");
        slot.restore(0xabc).expect("restore");
        assert_eq!(slot.current(), Some(original));

        // Restoring again without reinstalling reports drift (slot no longer
        // holds the replacement) and does not write.
        assert!(matches!(
            slot.restore(0xabc),
            Err(HookError::OwnershipDrift)
        ));
    }

    #[test]
    fn bind_rejects_null_slots_but_accepts_word_unaligned_code_pointers() {
        let null = Arc::new(MockSlot(AtomicUsize::new(0)));
        assert!(matches!(
            MethodPointerSlot::bind(null),
            Err(HookError::SlotMalformed)
        ));
        // arm64 code pointers are only 4-byte aligned; the stored value is
        // not required to satisfy the slot word's alignment.
        let code_ptr = Arc::new(MockSlot(AtomicUsize::new(0x102465e64)));
        assert!(MethodPointerSlot::bind(code_ptr).is_ok());
    }
}
