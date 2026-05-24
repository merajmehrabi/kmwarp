//! Windows-side HID translation: Win32 VK ↔ HID, HID → PS/2 scancode.
//!
//! The client uses [`hid_to_scancode`] with `SendInput` + `KEYEVENTF_SCANCODE`
//! so injection is keyboard-layout-independent (the wire carries layout-
//! independent HID codes; the kernel resolves the scancode to whatever
//! glyph the receiving user has mapped to that physical key).
//!
//! [`windows_to_hid`] is included for symmetry / future Mac-as-client use;
//! the v1 unidirectional flow only needs `hid_to_scancode`.
//!
//! **Scancode encoding:** values are PS/2 Set-1 scan codes. Extended keys
//! (arrows, nav cluster, RCtrl, RAlt, Win) carry the `0xE0` prefix in the
//! high byte — e.g. `0xE048` = Up arrow. The client splits this into
//! `wScan = scan & 0xFF` plus `KEYEVENTF_EXTENDEDKEY` iff `scan >> 8 == 0xE0`.

/// `(Win32 VK code, USB HID usage code)` mapping.
///
/// VK constants come from `winuser.h`. The v1 path doesn't use this slice
/// — the wire is unidirectional Mac→Win — but it's kept here so the future
/// Mac-as-client path and any keyboard-driven test harness on Windows can
/// share the same source of truth as the inverse [`HID_TO_SCANCODE`].
pub const WIN32_VK_TO_HID: &[(u16, u16)] = &[
    // ── Letters: VK is ASCII uppercase 0x41..=0x5A ──
    (0x41, 0x04), // A
    (0x42, 0x05), // B
    (0x43, 0x06), // C
    (0x44, 0x07), // D
    (0x45, 0x08), // E
    (0x46, 0x09), // F
    (0x47, 0x0A), // G
    (0x48, 0x0B), // H
    (0x49, 0x0C), // I
    (0x4A, 0x0D), // J
    (0x4B, 0x0E), // K
    (0x4C, 0x0F), // L
    (0x4D, 0x10), // M
    (0x4E, 0x11), // N
    (0x4F, 0x12), // O
    (0x50, 0x13), // P
    (0x51, 0x14), // Q
    (0x52, 0x15), // R
    (0x53, 0x16), // S
    (0x54, 0x17), // T
    (0x55, 0x18), // U
    (0x56, 0x19), // V
    (0x57, 0x1A), // W
    (0x58, 0x1B), // X
    (0x59, 0x1C), // Y
    (0x5A, 0x1D), // Z
    // ── Digits: VK is ASCII '0'..='9' 0x30..=0x39 ──
    (0x31, 0x1E), // 1
    (0x32, 0x1F), // 2
    (0x33, 0x20), // 3
    (0x34, 0x21), // 4
    (0x35, 0x22), // 5
    (0x36, 0x23), // 6
    (0x37, 0x24), // 7
    (0x38, 0x25), // 8
    (0x39, 0x26), // 9
    (0x30, 0x27), // 0
    // ── Editing / whitespace ──
    (0x0D, 0x28), // VK_RETURN  → Enter
    (0x1B, 0x29), // VK_ESCAPE
    (0x08, 0x2A), // VK_BACK    → Backspace
    (0x09, 0x2B), // VK_TAB
    (0x20, 0x2C), // VK_SPACE
    (0xBD, 0x2D), // VK_OEM_MINUS  → -
    (0xBB, 0x2E), // VK_OEM_PLUS   → =
    (0xDB, 0x2F), // VK_OEM_4      → [
    (0xDD, 0x30), // VK_OEM_6      → ]
    (0xDC, 0x31), // VK_OEM_5      → \
    (0xBA, 0x33), // VK_OEM_1      → ;
    (0xDE, 0x34), // VK_OEM_7      → '
    (0xC0, 0x35), // VK_OEM_3      → `
    (0xBC, 0x36), // VK_OEM_COMMA  → ,
    (0xBE, 0x37), // VK_OEM_PERIOD → .
    (0xBF, 0x38), // VK_OEM_2      → /
    (0x14, 0x39), // VK_CAPITAL    → CapsLock
    // ── F-keys: VK_F1..VK_F12 = 0x70..0x7B ──
    (0x70, 0x3A), // F1
    (0x71, 0x3B), // F2
    (0x72, 0x3C), // F3
    (0x73, 0x3D), // F4
    (0x74, 0x3E), // F5
    (0x75, 0x3F), // F6
    (0x76, 0x40), // F7
    (0x77, 0x41), // F8
    (0x78, 0x42), // F9
    (0x79, 0x43), // F10
    (0x7A, 0x44), // F11
    (0x7B, 0x45), // F12
    // ── Navigation cluster ──
    (0x2D, 0x49), // VK_INSERT
    (0x24, 0x4A), // VK_HOME
    (0x21, 0x4B), // VK_PRIOR  → PageUp
    (0x2E, 0x4C), // VK_DELETE
    (0x23, 0x4D), // VK_END
    (0x22, 0x4E), // VK_NEXT   → PageDown
    // ── Arrows ──
    (0x27, 0x4F), // VK_RIGHT
    (0x25, 0x50), // VK_LEFT
    (0x28, 0x51), // VK_DOWN
    (0x26, 0x52), // VK_UP
    // ── Modifiers (use left/right distinguished VKs where defined) ──
    (0xA2, 0xE0), // VK_LCONTROL
    (0xA0, 0xE1), // VK_LSHIFT
    (0xA4, 0xE2), // VK_LMENU    → LeftAlt
    (0x5B, 0xE3), // VK_LWIN     → LeftGUI
    (0xA3, 0xE4), // VK_RCONTROL
    (0xA1, 0xE5), // VK_RSHIFT
    (0xA5, 0xE6), // VK_RMENU    → RightAlt
    (0x5C, 0xE7), // VK_RWIN     → RightGUI
];

