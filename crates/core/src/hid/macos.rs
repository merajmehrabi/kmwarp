//! macOS virtual keycode ↔ USB HID usage translation.
//!
//! The macOS VK constants come from `Carbon/HIToolbox/Events.h`
//! (`kVK_ANSI_*`, `kVK_Return`, etc.). The HID codes come from USB HID
//! Usage Page 0x07 (Keyboard/Keypad). Both sides are documented inline
//! so a reviewer can verify each row without flipping between headers.
//!
//! Note: `kVK_Delete` (0x33, the main "Delete"/backspace key) maps to
//! HID 0x2A (Keyboard DELETE/Backspace). The separate "fn delete" key
//! (`kVK_ForwardDelete` = 0x75) maps to HID 0x4C (Keyboard Delete
//! Forward) — these two are easy to confuse and the bijection test
//! pins the distinction.

use crate::hid::HidUsage;

/// `(macOS virtual keycode, USB HID usage code)` mapping, exhaustive for
/// the v1 supported key set (alphanumeric + common punctuation + arrows +
/// nav cluster + F1–F12 + modifiers).
///
/// Lookup helpers: [`macos_to_hid`], [`hid_to_macos`]. Direct scans of
/// this slice are also fine — the bijection test pins ordering invariants.
pub const MACOS_VK_TO_HID: &[(u16, HidUsage)] = &[
    // ── Letters (kVK_ANSI_A..Z, layout-physical positions) ──
    (0x00, 0x04), // A
    (0x0B, 0x05), // B
    (0x08, 0x06), // C
    (0x02, 0x07), // D
    (0x0E, 0x08), // E
    (0x03, 0x09), // F
    (0x05, 0x0A), // G
    (0x04, 0x0B), // H
    (0x22, 0x0C), // I
    (0x26, 0x0D), // J
    (0x28, 0x0E), // K
    (0x25, 0x0F), // L
    (0x2E, 0x10), // M
    (0x2D, 0x11), // N
    (0x1F, 0x12), // O
    (0x23, 0x13), // P
    (0x0C, 0x14), // Q
    (0x0F, 0x15), // R
    (0x01, 0x16), // S
    (0x11, 0x17), // T
    (0x20, 0x18), // U
    (0x09, 0x19), // V
    (0x0D, 0x1A), // W
    (0x07, 0x1B), // X
    (0x10, 0x1C), // Y
    (0x06, 0x1D), // Z
    // ── Digits (top row, kVK_ANSI_1..0) ──
    (0x12, 0x1E), // 1
    (0x13, 0x1F), // 2
    (0x14, 0x20), // 3
    (0x15, 0x21), // 4
    (0x17, 0x22), // 5
    (0x16, 0x23), // 6
    (0x1A, 0x24), // 7
    (0x1C, 0x25), // 8
    (0x19, 0x26), // 9
    (0x1D, 0x27), // 0
    // ── Editing / whitespace ──
    (0x24, 0x28), // Return → Enter
    (0x35, 0x29), // Escape
    (0x33, 0x2A), // Delete → Backspace
    (0x30, 0x2B), // Tab
    (0x31, 0x2C), // Space
    (0x1B, 0x2D), // -
    (0x18, 0x2E), // =
    (0x21, 0x2F), // [
    (0x1E, 0x30), // ]
    (0x2A, 0x31), // \
    (0x29, 0x33), // ;
    (0x27, 0x34), // '
    (0x32, 0x35), // `
    (0x2B, 0x36), // ,
    (0x2F, 0x37), // .
    (0x2C, 0x38), // /
    (0x39, 0x39), // CapsLock
    // ── F-keys (F1..F12) ──
    (0x7A, 0x3A), // F1
    (0x78, 0x3B), // F2
    (0x63, 0x3C), // F3
    (0x76, 0x3D), // F4
    (0x60, 0x3E), // F5
    (0x61, 0x3F), // F6
    (0x62, 0x40), // F7
    (0x64, 0x41), // F8
    (0x65, 0x42), // F9
    (0x6D, 0x43), // F10
    (0x67, 0x44), // F11
    (0x6F, 0x45), // F12
    // ── Navigation cluster ──
    (0x73, 0x4A), // Home
    (0x74, 0x4B), // PageUp
    (0x75, 0x4C), // ForwardDelete → Delete (HID)
    (0x77, 0x4D), // End
    (0x79, 0x4E), // PageDown
    // ── Arrow keys ──
    (0x7C, 0x4F), // Right
    (0x7B, 0x50), // Left
    (0x7D, 0x51), // Down
    (0x7E, 0x52), // Up
    // ── Modifiers ──
    (0x3B, 0xE0), // LControl  → LeftCtrl
    (0x38, 0xE1), // LShift    → LeftShift
    (0x3A, 0xE2), // LOption   → LeftAlt
    (0x37, 0xE3), // LCommand  → LeftGUI
    (0x3E, 0xE4), // RControl  → RightCtrl
    (0x3C, 0xE5), // RShift    → RightShift
    (0x3D, 0xE6), // ROption   → RightAlt
    (0x36, 0xE7), // RCommand  → RightGUI
];

/// Translate a macOS virtual keycode to its USB HID usage code.
///
/// Returns `None` for keys outside the v1 supported set; callers should
/// drop the event (and the server's tap will emit a `trace!` so the
/// missing key is visible during M5 acceptance).
pub fn macos_to_hid(vk: u16) -> Option<HidUsage> {
    MACOS_VK_TO_HID
        .iter()
        .find(|(mac_vk, _)| *mac_vk == vk)
        .map(|(_, hid)| *hid)
}

/// Reverse lookup: USB HID usage code → macOS virtual keycode.
///
/// Reserved for the future Mac-as-client path (the v1 wire is
/// unidirectional Mac→Win); also used by the bijection test to assert
/// table consistency.
pub fn hid_to_macos(hid: HidUsage) -> Option<u16> {
    MACOS_VK_TO_HID
        .iter()
        .find(|(_, h)| *h == hid)
        .map(|(mac_vk, _)| *mac_vk)
}
