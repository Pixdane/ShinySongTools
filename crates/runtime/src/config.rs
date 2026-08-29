//! Typed configuration loading: `DataRoot/scsp.toml`, fail-closed.
//!
//! Missing file: defaults. Parse error or schema mismatch: defaults with
//! `debug.enabled` forced off. The v1 schema defines `[debug]` and `[fps]`;
//! every malformed section falls back to the fully disabled default.

use corelib::DataRoot;
use plugins::{DebugConfig, FpsConfig, RuntimeConfig};

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

/// Parse the `[debug]` and optional `[fps]` sections; anything unexpected is
/// fail-closed (`None`).
fn parse_config(text: &str) -> Option<RuntimeConfig> {
    let value: toml::Value = toml::from_str(text).ok()?;
    let debug = match value.get("debug") {
        None => false,
        Some(section) => section.get("enabled")?.as_bool()?,
    };
    let fps = match value.get("fps") {
        None => FpsConfig::default(),
        Some(section) => {
            let enabled = section.get("enabled")?.as_bool()?;
            let target = i32::try_from(section.get("target")?.as_integer()?).ok()?;
            if !(1..=1000).contains(&target) {
                return None;
            }
            FpsConfig { enabled, target }
        }
    };
    Some(RuntimeConfig {
        debug: DebugConfig { enabled: debug },
        fps,
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
        assert_eq!(config.fps, FpsConfig::default());
        // Fail-closed on schema mismatch.
        assert!(parse_config("[debug]\nenabled = \"yes\"\n").is_none());
        assert!(parse_config("not toml at all [").is_none());
        let config =
            parse_config("[fps]\nenabled = true\ntarget = 144\n").expect("valid fps config");
        assert_eq!(
            config.fps,
            FpsConfig {
                enabled: true,
                target: 144
            }
        );
        assert!(parse_config("[fps]\nenabled = true\ntarget = 0\n").is_none());
    }
}