/// `(USB HID usage, PS/2 Set-1 scancode)`. Extended-key scancodes carry
/// `0xE0` in the high byte; see module-level docs for the split convention.
pub const HID_TO_SCANCODE: &[(u16, u16)] = &[
    // ── Letters ──
    (0x04, 0x1E), // A
    (0x05, 0x30), // B
    (0x06, 0x2E), // C
    (0x07, 0x20), // D
    (0x08, 0x12), // E
    (0x09, 0x21), // F
    (0x0A, 0x22), // G
    (0x0B, 0x23), // H
    (0x0C, 0x17), // I
    (0x0D, 0x24), // J
    (0x0E, 0x25), // K
    (0x0F, 0x26), // L
    (0x10, 0x32), // M
    (0x11, 0x31), // N
    (0x12, 0x18), // O
    (0x13, 0x19), // P
    (0x14, 0x10), // Q
    (0x15, 0x13), // R
    (0x16, 0x1F), // S
    (0x17, 0x14), // T
    (0x18, 0x16), // U
    (0x19, 0x2F), // V
    (0x1A, 0x11), // W
    (0x1B, 0x2D), // X
    (0x1C, 0x15), // Y
    (0x1D, 0x2C), // Z
    // ── Digits (top row) ──
    (0x1E, 0x02), // 1
    (0x1F, 0x03), // 2
    (0x20, 0x04), // 3
    (0x21, 0x05), // 4
    (0x22, 0x06), // 5
    (0x23, 0x07), // 6
    (0x24, 0x08), // 7
    (0x25, 0x09), // 8
    (0x26, 0x0A), // 9
    (0x27, 0x0B), // 0
    // ── Editing / whitespace ──
    (0x28, 0x1C), // Enter
    (0x29, 0x01), // Escape
    (0x2A, 0x0E), // Backspace
    (0x2B, 0x0F), // Tab
    (0x2C, 0x39), // Space
    (0x2D, 0x0C), // -
    (0x2E, 0x0D), // =
    (0x2F, 0x1A), // [
    (0x30, 0x1B), // ]
    (0x31, 0x2B), // \
    (0x33, 0x27), // ;
    (0x34, 0x28), // '
    (0x35, 0x29), // `
    (0x36, 0x33), // ,
    (0x37, 0x34), // .
    (0x38, 0x35), // /
    (0x39, 0x3A), // CapsLock
    // ── F-keys ──
    (0x3A, 0x3B), // F1
    (0x3B, 0x3C), // F2
    (0x3C, 0x3D), // F3
    (0x3D, 0x3E), // F4
    (0x3E, 0x3F), // F5
    (0x3F, 0x40), // F6
    (0x40, 0x41), // F7
    (0x41, 0x42), // F8
    (0x42, 0x43), // F9
    (0x43, 0x44), // F10
    (0x44, 0x57), // F11
    (0x45, 0x58), // F12
    // ── Navigation cluster (all extended) ──
    (0x49, 0xE052), // Insert
    (0x4A, 0xE047), // Home
    (0x4B, 0xE049), // PageUp
    (0x4C, 0xE053), // Delete
    (0x4D, 0xE04F), // End
    (0x4E, 0xE051), // PageDown
    // ── Arrows (all extended) ──
    (0x4F, 0xE04D), // Right
    (0x50, 0xE04B), // Left
    (0x51, 0xE050), // Down
    (0x52, 0xE048), // Up
    // ── Modifiers ──
    (0xE0, 0x001D), // LCtrl
    (0xE1, 0x002A), // LShift
    (0xE2, 0x0038), // LAlt
    (0xE3, 0xE05B), // LGUI  (extended)
    (0xE4, 0xE01D), // RCtrl (extended)
    (0xE5, 0x0036), // RShift
    (0xE6, 0xE038), // RAlt  (extended)
    (0xE7, 0xE05C), // RGUI  (extended)
];

/// Translate a Win32 VK to its USB HID usage code. `None` for unmapped VKs.
pub fn windows_to_hid(vk: u16) -> Option<u16> {
    WIN32_VK_TO_HID
        .iter()
        .find(|(win_vk, _)| *win_vk == vk)
        .map(|(_, hid)| *hid)
}

/// Translate a USB HID usage code to its PS/2 Set-1 scancode (with the
/// `0xE0` extended-key prefix encoded in the high byte where applicable;
/// see module-level docs).
pub fn hid_to_scancode(hid: u16) -> Option<u16> {
    HID_TO_SCANCODE
        .iter()
        .find(|(h, _)| *h == hid)
        .map(|(_, sc)| *sc)
}

/// True iff `scancode` (as returned by [`hid_to_scancode`]) represents an
/// extended key — i.e. the client should set `KEYEVENTF_EXTENDEDKEY` when
/// passing it to `SendInput`. Defined here so the convention stays with
/// the table, not buried in the platform crate.
pub fn is_extended_scancode(scancode: u16) -> bool {
    (scancode >> 8) == 0xE0
}

/// Extract the low byte of `scancode` for use as `wScan` in a `KEYBDINPUT`.
/// Trivial helper, included so the platform-side code is grep-able for
/// the convention.
pub fn scancode_low_byte(scancode: u16) -> u16 {
    scancode & 0x00FF
}
