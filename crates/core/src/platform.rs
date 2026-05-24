//! Platform abstraction traits.
//!
//! Neither the macOS server nor the Windows client should know about the
//! other's OS APIs. Both binaries implement the traits in this module
//! against their native platform layer; the state machine and codec
//! (`core::edge`, `core::wire`) stay platform-agnostic and thus fully
//! testable with mock implementations.
//!
//! Split across three traits so each binary only implements what it needs:
//! - `InputSource`: read keyboard/mouse events. macOS server (M2).
//! - `InputSink`: inject keyboard/mouse + cursor warp/hide. Windows client
//!   (M3+) needs the full surface; macOS server (M6) needs only the warp/
//!   hide methods.
//! - `Clipboard`: read/write/watch the system clipboard. Both sides (M8).

use crate::wire::Message;

/// Press/release transition for a key or mouse button.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyState {
    Up,
    Down,
}

/// Mouse buttons we forward in v1. Extra buttons collapse to `X1`/`X2`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

/// Modifier bitmask carried alongside every `KeyEvent`.
///
/// Bit assignments match the spec's wire-format modifier byte; the exact
/// mapping is finalized in M5 once HID translation lands. The byte is
/// reserved here so M2/M3 can already typecheck against it.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ModMask(pub u8);

impl ModMask {
    pub const SHIFT: ModMask = ModMask(1 << 0);
    pub const CTRL: ModMask = ModMask(1 << 1);
    pub const ALT: ModMask = ModMask(1 << 2);
    /// `Cmd` on macOS, `Win` on Windows. Remap is a config-time concern
    /// (see PLAN.md §Config `[modifiers]`), not a wire-format concern.
    pub const META: ModMask = ModMask(1 << 3);

    /// True iff every bit in `other` is set in `self`.
    pub fn contains(self, other: ModMask) -> bool {
        (self.0 & other.0) == other.0
    }

    pub fn insert(&mut self, other: ModMask) {
        self.0 |= other.0;
    }

    pub fn remove(&mut self, other: ModMask) {
        self.0 &= !other.0;
    }
}

/// Event emitted by an `InputSource`.
///
/// Mouse and key events match the wire-format payload semantics 1:1, so
/// the server's encode path is a near-direct translation. `CursorAt` is
/// not on the wire — it's a server-internal signal the edge state machine
/// uses to detect when the cursor crosses the linked edge (M6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEvent {
    MouseRel {
        dx: i16,
        dy: i16,
    },
    MouseButton {
        button: MouseButton,
        state: KeyState,
    },
    MouseWheel {
        dx: i16,
        dy: i16,
    },
    Key {
        hid_usage: u16,
        state: KeyState,
        mods: ModMask,
    },
    /// Absolute cursor position in physical pixels of the source screen.
    /// Used by the edge state machine in M6 to detect crossings.
    CursorAt {
        x: i32,
        y: i32,
    },
}

/// Event emitted by a `Clipboard` watcher when the system clipboard changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardEvent {
    TextChanged(String),
}

/// Sink for injecting input on the receiving side.
///
/// Implementor: Windows client in M3 (`SendInput`), macOS server in M6
/// (cursor warp/hide only — the inject_* methods can be `unimplemented!()`
/// or panic if called on the server, since the server never injects).
pub trait InputSink: Send {
    fn inject_mouse_rel(&mut self, dx: i32, dy: i32);
    fn inject_mouse_button(&mut self, btn: MouseButton, state: KeyState);
    fn inject_mouse_wheel(&mut self, dx: i16, dy: i16);
    fn inject_key(&mut self, hid: u16, state: KeyState, mods: ModMask);
    fn warp_cursor_abs(&mut self, x: i32, y: i32);
    fn hide_cursor(&mut self);
    fn show_cursor(&mut self);
}

/// Source of input events on the sending side.
///
/// Implementor: macOS `CGEventTap` in M2. Returns `None` only when the
/// underlying source has been shut down; otherwise it awaits the next
/// event.
#[async_trait::async_trait]
pub trait InputSource: Send {
    async fn next_event(&mut self) -> Option<SourceEvent>;
}

/// Clipboard adapter, implemented on both sides in M8.
///
/// `next_change` returns `None` when the watcher is shutting down.
#[async_trait::async_trait]
pub trait Clipboard: Send {
    fn read_text(&self) -> Option<String>;
    fn write_text(&mut self, s: &str);
    async fn next_change(&mut self) -> Option<ClipboardEvent>;
}

// Touch `Message` so the import isn't flagged before M8 wires the
// source/sink↔wire bridge through helper functions. Removing the
// re-export entirely would force a churn-y add-back later.
#[allow(dead_code)]
fn _unused_message_marker(_: &Message) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modmask_insert_remove_contains_roundtrip() {
        let mut m = ModMask::default();
        assert!(!m.contains(ModMask::SHIFT));
        m.insert(ModMask::SHIFT);
        m.insert(ModMask::CTRL);
        assert!(m.contains(ModMask::SHIFT));
        assert!(m.contains(ModMask::CTRL));
        assert!(!m.contains(ModMask::ALT));
        // Combined contains check
        assert!(m.contains(ModMask(ModMask::SHIFT.0 | ModMask::CTRL.0)));
        m.remove(ModMask::SHIFT);
        assert!(!m.contains(ModMask::SHIFT));
        assert!(m.contains(ModMask::CTRL));
    }
}
