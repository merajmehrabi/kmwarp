//! `~/.config/kmwarp/config.toml` (mac/linux) and
//! `%APPDATA%\kmwarp\config.toml` (Windows) schema + loader.
//!
//! Schema mirrors PLAN.md §Cross-cutting design → Config exactly:
//!
//! ```toml
//! [peer]
//! bind = "0.0.0.0:51423"       # server only
//! connect = "10.0.0.5:51423"   # client only
//! name = "merajs-mbp"
//!
//! [edge]
//! side = "right"               # right|left|top|bottom; v1 hardcodes right
//! remote_screen_px = [2560, 1440]
//!
//! [modifiers]
//! cmd = "ctrl"
//! option = "alt"
//!
//! [tls]
//! pin_file = "~/.config/kmwarp/peer.pin"
//! ```
//!
//! Each section is optional; missing sections / fields fall back to
//! the spec defaults so a partial TOML still parses. `Config::default()`
//! is the runtime fallback when the on-disk file is missing entirely.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::ConfigError;
use crate::modmap::ModRemap;

/// Top-level config struct. Every section is `#[serde(default)]` so
/// a partial config (e.g. just `[modifiers]`) parses cleanly.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub peer: PeerConfig,
    pub edge: EdgeSection,
    pub modifiers: ModRemap,
    pub tls: TlsConfig,
}

impl Config {
    /// Parse a config from a TOML string.
    pub fn parse(toml_str: &str) -> Result<Self, ConfigError> {
        toml::from_str(toml_str).map_err(ConfigError::Parse)
    }

    /// Load from an explicit path. Errors on IO failure or TOML parse
    /// failure; missing-file is the caller's concern.
    pub fn load_from_path(path: &Path) -> Result<Self, ConfigError> {
        let s = std::fs::read_to_string(path)?;
        Self::parse(&s)
    }

    /// Load from the OS-conventional config path:
    /// - macOS: `~/.config/kmwarp/config.toml` (Linux XDG convention,
    ///   per spec — NOT `~/Library/Application Support`)
    /// - Linux: `$XDG_CONFIG_HOME/kmwarp/config.toml` via the
    ///   `directories` crate
    /// - Windows: `%APPDATA%\kmwarp\config.toml`
    ///
    /// Returns `Ok(Config::default())` if the file doesn't exist —
    /// kmwarp is meant to run with sensible defaults out of the box.
    /// Returns `Err(ConfigError::MissingDir)` if the platform doesn't
    /// expose a home directory at all (rare, e.g. sandboxed contexts
    /// without a `$HOME`).
    pub fn load_default() -> Result<Self, ConfigError> {
        let path = Self::default_config_path().ok_or(ConfigError::MissingDir)?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Self::load_from_path(&path)
    }

    /// The OS-conventional config path for this platform. `None` if
    /// the user's home directory cannot be resolved.
    pub fn default_config_path() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            // Per spec: `~/.config/kmwarp/config.toml` on macOS to match
            // the Linux convention. `directories::ProjectDirs` would
            // route us into `~/Library/Application Support` here, which
            // the spec explicitly does not want.
            directories::BaseDirs::new().map(|b| b.home_dir().join(".config/kmwarp/config.toml"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            directories::ProjectDirs::from("", "", "kmwarp")
                .map(|p| p.config_dir().join("config.toml"))
        }
    }
}

/// `[peer]` section. Both `bind` and `connect` are optional so the
/// same `Config` struct shape works for server + client; each binary
/// reads the field that applies to it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PeerConfig {
    /// Server-side: `IP:PORT` to bind the listener on.
    pub bind: Option<String>,
    /// Client-side: `IP:PORT` of the server to dial.
    pub connect: Option<String>,
    /// Human-readable peer name, sent in the `Hello` handshake.
    pub name: Option<String>,
}

/// `[edge]` section: cursor-crossing layout + remote screen
/// dimensions.
///
/// `side` defaults to `Right` (v1 hardcodes the Windows-right-of-Mac
/// topology); v1.1's M11 config UI exposes the other sides.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EdgeSection {
    pub side: EdgeSide,
    pub remote_screen_px: Option<(u32, u32)>,
}

impl Default for EdgeSection {
    fn default() -> Self {
        Self {
            side: EdgeSide::Right,
            remote_screen_px: None,
        }
    }
}

/// Which screen edge crosses to the remote peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeSide {
    Right,
    Left,
    Top,
    Bottom,
}

