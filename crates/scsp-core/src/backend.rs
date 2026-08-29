//! IL2CPP backend abstraction.
//!
//! The production backend wraps the exact UnityFramework handle and the
//! IL2CPP API table loaded through the pinned bridge crate. Everything the
//! runtime and the hook layer need is expressed through the [`Il2CppApi`] and
//! [`MethodResolver`] traits so that no-game fixtures can drive the same
//! protocol with mocks.
//!
//! Readiness ladder ownership (see the runtime bootstrap document):
//! ladder 1 image identity is polled by the caller; ladders 2-5 are
//! single-shot and fail closed. In particular `domain_get` is called exactly
//! once across the whole bootstrap.

use crate::error::{HookError, Il2CppError};
use crate::method_slot::{MethodRef, SlotMemory, TargetId};
use std::sync::Arc;

/// Opaque handle to the exact UnityFramework image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageHandle(pub usize);

/// Identity facts of the resolved UnityFramework image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageIdentity {
    pub name: String,
    pub handle: ImageHandle,
}

/// Opaque domain pointer returned by `il2cpp_domain_get`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DomainHandle(pub usize);

/// Runtime/layout identity facts validated at ladder 5.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub unity_version: String,
    pub il2cpp_variant: String,
}

/// RAII guard for a thread attachment established by this process. Dropping
/// detaches only the attachment this guard created; attachments made by
/// external code are never touched.
pub struct AttachGuard {
    detach: Option<Box<dyn FnOnce() + Send>>,
}

impl AttachGuard {
    pub fn new(detach: impl FnOnce() + Send + 'static) -> Self {
        Self {
            detach: Some(Box::new(detach)),
        }
    }

    /// Give up the guard without detaching (used when the attachment is
    /// intentionally handed over to a longer-lived owner).
    pub fn leak(mut self) {
        self.detach = None;
    }
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        if let Some(detach) = self.detach.take() {
            detach();
        }
    }
}

impl core::fmt::Debug for AttachGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AttachGuard")
            .field("armed", &self.detach.is_some())
            .finish()
    }
}

/// The IL2CPP operations the runtime bootstrap and scheduler need.
///
/// Implementations must uphold the readiness ladder: operations beyond the
/// reached rung return [`Il2CppError::NotReady`].
pub trait Il2CppApi: Send + Sync + 'static {
    /// Ladder 1 (caller may poll): resolve the unique UnityFramework image.
    fn unity_framework_image(&self) -> Result<ImageIdentity, Il2CppError>;
    /// Ladder 2 (single shot): load all required exports from the exact handle.
    fn load_exports(&self) -> Result<(), Il2CppError>;
    /// Ladder 3 (exactly once per process): fetch the il2cpp domain.
    fn domain_get(&self) -> Result<DomainHandle, Il2CppError>;
    /// Ladder 4: attach the current thread; the guard detaches only this
    /// attachment.
    fn attach_current_thread(&self) -> Result<AttachGuard, Il2CppError>;
    /// Ladder 4 (after attach): hydrate the metadata cache for target
    /// resolution. Expensive; runs on the bootstrap worker.
    fn hydrate_metadata(&self) -> Result<(), Il2CppError>;
    /// Ladder 5 (single shot): validate the supported runtime/layout identity.
    fn runtime_identity(&self) -> Result<RuntimeIdentity, Il2CppError>;
}

/// Method resolution for hook targets, exposed to the hook typestate layer.
pub trait MethodResolver: Send + Sync {
    fn resolve(&self, target: &TargetId) -> Result<MethodRef, HookError>;
    fn slot_memory(&self, method: &MethodRef) -> Arc<dyn SlotMemory>;
}

/// Combined capability handed to hook installation.
pub struct ResolvedMethod {
    pub method: MethodRef,
    pub slot_memory: Arc<dyn SlotMemory>,
}

impl ResolvedMethod {
    pub fn new(method: MethodRef, slot_memory: Arc<dyn SlotMemory>) -> Self {
        Self {
            method,
            slot_memory,
        }
    }
}
