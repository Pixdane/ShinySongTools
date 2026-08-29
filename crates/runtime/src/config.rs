//! Typed configuration loading: `DataRoot/shiny-song-tools/scsp.toml`, fail-closed.
//!
//! Missing file: create a fail-closed empty config, then use defaults. Parse
//! error or schema mismatch: defaults with `debug.enabled` forced off. The v1
//! schema defines `[debug]` and `[fps]`; every malformed section falls back
//! to the fully disabled default.

use corelib::DataRoot;
use corelib::{DebugConfig, FpsConfig, RuntimeConfig};
use std::io::Write;

const EMPTY_CONFIG: &str = "# Shiny Song Tools runtime configuration.\n# Empty by default: all optional plugins remain disabled.\n";

/// Load the typed configuration. Never fails: every deviation falls back to
/// the fail-closed default (debug forced off).
#[must_use]
pub fn load_config(data_root: &DataRoot) -> RuntimeConfig {
    let path = data_root.join("shiny-song-tools").join("scsp.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Best-effort create-new: never overwrite a user file and never
            // turn a config convenience into a startup failure. A concurrent
            // creator is harmless; its file will be read on the next start.
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                let _ = file.write_all(EMPTY_CONFIG.as_bytes());
            }
            return RuntimeConfig::default();
        }
        Err(_) => return RuntimeConfig::default(),
    };
    parse_config(&text).unwrap_or_default()
}

/// Parse the `[debug]` and optional `[fps]` sections; anything unexpected is
/// fail-closed (`None`).
fn parse_config(text: &str) -> Option<RuntimeConfig> {
    let value: toml::Value = toml::from_str(text).ok()?;
    let table = value.as_table()?;
    if table.keys().any(|key| key != "debug" && key != "fps") {
        return None;
    }
    let debug = match value.get("debug") {
        None => false,
        Some(section) => {
            let table = section.as_table()?;
            if table.keys().any(|key| key != "enabled") {
                return None;
            }
            table.get("enabled")?.as_bool()?
        }
    };
    let fps = match value.get("fps") {
        None => FpsConfig::default(),
        Some(section) => {
            let table = section.as_table()?;
            if table.keys().any(|key| key != "unlock_fps") {
                return None;
            }
            let unlock_fps = table.get("unlock_fps")?.as_bool()?;
            FpsConfig { unlock_fps }
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
        // Missing scsp.toml is created as an empty, fail-closed config.
        let root = DataRoot::new(
            std::env::temp_dir().join(format!("scsp-config-missing-{}", std::process::id())),
        );
        let path = root.join("shiny-song-tools").join("scsp.toml");
        let _ = std::fs::remove_dir_all(root.path());
        let _ = std::fs::remove_file(&path);
        let config = load_config(&root);
        assert!(!config.debug.enabled);
        assert!(!config.fps.unlock_fps);
        assert_eq!(
            std::fs::read_to_string(path).expect("created config"),
            EMPTY_CONFIG
        );
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
        let config = parse_config("[fps]\nunlock_fps = true\n").expect("valid fps config");
        assert_eq!(config.fps, FpsConfig { unlock_fps: true });
        assert!(parse_config("[fps]\nenabled = true\ntarget = 120\n").is_none());
        assert!(parse_config("[unlock_fps]\nunlock_fps = true\n").is_none());
    }
}
