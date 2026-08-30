//! Runtime configuration as seen by plugins.
//!
//! The single configuration source is `DataRoot/shiny-song-tools/scsp.toml`; the runtime
//! parses it (fail-closed for syntax and known-field errors; unknown fields
//! are ignored) and hands the typed result to `App::new`. Plugins
//! only read the typed snapshot through `AppCtx`.

/// Typed runtime configuration snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub debug: DebugConfig,
    pub fps: FpsConfig,
    pub translation: TranslationConfig,
    pub recon: ReconConfig,
}

/// `[debug]` section. `enabled = true` registers the DebugPlugin at the head
/// of the production plugin list; the default is off and the fail-closed
/// fallback forces it off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DebugConfig {
    pub enabled: bool,
}

/// `[fps]` section. The plugin is disabled by default; when enabled, setter
/// hooks force the configured target frame rate and disable v-sync.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FpsConfig {
    pub unlock_fps: bool,
}

/// `[translation]` section. Dump mode is an explicit development switch and
/// remains disabled in both the default and every fail-closed fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranslationConfig {
    pub dump: bool,
}

/// `[recon]` section. Development-only runtime reconnaissance plugin;
/// disabled by default and in every fail-closed fallback.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconConfig {
    pub enabled: bool,
}
