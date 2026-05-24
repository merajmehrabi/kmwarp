//! HID table invariants + ASCII coverage spot-checks + ModMask wire codec.
//!
//! Tables in `core::hid` are `&[(u16, _)]` const slices. These tests pin
//! the invariants the platform layers depend on:
//!
//! - **Bijection**: no two source codes map to the same destination, and
//!   no two destination codes are produced by different sources. A
//!   collision either way would silently drop key events at runtime.
//! - **Coverage**: every alphabet letter, digit, and a representative
//!   set of punctuation round-trips macOS-VK → HID → scancode.
//! - **Modifier closure**: the four modifier classes (Ctrl/Shift/Alt/GUI)
//!   each have both left and right entries in both tables.
//! - **Extended-key convention**: keys we expect to be extended (arrows,
//!   nav cluster) come back with `extended: true`; letters do not.
//! - **macOS Delete confusion guard**: `kVK_Delete` (0x33) → HID 0x2A
//!   (Backspace), and `kVK_ForwardDelete` (0x75) → HID 0x4C (Delete).
//!   These two are the easiest HID newbie trap.
//! - **ModMask wire round-trip** masks off reserved bits 4-7 for every
//!   possible input byte.

use std::collections::HashSet;

use kmwarp_core::hid::{
    hid_to_macos, hid_to_windows_scancode, macos_to_hid, usage, windows_to_hid, HID_TO_SCANCODE,
    MACOS_VK_TO_HID, WIN32_VK_TO_HID,
};
use kmwarp_core::ModMask;

/// Helper: assert no source value is repeated and no destination value is
/// repeated. Generic over the destination type so we can re-use this for
/// `(u16, u16)` and `(u16, _)` tables.
fn assert_bijection<T: Copy + std::fmt::Debug + std::hash::Hash + Eq>(
    label: &str,
    table: &[(u16, T)],
) {
    let mut sources = HashSet::with_capacity(table.len());
    let mut dests: HashSet<T> = HashSet::with_capacity(table.len());
    for (src, dst) in table {
        assert!(
            sources.insert(*src),
            "{label}: duplicate source code 0x{src:04X}"
        );
        assert!(
            dests.insert(*dst),
            "{label}: duplicate destination value {dst:?}"
        );
    }
}

#[test]
fn macos_vk_to_hid_is_a_bijection() {
    assert_bijection("MACOS_VK_TO_HID", MACOS_VK_TO_HID);
}

#[test]
fn win32_vk_to_hid_is_a_bijection() {
    assert_bijection("WIN32_VK_TO_HID", WIN32_VK_TO_HID);
}

#[test]
fn hid_to_scancode_is_a_bijection() {
    assert_bijection("HID_TO_SCANCODE", HID_TO_SCANCODE);
}

#[test]
fn macos_forward_and_reverse_are_mutual_inverses() {
    for (vk, hid) in MACOS_VK_TO_HID {
        assert_eq!(macos_to_hid(*vk), Some(*hid));
        assert_eq!(
            hid_to_macos(*hid),
            Some(*vk),
            "hid_to_macos(0x{hid:04X}) should round-trip to 0x{vk:04X}"
        );
    }
    // VK 0xFF is not in any kVK_* constant; helper should reject it.
    assert_eq!(macos_to_hid(0xFF), None);
    // HID 0xFFFF is outside Page 0x07; reverse should also be None.
    assert_eq!(hid_to_macos(0xFFFF), None);
}

#[test]
fn windows_lookup_helpers_match_tables() {
    for (vk, hid) in WIN32_VK_TO_HID {
        assert_eq!(windows_to_hid(*vk), Some(*hid));
    }
    for (hid, packed) in HID_TO_SCANCODE {
        let sc = hid_to_windows_scancode(*hid).expect("scancode lookup succeeds");
        // The struct must reflect the packed table value exactly.
        let expected_extended = (packed >> 8) == 0xE0;
        let expected_code = packed & 0x00FF;
        assert_eq!(sc.code, expected_code, "code for HID 0x{hid:04X}");
        assert_eq!(
            sc.extended, expected_extended,
            "extended for HID 0x{hid:04X}"
        );
    }
    // Bogus HID code (out of Page 0x07 range we cover) → None.
    assert_eq!(hid_to_windows_scancode(0xFFFF), None);
}

