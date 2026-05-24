//! Modifier remap: HID-level and ModMask-level translation.
//!
//! Per spec, modifier remap is a config-driven translation layer applied
//! during platform-side encode/decode, **never** in the wire protocol
//! itself. The on-wire key codes are USB HID usages; this module
//! translates the few modifier HIDs (and the parallel `ModMask` bits)
//! at the encoding boundary.
//!
//! Default Mac→Windows mapping:
//! - Cmd  (HID 0xE3/0xE7) → Ctrl (HID 0xE0/0xE4) — so Cmd+C becomes Ctrl+C.
//! - Option (HID 0xE2/0xE6) → Alt (already the same HID, identity).
//! - Shift, Ctrl: passthrough.
//!
//! Two parallel API entry points the server runtime calls:
//! - [`ModRemap::apply_to_hid`] for *modifier KeyEvents themselves* —
//!   when the user presses Cmd, the wire frame should carry the Ctrl
//!   HID, not the Cmd HID.
//! - [`ModRemap::apply_to_modmask`] for the `mods` byte of
//!   *non-modifier* KeyEvents — when the user presses Cmd+C, the C
//!   frame's `modifiers` byte should have the Ctrl bit set, not Meta.

use serde::Deserialize;

use crate::hid::usage;
use crate::platform::ModMask;

/// Destination modifier name the source-side modifier is remapped to.
///
/// `Identity` means "leave the modifier alone" — the wire output for
/// that input bit equals the input bit. Different from picking a
/// specific destination (e.g. `Alt`) when the input bit happens to be
/// the same kind: `Identity` is symbolic and resilient to a future
/// renumbering of the wire `ModMask` layout.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModTarget {
    Ctrl,
    Alt,
    Shift,
    Meta,
    #[serde(alias = "win")]
    Identity,
}

/// `[modifiers]` section of the TOML config.
///
/// Shift and Ctrl are always passthrough — the spec calls out Cmd↔Ctrl
/// and Option↔Alt as the only two remappable modifiers in v1, so the
/// other two have no config knob.
#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ModRemap {
    /// What the macOS Cmd key (HID 0xE3/0xE7, ModMask::META) becomes.
    /// Default: Ctrl.
    pub cmd: ModTarget,
    /// What the macOS Option key (HID 0xE2/0xE6, ModMask::ALT) becomes.
    /// Default: Alt (already the same HID).
    pub option: ModTarget,
}

impl Default for ModRemap {
    fn default() -> Self {
        Self {
            cmd: ModTarget::Ctrl,
            option: ModTarget::Alt,
        }
    }
}

impl ModRemap {
    /// Identity remap — no Cmd↔Ctrl swap. Useful for tests and for the
    /// future Mac-as-client direction where the user wants their Mac
    /// keyboard layout reproduced verbatim on the peer.
    pub fn identity() -> Self {
        Self {
            cmd: ModTarget::Identity,
            option: ModTarget::Identity,
        }
    }

    /// Translate a single HID usage per the remap. Non-modifier HIDs
    /// pass through unchanged.
    ///
    /// Left- and right-sided modifiers map to the corresponding
    /// left/right destination (e.g. `Cmd → Ctrl` sends `LCmd → LCtrl`
    /// and `RCmd → RCtrl`).
    pub fn apply_to_hid(&self, hid: u16) -> u16 {
        match hid {
            usage::LEFT_GUI => left_hid_for(self.cmd).unwrap_or(hid),
            usage::RIGHT_GUI => right_hid_for(self.cmd).unwrap_or(hid),
            usage::LEFT_ALT => left_hid_for(self.option).unwrap_or(hid),
            usage::RIGHT_ALT => right_hid_for(self.option).unwrap_or(hid),
            _ => hid,
        }
    }

    /// Translate a `ModMask` chord byte per the remap.
    ///
    /// Combining is OR. With default `cmd = Ctrl, option = Alt`:
    /// - output CTRL = input CTRL ∨ input META  (Cmd folds into Ctrl)
    /// - output META = 0                        (no input targets Meta)
    /// - output ALT  = input ALT                (Option → Alt identity)
    /// - output SHIFT = input SHIFT             (always passthrough)
    pub fn apply_to_modmask(&self, src: ModMask) -> ModMask {
        let mut out = ModMask::default();
        // Shift + Ctrl are always passthrough.
        if src.contains(ModMask::SHIFT) {
            out.insert(ModMask::SHIFT);
        }
        if src.contains(ModMask::CTRL) {
            out.insert(ModMask::CTRL);
        }
        // Option remaps to whatever `option` says.
        if src.contains(ModMask::ALT) {
            out.insert(modmask_for(self.option, ModMask::ALT));
        }
        // Cmd remaps to whatever `cmd` says.
        if src.contains(ModMask::META) {
            out.insert(modmask_for(self.cmd, ModMask::META));
        }
        out
    }
}

/// Map a [`ModTarget`] to a left-sided HID. `Identity` returns `None`
/// so the caller can fall back to the original HID.
fn left_hid_for(target: ModTarget) -> Option<u16> {
    match target {
        ModTarget::Ctrl => Some(usage::LEFT_CTRL),
        ModTarget::Alt => Some(usage::LEFT_ALT),
        ModTarget::Shift => Some(usage::LEFT_SHIFT),
        ModTarget::Meta => Some(usage::LEFT_GUI),
        ModTarget::Identity => None,
    }
}

