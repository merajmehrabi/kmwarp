//! Configuration: TOML parsing + modifier remap.
//!
//! v1 lands the `[modifiers]` section that M7 needs. Other sections
//! (`[peer]`, `[edge]`, `[tls]`) will land alongside their respective
//! milestones (M1 already hardcodes peer addrs; M6 already uses
//! `EdgeConfig::default()`; M9 adds TLS pinning). Keeping `Config` in
//! one place from the start means later milestones just fill in the
//! struct rather than introduce new files.

use serde::Deserialize;

use crate::error::ConfigError;
use crate::platform::ModMask;

/// Top-level config struct, mirroring the TOML schema in PLAN.md §Config.
///
/// Every section is optional at the TOML level so a partial config
/// (e.g. just `[modifiers]`) still parses; missing fields fall back to
/// the spec's documented defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub modifiers: ModifierConfig,
}

impl Config {
    /// Parse a config from a TOML string. Returns a typed error so
    /// `fn main()` can surface a clean message to the user.
    pub fn parse(toml_str: &str) -> Result<Self, ConfigError> {
        toml::from_str(toml_str).map_err(|e| ConfigError::Parse(e.to_string()))
    }
}

/// `[modifiers]` section.
///
/// Each field names what the corresponding **source-side** modifier
/// should become on the **wire**. The destination platform then
/// reinterprets the wire bits per its own convention.
///
/// Defaults (per PLAN.md §M7):
/// - `cmd` (macOS Cmd / META bit)   → `Ctrl`
/// - `option` (macOS Option / ALT)  → `Alt` (identity)
/// - `control` (macOS Ctrl / CTRL)  → `Ctrl` (identity)
/// - `shift` (macOS Shift / SHIFT)  → `Shift` (identity)
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ModifierConfig {
    #[serde(default = "default_cmd")]
    pub cmd: ModifierName,
    #[serde(default = "default_option")]
    pub option: ModifierName,
    #[serde(default = "default_control")]
    pub control: ModifierName,
    #[serde(default = "default_shift")]
    pub shift: ModifierName,
}

impl Default for ModifierConfig {
    fn default() -> Self {
        Self {
            cmd: default_cmd(),
            option: default_option(),
            control: default_control(),
            shift: default_shift(),
        }
    }
}

fn default_cmd() -> ModifierName {
    ModifierName::Ctrl
}
fn default_option() -> ModifierName {
    ModifierName::Alt
}
fn default_control() -> ModifierName {
    ModifierName::Ctrl
}
fn default_shift() -> ModifierName {
    ModifierName::Shift
}

/// Wire-side modifier name as it appears in the TOML config.
///
/// `meta` and `win` are interchangeable aliases for the bit-3 modifier
/// (Cmd on macOS, Win key on Windows). `none` disables the source
/// modifier — pressing it on the source side contributes no bit to the
/// outgoing wire chord. Useful for "I never want Caps Lock to do
/// anything" style configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModifierName {
    Shift,
    Ctrl,
    Alt,
    Meta,
    #[serde(alias = "win")]
    Win,
    None,
}

impl ModifierName {
    /// The single-bit `ModMask` this name represents on the wire.
    /// `None` returns `ModMask::default()` (no bits set).
    pub fn to_mask(self) -> ModMask {
        match self {
            ModifierName::Shift => ModMask::SHIFT,
            ModifierName::Ctrl => ModMask::CTRL,
            ModifierName::Alt => ModMask::ALT,
            ModifierName::Meta | ModifierName::Win => ModMask::META,
            ModifierName::None => ModMask::default(),
        }
    }
}

impl ModifierConfig {
    /// Build a [`ModRemap`] from this config. Captures the mapping at
    /// the moment of the call — re-call after a config reload.
    pub fn to_remap(self) -> ModRemap {
        ModRemap {
            shift_to: self.shift.to_mask(),
            ctrl_to: self.control.to_mask(),
            alt_to: self.option.to_mask(),
            meta_to: self.cmd.to_mask(),
        }
    }
}

/// Modifier bit-to-bit remap table.
///
/// Applied at the wire encoding boundary (source → wire) by the server
/// runtime. Each field names the destination [`ModMask`] each source
/// bit maps to; combining sources is by bitwise-OR.
///
/// # Example
///
/// Default mapping (Cmd → Ctrl, Option → Alt, others identity):
/// ```
/// use kmwarp_core::config::{ModifierConfig, ModRemap};
/// use kmwarp_core::platform::ModMask;
///
/// let remap = ModifierConfig::default().to_remap();
///
/// // Cmd+C on a Mac: source ModMask has META set.
/// let result = remap.apply(ModMask::META);
/// assert_eq!(result, ModMask::CTRL);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ModRemap {
    pub shift_to: ModMask,
    pub ctrl_to: ModMask,
    pub alt_to: ModMask,
    pub meta_to: ModMask,
}

impl Default for ModRemap {
    fn default() -> Self {
        ModifierConfig::default().to_remap()
    }
}