#[test]
fn ascii_alphabet_roundtrips_macos_vk_to_hid_to_scancode() {
    let letters: &[(u16, u16)] = &[
        (0x00, usage::A),
        (0x0B, usage::B),
        (0x08, usage::C),
        (0x02, usage::D),
        (0x0E, usage::E),
        (0x03, usage::F),
        (0x05, usage::G),
        (0x04, usage::H),
        (0x22, usage::I),
        (0x26, usage::J),
        (0x28, usage::K),
        (0x25, usage::L),
        (0x2E, usage::M),
        (0x2D, usage::N),
        (0x1F, usage::O),
        (0x23, usage::P),
        (0x0C, usage::Q),
        (0x0F, usage::R),
        (0x01, usage::S),
        (0x11, usage::T),
        (0x20, usage::U),
        (0x09, usage::V),
        (0x0D, usage::W),
        (0x07, usage::X),
        (0x10, usage::Y),
        (0x06, usage::Z),
    ];
    for (mac_vk, expected_hid) in letters {
        let hid =
            macos_to_hid(*mac_vk).unwrap_or_else(|| panic!("no HID for mac VK {mac_vk:#04X}"));
        assert_eq!(hid, *expected_hid, "letter at mac VK {mac_vk:#04X}");
        let sc = hid_to_windows_scancode(hid)
            .unwrap_or_else(|| panic!("no scancode for HID {hid:#04X}"));
        assert_ne!(sc.code, 0, "letter scancodes are non-zero");
        assert!(!sc.extended, "letter scancodes are not extended");
    }
}

#[test]
fn digits_roundtrip() {
    let digits: &[(u16, u16)] = &[
        (0x12, usage::D1),
        (0x13, usage::D2),
        (0x14, usage::D3),
        (0x15, usage::D4),
        (0x17, usage::D5),
        (0x16, usage::D6),
        (0x1A, usage::D7),
        (0x1C, usage::D8),
        (0x19, usage::D9),
        (0x1D, usage::D0),
    ];
    for (mac_vk, expected_hid) in digits {
        assert_eq!(macos_to_hid(*mac_vk), Some(*expected_hid));
        assert!(hid_to_windows_scancode(*expected_hid).is_some());
    }
}

#[test]
fn common_punctuation_roundtrips() {
    let entries: &[(u16, u16)] = &[
        (0x24, usage::ENTER),
        (0x35, usage::ESCAPE),
        (0x33, usage::BACKSPACE),
        (0x30, usage::TAB),
        (0x31, usage::SPACE),
        (0x1B, usage::MINUS),
        (0x18, usage::EQUAL),
        (0x21, usage::LBRACKET),
        (0x1E, usage::RBRACKET),
        (0x2A, usage::BACKSLASH),
        (0x29, usage::SEMICOLON),
        (0x27, usage::QUOTE),
        (0x32, usage::GRAVE),
        (0x2B, usage::COMMA),
        (0x2F, usage::PERIOD),
        (0x2C, usage::SLASH),
    ];
    for (mac_vk, expected_hid) in entries {
        assert_eq!(macos_to_hid(*mac_vk), Some(*expected_hid));
        assert!(
            hid_to_windows_scancode(*expected_hid).is_some(),
            "missing scancode for HID {expected_hid:#04X}"
        );
    }
}

#[test]
fn macos_delete_keys_are_distinct() {
    // The two macOS Delete keys are the classic HID newbie trap:
    //   kVK_Delete (0x33) — the main "delete" key (above Return on a Mac
    //   keyboard) — maps to HID 0x2A (USB HID Keyboard "DELETE", which is
    //   actually Backspace in PC parlance).
    //   kVK_ForwardDelete (0x75) — fn+delete or the dedicated Forward
    //   Delete key — maps to HID 0x4C (USB HID Keyboard "Delete Forward").
    assert_eq!(
        macos_to_hid(0x33),
        Some(0x2A),
        "kVK_Delete should map to HID 0x2A (Backspace)"
    );
    assert_eq!(
        macos_to_hid(0x75),
        Some(0x4C),
        "kVK_ForwardDelete should map to HID 0x4C (Delete Forward)"
    );
    assert_eq!(usage::BACKSPACE, 0x2A);
    assert_eq!(usage::DELETE, 0x4C);
}

