//! Runtime configuration as seen by plugins.
//!
//! The single configuration source is `DataRoot/scsp.toml`; the runtime
//! parses it (fail-closed) and hands the typed result to `App::new`. Plugins
//! only read the typed snapshot through `AppCtx`.

/// Typed runtime configuration snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub debug: DebugConfig,
}

/// `[debug]` section. `enabled = true` registers the DebugPlugin at the head
/// of the production plugin list; the default is off and the fail-closed
/// fallback forces it off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugConfig {
    pub enabled: bool,
}