/// `[tls]` section: cert pinning (M9).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// Filesystem path to the pinned peer cert SHA-256 (hex). Tilde
    /// expansion is the binary's responsibility (`directories` /
    /// `shellexpand`) — `core` stores the raw string.
    pub pin_file: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    use crate::modmap::ModTarget;

    #[test]
    fn default_config_is_loadable_and_sensible() {
        let cfg = Config::default();
        assert!(cfg.peer.bind.is_none());
        assert!(cfg.peer.connect.is_none());
        assert_eq!(cfg.edge.side, EdgeSide::Right);
        assert_eq!(cfg.modifiers.cmd, ModTarget::Ctrl);
        assert_eq!(cfg.modifiers.option, ModTarget::Alt);
        assert!(cfg.tls.pin_file.is_none());
    }

    #[test]
    fn parse_empty_config_yields_defaults() {
        let cfg = Config::parse("").expect("empty parses");
        assert_eq!(cfg.edge.side, EdgeSide::Right);
        assert_eq!(cfg.modifiers.cmd, ModTarget::Ctrl);
    }

    #[test]
    fn full_toml_round_trips() {
        let toml = r#"
            [peer]
            bind = "0.0.0.0:51423"
            connect = "10.0.0.5:51423"
            name = "merajs-mbp"

            [edge]
            side = "right"
            remote_screen_px = [2560, 1440]

            [modifiers]
            cmd = "ctrl"
            option = "alt"

            [tls]
            pin_file = "~/.config/kmwarp/peer.pin"
        "#;
        let cfg = Config::parse(toml).expect("parse");
        assert_eq!(cfg.peer.bind.as_deref(), Some("0.0.0.0:51423"));
        assert_eq!(cfg.peer.connect.as_deref(), Some("10.0.0.5:51423"));
        assert_eq!(cfg.peer.name.as_deref(), Some("merajs-mbp"));
        assert_eq!(cfg.edge.side, EdgeSide::Right);
        assert_eq!(cfg.edge.remote_screen_px, Some((2560, 1440)));
        assert_eq!(cfg.modifiers.cmd, ModTarget::Ctrl);
        assert_eq!(cfg.modifiers.option, ModTarget::Alt);
        assert_eq!(
            cfg.tls.pin_file.as_deref(),
            Some("~/.config/kmwarp/peer.pin")
        );
    }

    #[test]
    fn partial_toml_uses_defaults_for_missing_sections() {
        let toml = r#"
            [modifiers]
            cmd = "alt"
        "#;
        let cfg = Config::parse(toml).expect("parse");
        assert_eq!(cfg.modifiers.cmd, ModTarget::Alt);
        // Other sections still get sensible defaults.
        assert_eq!(cfg.edge.side, EdgeSide::Right);
        assert!(cfg.peer.bind.is_none());
        assert!(cfg.tls.pin_file.is_none());
    }

    #[test]
    fn unknown_modifier_name_is_a_parse_error() {
        let toml = r#"
            [modifiers]
            cmd = "hyper"
        "#;
        assert!(Config::parse(toml).is_err());
    }

    #[test]
    fn edge_side_accepts_all_four_directions() {
        for (input, expected) in [
            ("right", EdgeSide::Right),
            ("left", EdgeSide::Left),
            ("top", EdgeSide::Top),
            ("bottom", EdgeSide::Bottom),
        ] {
            let toml = format!("[edge]\nside = \"{input}\"");
            let cfg = Config::parse(&toml).expect("parse");
            assert_eq!(cfg.edge.side, expected);
        }
    }

    #[test]
    fn load_from_path_reads_a_real_file() {
        let mut f = tempfile::NamedTempFile::new().expect("tempfile");
        writeln!(
            f,
            r#"
                [modifiers]
                cmd = "alt"
                option = "ctrl"
            "#
        )
        .unwrap();

        let cfg = Config::load_from_path(f.path()).expect("load");
        assert_eq!(cfg.modifiers.cmd, ModTarget::Alt);
        assert_eq!(cfg.modifiers.option, ModTarget::Ctrl);
    }

    #[test]
    fn load_from_path_returns_io_error_on_missing_file() {
        let bogus = PathBuf::from("/nonexistent/kmwarp-test-1234/config.toml");
        match Config::load_from_path(&bogus) {
            Err(ConfigError::Io(_)) => {}
            other => panic!("expected IO error, got {other:?}"),
        }
    }

    #[test]
    fn load_default_returns_default_when_file_absent() {
        // We can't reliably guarantee the user's config dir is empty,
        // so we verify the documented contract directly:
        // `load_default()` returns `Ok(Default)` when the file at
        // `default_config_path()` doesn't exist.
        if let Some(path) = Config::default_config_path() {
            if !path.exists() {
                let cfg = Config::load_default().expect("default ok");
                // Sanity: it's the default shape.
                assert_eq!(cfg.modifiers.cmd, ModTarget::Ctrl);
            }
        }
    }

    #[test]
    fn default_config_path_resolves_on_this_platform() {
        // Just assert the path resolves — content may not exist.
        let path = Config::default_config_path();
        assert!(path.is_some(), "no home dir available?");
        let path = path.unwrap();
        // On macOS the spec says ~/.config/kmwarp/config.toml.
        #[cfg(target_os = "macos")]
        {
            assert!(
                path.to_string_lossy()
                    .contains(".config/kmwarp/config.toml"),
                "macOS path should follow XDG convention: {path:?}"
            );
        }
    }
}