#[test]
fn arrows_are_extended_scancodes() {
    for hid in [
        usage::LEFT_ARROW,
        usage::RIGHT_ARROW,
        usage::UP_ARROW,
        usage::DOWN_ARROW,
    ] {
        let sc = hid_to_windows_scancode(hid).expect("arrow has scancode");
        assert!(
            sc.extended,
            "arrow HID {hid:#04X} → scancode {sc:?} should be extended"
        );
    }
}

#[test]
fn navigation_keys_are_extended_scancodes() {
    for hid in [
        usage::INSERT,
        usage::HOME,
        usage::PAGE_UP,
        usage::DELETE,
        usage::END,
        usage::PAGE_DOWN,
    ] {
        let sc = hid_to_windows_scancode(hid).expect("nav key has scancode");
        assert!(
            sc.extended,
            "nav HID {hid:#04X} → scancode {sc:?} should be extended"
        );
    }
}

#[test]
fn letter_scancodes_are_not_extended() {
    for hid in usage::A..=usage::Z {
        let sc = hid_to_windows_scancode(hid).expect("letter has scancode");
        assert!(
            !sc.extended,
            "letter HID {hid:#04X} → scancode {sc:?} should not be extended"
        );
    }
}

#[test]
fn modifier_classes_have_left_and_right_entries() {
    let modifiers: &[u16] = &[
        usage::LEFT_CTRL,
        usage::LEFT_SHIFT,
        usage::LEFT_ALT,
        usage::LEFT_GUI,
        usage::RIGHT_CTRL,
        usage::RIGHT_SHIFT,
        usage::RIGHT_ALT,
        usage::RIGHT_GUI,
    ];
    for hid in modifiers {
        assert!(
            hid_to_windows_scancode(*hid).is_some(),
            "no scancode for modifier HID {hid:#04X}"
        );
        assert!(
            MACOS_VK_TO_HID.iter().any(|(_, h)| h == hid),
            "MACOS_VK_TO_HID missing modifier HID {hid:#04X}"
        );
        assert!(
            WIN32_VK_TO_HID.iter().any(|(_, h)| h == hid),
            "WIN32_VK_TO_HID missing modifier HID {hid:#04X}"
        );
    }
}

#[test]
fn every_macos_hid_has_a_scancode() {
    // Anything the server can produce from a key tap must be dispatchable
    // on the Windows side. The reverse is NOT required — the scancode
    // table also serves the future Mac-as-client path.
    for (_, hid) in MACOS_VK_TO_HID {
        assert!(
            hid_to_windows_scancode(*hid).is_some(),
            "MACOS_VK_TO_HID HID 0x{hid:04X} has no scancode entry"
        );
    }
}

#[test]
fn modmask_wire_codec_clears_reserved_bits_for_every_byte() {
    // `from_wire(byte).to_wire()` must equal `byte & 0x0F` for every
    // possible input — bits 4-7 are reserved and cannot leak through.
    for byte in 0u8..=255 {
        let m = ModMask::from_wire(byte);
        assert_eq!(
            m.to_wire(),
            byte & 0x0F,
            "reserved bits leaked through ModMask round-trip for byte 0x{byte:02X}"
        );
    }
    // Spot-check the well-known masks.
    assert_eq!(ModMask::from_wire(0b0000_0001), ModMask::SHIFT);
    assert_eq!(ModMask::from_wire(0b0000_0010), ModMask::CTRL);
    assert_eq!(ModMask::from_wire(0b0000_0100), ModMask::ALT);
    assert_eq!(ModMask::from_wire(0b0000_1000), ModMask::META);
    // Reserved bit 7 set: stripped on the way in.
    assert_eq!(ModMask::from_wire(0b1000_0001), ModMask::SHIFT);
}
