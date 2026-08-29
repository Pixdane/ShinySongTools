//! Typed configuration loading: `DataRoot/scsp.toml`, fail-closed.
//!
//! Missing file: defaults. Parse error or schema mismatch: defaults with
//! `debug.enabled` forced off. The v1 schema only defines `[debug] enabled`;
//! plugin-private sections are read by the plugins themselves in later
//! phases through the same typed mechanism.

use scsp_core::DataRoot;
use scsp_plugin_api::{DebugConfig, RuntimeConfig};

/// Load the typed configuration. Never fails: every deviation falls back to
/// the fail-closed default (debug forced off).
#[must_use]
pub fn load_config(data_root: &DataRoot) -> RuntimeConfig {
    let path = data_root.join("scsp.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => return RuntimeConfig::default(),
    };
    parse_config(&text).unwrap_or_default()
}

/// Parse one `[debug] enabled = <bool>` section; anything unexpected is
/// fail-closed (`None`).
fn parse_config(text: &str) -> Option<RuntimeConfig> {
    let value: toml::Value = toml::from_str(text).ok()?;
    let enabled = value.get("debug")?.get("enabled")?.as_bool()?;
    Some(RuntimeConfig {
        debug: DebugConfig { enabled },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_invalid_falls_back_to_fail_closed() {
        // load_config with a data root that has no scsp.toml: default.
        let root = DataRoot::new(std::env::temp_dir().join("scsp-config-missing"));
        let config = load_config(&root);
        assert!(!config.debug.enabled);
    }

    #[test]
    fn parses_debug_enabled() {
        let config = parse_config("[debug]\nenabled = true\n").expect("valid config");
        assert!(config.debug.enabled);
        let config = parse_config("[debug]\nenabled = false\n").expect("valid config");
        assert!(!config.debug.enabled);
        // Fail-closed on schema mismatch.
        assert!(parse_config("[debug]\nenabled = \"yes\"\n").is_none());
        assert!(parse_config("not toml at all [").is_none());
    }
}
