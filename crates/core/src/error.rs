//! The single error chain visible to plugin authors.
//!
//! Plugin code only ever sees [`PluginError`] / [`HookError`] /
//! [`Il2CppError`] / [`RestoreError`]. Concrete failure context travels
//! through observability events, never through persistent error state.

use thiserror::Error;

/// Top-level error returned by plugin `build`, phase systems, and facade
/// methods. Derived sub-errors keep the chain closed: plugin authors never
/// handle a foreign error type.
#[derive(Debug, Error)]
pub enum PluginError {
    #[error("resource conflict: {0}")]
    ResourceConflict(&'static str),
    #[error("missing dependency: {0}")]
    MissingDependency(&'static str),
    #[error("hook: {0}")]
    Hook(#[from] HookError),
    #[error("il2cpp: {0}")]
    Il2Cpp(#[from] Il2CppError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(&'static str),
}

/// Hook installation / slot ownership failures. CAS, readback, and concrete
/// addresses are deliberately not exposed here; slot ownership drift is
/// reported as a single variant and diagnosed through observability.
#[derive(Debug, Error)]
pub enum HookError {
    #[error("hook target unavailable")]
    TargetUnavailable,
    #[error("hook target signature mismatch")]
    SignatureMismatch,
    #[error("hook site already registered")]
    SiteAlreadyRegistered,
    #[error("method pointer slot conflict: slot is owned by another party")]
    SlotConflict,
    #[error("hook installation succeeded but readback did not confirm the replacement")]
    InstallationFailed,
    #[error("method pointer slot ownership drift detected")]
    OwnershipDrift,
    #[error("method pointer slot is malformed (unreadable, null, or misaligned)")]
    SlotMalformed,
    #[error("entry patch unsupported at this site: {0}")]
    EntryPatchUnsupported(&'static str),
}

/// Failures of a single restore action. This is the return value of one
/// rollback step, not a persistent effect state machine.
#[derive(Debug, Error)]
pub enum RestoreError {
    #[error("ownership of the restored state was lost")]
    OwnershipLost,
    #[error("restore action failed")]
    Failed,
}

/// IL2CPP platform errors surfaced by the backend abstraction.
#[derive(Debug, Error)]
pub enum Il2CppError {
    #[error("UnityFramework image not found before the readiness deadline")]
    ImageNotFound,
    #[error("UnityFramework image identity mismatch")]
    ImageIdentityMismatch,
    #[error("required IL2CPP export missing: {0}")]
    ExportMissing(&'static str),
    #[error("required bootstrap readiness symbol missing: {0}")]
    ReadinessSymbolMissing(&'static str),
    #[error("bootstrap readiness deadline exceeded")]
    ReadinessDeadlineExceeded,
    #[error("il2cpp class not found: {0}")]
    ClassNotFound(String),
    #[error("il2cpp_domain_get returned null; one-shot bootstrap terminated")]
    DomainUnavailable,
    #[error("thread attach to the IL2CPP domain failed")]
    AttachFailed,
    #[error("IL2CPP runtime/layout identity mismatch")]
    IdentityMismatch,
    #[error("IL2CPP method resolution failed: {0}")]
    MethodResolutionFailed(&'static str),
    #[error("IL2CPP API used before readiness")]
    NotReady,
}