/// Map a [`ModTarget`] to a right-sided HID. `Identity` returns `None`.
fn right_hid_for(target: ModTarget) -> Option<u16> {
    match target {
        ModTarget::Ctrl => Some(usage::RIGHT_CTRL),
        ModTarget::Alt => Some(usage::RIGHT_ALT),
        ModTarget::Shift => Some(usage::RIGHT_SHIFT),
        ModTarget::Meta => Some(usage::RIGHT_GUI),
        ModTarget::Identity => None,
    }
}

/// Map a [`ModTarget`] to the single-bit [`ModMask`] it represents.
/// `Identity` returns `original` (the source bit unchanged).
fn modmask_for(target: ModTarget, original: ModMask) -> ModMask {
    match target {
        ModTarget::Ctrl => ModMask::CTRL,
        ModTarget::Alt => ModMask::ALT,
        ModTarget::Shift => ModMask::SHIFT,
        ModTarget::Meta => ModMask::META,
        ModTarget::Identity => original,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_remap_translates_cmd_to_ctrl() {
        let r = ModRemap::default();
        // Left and right Cmd both go to their respective Ctrl.
        assert_eq!(r.apply_to_hid(usage::LEFT_GUI), usage::LEFT_CTRL);
        assert_eq!(r.apply_to_hid(usage::RIGHT_GUI), usage::RIGHT_CTRL);
    }

    #[test]
    fn default_remap_keeps_option_as_alt() {
        let r = ModRemap::default();
        // Option is already Alt — HID unchanged.
        assert_eq!(r.apply_to_hid(usage::LEFT_ALT), usage::LEFT_ALT);
        assert_eq!(r.apply_to_hid(usage::RIGHT_ALT), usage::RIGHT_ALT);
    }

    #[test]
    fn custom_remap_can_swap_cmd_to_alt() {
        let r = ModRemap {
            cmd: ModTarget::Alt,
            option: ModTarget::Ctrl,
        };
        assert_eq!(r.apply_to_hid(usage::LEFT_GUI), usage::LEFT_ALT);
        assert_eq!(r.apply_to_hid(usage::RIGHT_GUI), usage::RIGHT_ALT);
        assert_eq!(r.apply_to_hid(usage::LEFT_ALT), usage::LEFT_CTRL);
        assert_eq!(r.apply_to_hid(usage::RIGHT_ALT), usage::RIGHT_CTRL);
    }

    #[test]
    fn non_modifier_hids_pass_through() {
        let r = ModRemap::default();
        for hid in [
            usage::A,
            usage::Z,
            usage::D0,
            usage::ENTER,
            usage::SPACE,
            usage::F1,
            usage::LEFT_ARROW,
            usage::LEFT_CTRL,  // already Ctrl on the left
            usage::LEFT_SHIFT, // passthrough modifier
            usage::RIGHT_CTRL,
            usage::RIGHT_SHIFT,
        ] {
            assert_eq!(
                r.apply_to_hid(hid),
                hid,
                "HID {hid:#04X} should pass through"
            );
        }
    }

    #[test]
    fn identity_remap_leaves_modifiers_alone() {
        let r = ModRemap::identity();
        for hid in [
            usage::LEFT_GUI,
            usage::RIGHT_GUI,
            usage::LEFT_ALT,
            usage::RIGHT_ALT,
        ] {
            assert_eq!(r.apply_to_hid(hid), hid);
        }
    }

    #[test]
    fn apply_to_modmask_for_defaults() {
        let r = ModRemap::default();
        // Bare META → CTRL, META bit clear.
        let out = r.apply_to_modmask(ModMask::META);
        assert!(out.contains(ModMask::CTRL));
        assert!(!out.contains(ModMask::META));

        // CTRL + META input → CTRL only (no double set on output).
        let mut src = ModMask::default();
        src.insert(ModMask::CTRL);
        src.insert(ModMask::META);
        let out = r.apply_to_modmask(src);
        assert!(out.contains(ModMask::CTRL));
        assert!(!out.contains(ModMask::META));

        // SHIFT + ALT pass through.
        let mut src = ModMask::default();
        src.insert(ModMask::SHIFT);
        src.insert(ModMask::ALT);
        let out = r.apply_to_modmask(src);
        assert!(out.contains(ModMask::SHIFT));
        assert!(out.contains(ModMask::ALT));
        assert!(!out.contains(ModMask::CTRL));
        assert!(!out.contains(ModMask::META));
    }

    #[test]
    fn empty_modmask_remaps_to_empty() {
        assert_eq!(
            ModRemap::default().apply_to_modmask(ModMask::default()),
            ModMask::default()
        );
    }

    #[test]
    fn identity_modmask_preserves_all_bits() {
        let r = ModRemap::identity();
        for m in [ModMask::SHIFT, ModMask::CTRL, ModMask::ALT, ModMask::META] {
            assert_eq!(r.apply_to_modmask(m), m);
        }
    }
}