impl ModRemap {
    /// Apply the remap to a source-side [`ModMask`], returning the
    /// destination-side mask. Combining is OR — pressing Cmd+Shift on
    /// the source with `cmd → Ctrl` produces `Ctrl | Shift` on the wire.
    pub fn apply(&self, src: ModMask) -> ModMask {
        let mut out = ModMask::default();
        if src.contains(ModMask::SHIFT) {
            out.insert(self.shift_to);
        }
        if src.contains(ModMask::CTRL) {
            out.insert(self.ctrl_to);
        }
        if src.contains(ModMask::ALT) {
            out.insert(self.alt_to);
        }
        if src.contains(ModMask::META) {
            out.insert(self.meta_to);
        }
        out
    }

    /// Identity remap. Useful for tests and for the future
    /// Mac-as-client direction where no remap is needed.
    pub fn identity() -> Self {
        Self {
            shift_to: ModMask::SHIFT,
            ctrl_to: ModMask::CTRL,
            alt_to: ModMask::ALT,
            meta_to: ModMask::META,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_remap_maps_cmd_to_ctrl() {
        let r = ModRemap::default();
        assert_eq!(r.apply(ModMask::META), ModMask::CTRL);
    }

    #[test]
    fn default_remap_keeps_option_as_alt() {
        let r = ModRemap::default();
        assert_eq!(r.apply(ModMask::ALT), ModMask::ALT);
    }

    #[test]
    fn default_remap_leaves_shift_and_ctrl_unchanged() {
        let r = ModRemap::default();
        assert_eq!(r.apply(ModMask::SHIFT), ModMask::SHIFT);
        assert_eq!(r.apply(ModMask::CTRL), ModMask::CTRL);
    }

    #[test]
    fn default_remap_combines_with_or() {
        let r = ModRemap::default();
        // Cmd + Shift → Ctrl + Shift.
        let mut src = ModMask::default();
        src.insert(ModMask::META);
        src.insert(ModMask::SHIFT);
        let out = r.apply(src);
        assert!(out.contains(ModMask::CTRL));
        assert!(out.contains(ModMask::SHIFT));
        assert!(!out.contains(ModMask::META));
    }

    #[test]
    fn empty_modmask_remaps_to_empty() {
        let r = ModRemap::default();
        assert_eq!(r.apply(ModMask::default()), ModMask::default());
    }

    #[test]
    fn identity_remap_preserves_all_bits() {
        let r = ModRemap::identity();
        for m in [ModMask::SHIFT, ModMask::CTRL, ModMask::ALT, ModMask::META] {
            assert_eq!(r.apply(m), m);
        }
    }

    #[test]
    fn parse_full_modifiers_section() {
        let toml = r#"
            [modifiers]
            cmd = "ctrl"
            option = "alt"
            control = "ctrl"
            shift = "shift"
        "#;
        let cfg = Config::parse(toml).expect("parse");
        assert_eq!(cfg.modifiers.cmd, ModifierName::Ctrl);
        assert_eq!(cfg.modifiers.option, ModifierName::Alt);
    }

    #[test]
    fn parse_partial_modifiers_section_uses_defaults_for_missing() {
        let toml = r#"
            [modifiers]
            cmd = "alt"
        "#;
        let cfg = Config::parse(toml).expect("parse");
        assert_eq!(cfg.modifiers.cmd, ModifierName::Alt);
        // option/control/shift fall back to spec defaults.
        assert_eq!(cfg.modifiers.option, ModifierName::Alt);
        assert_eq!(cfg.modifiers.control, ModifierName::Ctrl);
        assert_eq!(cfg.modifiers.shift, ModifierName::Shift);
    }

    #[test]
    fn parse_empty_config_yields_defaults() {
        let cfg = Config::parse("").expect("empty parses");
        assert_eq!(cfg.modifiers.cmd, ModifierName::Ctrl);
    }

    #[test]
    fn parse_rejects_unknown_modifier_name() {
        let toml = r#"
            [modifiers]
            cmd = "hyper"
        "#;
        assert!(Config::parse(toml).is_err());
    }

    #[test]
    fn parse_accepts_win_alias_for_meta() {
        let toml = r#"
            [modifiers]
            cmd = "win"
        "#;
        let cfg = Config::parse(toml).expect("parse");
        assert_eq!(cfg.modifiers.cmd, ModifierName::Win);
        // Both Win and Meta produce the same wire bit.
        assert_eq!(ModifierName::Win.to_mask(), ModMask::META);
    }

    #[test]
    fn none_modifier_drops_the_bit() {
        let cfg = ModifierConfig {
            cmd: ModifierName::None,
            option: ModifierName::Alt,
            control: ModifierName::Ctrl,
            shift: ModifierName::Shift,
        };
        let remap = cfg.to_remap();
        assert_eq!(remap.apply(ModMask::META), ModMask::default());
    }

    #[test]
    fn custom_swap_remap() {
        // Swap Cmd ↔ Ctrl: cmd → ctrl, control → meta.
        let cfg = ModifierConfig {
            cmd: ModifierName::Ctrl,
            option: ModifierName::Alt,
            control: ModifierName::Meta,
            shift: ModifierName::Shift,
        };
        let remap = cfg.to_remap();
        assert_eq!(remap.apply(ModMask::META), ModMask::CTRL);
        assert_eq!(remap.apply(ModMask::CTRL), ModMask::META);
    }
}
