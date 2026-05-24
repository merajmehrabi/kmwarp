//! USB HID Usage Page 0x07 (Keyboard/Keypad) translation tables.
//!
//! The wire protocol carries keycodes as USB HID usage codes; each platform
//! layer translates at the boundary. This module owns the truth tables for
//! both directions:
//!
//! - [`macos::macos_to_hid`] / [`macos::hid_to_macos`] — macOS virtual
//!   keycode ↔ HID usage. Server uses the forward direction to encode
//!   `kCGEventKeyDown/Up` events for the wire.
//! - [`windows::windows_to_hid`] — Win32 VK → HID usage. Reserved for the
//!   future Mac-as-client path; not used in v1 unidirectional flow.
//! - [`windows::hid_to_windows_scancode`] — HID usage → PS/2 Set-1 scancode
//!   wrapped in a [`windows::WinScancode`] (code + extended flag). Client
//!   feeds this to `SendInput` with `KEYEVENTF_SCANCODE` for
//!   layout-independent injection.
//!
//! Tables are `pub const &[(u16, _)]` slices. Linear-scan lookups; ~80
//! entries, called at human typing rate, so the hash-map overhead isn't
//! worth it.
//!
//! v1 coverage: ANSI alphabet, digits, common punctuation, F1–F12, arrow /
//! navigation cluster, both-side modifiers (Ctrl/Shift/Alt/GUI). Deferred:
//! media keys, Fn-layer, numpad — tracked in IDEAS.md (per PLAN.md §M5).

pub mod macos;
pub mod windows;

/// USB HID Usage Page 0x07 key code. Pure documentation alias for `u16`
/// — keeps function signatures self-explanatory without a newtype's
/// ergonomic cost.
pub type HidUsage = u16;

pub use macos::{hid_to_macos, macos_to_hid, MACOS_VK_TO_HID};
pub use windows::{
    hid_to_windows_scancode, windows_to_hid, WinScancode, HID_TO_SCANCODE, WIN32_VK_TO_HID,
};

// Compat re-exports for the in-flight Windows M5 inject path; new
// callers should prefer `hid_to_windows_scancode` returning `WinScancode`.
#[doc(hidden)]
pub use windows::{hid_to_scancode, is_extended_scancode, scancode_low_byte};

/// Named HID usage codes for the keys v1 cares about. Kept here so tests
/// and downstream callers can reference `usage::A` instead of `0x04`.
///
/// Values are exactly the USB HID Keyboard Page (0x07) usage IDs.
#[allow(dead_code)]
pub mod usage {
    // Letters
    pub const A: u16 = 0x04;
    pub const B: u16 = 0x05;
    pub const C: u16 = 0x06;
    pub const D: u16 = 0x07;
    pub const E: u16 = 0x08;
    pub const F: u16 = 0x09;
    pub const G: u16 = 0x0A;
    pub const H: u16 = 0x0B;
    pub const I: u16 = 0x0C;
    pub const J: u16 = 0x0D;
    pub const K: u16 = 0x0E;
    pub const L: u16 = 0x0F;
    pub const M: u16 = 0x10;
    pub const N: u16 = 0x11;
    pub const O: u16 = 0x12;
    pub const P: u16 = 0x13;
    pub const Q: u16 = 0x14;
    pub const R: u16 = 0x15;
    pub const S: u16 = 0x16;
    pub const T: u16 = 0x17;
    pub const U: u16 = 0x18;
    pub const V: u16 = 0x19;
    pub const W: u16 = 0x1A;
    pub const X: u16 = 0x1B;
    pub const Y: u16 = 0x1C;
    pub const Z: u16 = 0x1D;

    // Digits (top row)
    pub const D1: u16 = 0x1E;
    pub const D2: u16 = 0x1F;
    pub const D3: u16 = 0x20;
    pub const D4: u16 = 0x21;
    pub const D5: u16 = 0x22;
    pub const D6: u16 = 0x23;
    pub const D7: u16 = 0x24;
    pub const D8: u16 = 0x25;
    pub const D9: u16 = 0x26;
    pub const D0: u16 = 0x27;

    // Editing
    pub const ENTER: u16 = 0x28;
    pub const ESCAPE: u16 = 0x29;
    pub const BACKSPACE: u16 = 0x2A;
    pub const TAB: u16 = 0x2B;
    pub const SPACE: u16 = 0x2C;
    pub const MINUS: u16 = 0x2D;
    pub const EQUAL: u16 = 0x2E;
    pub const LBRACKET: u16 = 0x2F;
    pub const RBRACKET: u16 = 0x30;
    pub const BACKSLASH: u16 = 0x31;
    pub const SEMICOLON: u16 = 0x33;
    pub const QUOTE: u16 = 0x34;
    pub const GRAVE: u16 = 0x35;
    pub const COMMA: u16 = 0x36;
    pub const PERIOD: u16 = 0x37;
    pub const SLASH: u16 = 0x38;
    pub const CAPS_LOCK: u16 = 0x39;

    // F-keys
    pub const F1: u16 = 0x3A;
    pub const F12: u16 = 0x45;

    // Navigation
    pub const INSERT: u16 = 0x49;
    pub const HOME: u16 = 0x4A;
    pub const PAGE_UP: u16 = 0x4B;
    pub const DELETE: u16 = 0x4C;
    pub const END: u16 = 0x4D;
    pub const PAGE_DOWN: u16 = 0x4E;
    pub const RIGHT_ARROW: u16 = 0x4F;
    pub const LEFT_ARROW: u16 = 0x50;
    pub const DOWN_ARROW: u16 = 0x51;
    pub const UP_ARROW: u16 = 0x52;

    // Modifiers
    pub const LEFT_CTRL: u16 = 0xE0;
    pub const LEFT_SHIFT: u16 = 0xE1;
    pub const LEFT_ALT: u16 = 0xE2;
    pub const LEFT_GUI: u16 = 0xE3;
    pub const RIGHT_CTRL: u16 = 0xE4;
    pub const RIGHT_SHIFT: u16 = 0xE5;
    pub const RIGHT_ALT: u16 = 0xE6;
    pub const RIGHT_GUI: u16 = 0xE7;
}
