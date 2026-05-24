//! Translation helpers between platform types and wire `Message`s.
//!
//! These functions deliberately live in `core` (not in either binary) so
//! both sides agree on byte values for `MouseButton.button` /
//! `MouseButton.state`. The spec table only specifies the field as `u8`;
//! [`mouse_button_to_byte`] / [`byte_to_mouse_button`] pin the convention
//! and [`source_event_to_message`] / [`apply_mouse_to_sink`] route through
//! them so server and client cannot drift.
//!
//! Wire convention for mouse deltas, per PLAN.md §M4 risks: **physical
//! pixels of the server screen.** Each platform layer is responsible for
//! converting its native units to/from this convention; the helpers here
//! are byte-pure (i.e. they do not do unit conversion).

use crate::modmap::ModRemap;
use crate::platform::{InputSink, KeyState, ModMask, MouseButton, SourceEvent};
use crate::wire::{key_state_code, mouse_button_code, Message};

/// Wire byte → [`MouseButton`]. Returns `None` on unrecognized values so
/// the injector can warn-and-drop rather than panic on a malformed peer.
pub fn byte_to_mouse_button(b: u8) -> Option<MouseButton> {
    match b {
        mouse_button_code::LEFT => Some(MouseButton::Left),
        mouse_button_code::RIGHT => Some(MouseButton::Right),
        mouse_button_code::MIDDLE => Some(MouseButton::Middle),
        mouse_button_code::X1 => Some(MouseButton::X1),
        mouse_button_code::X2 => Some(MouseButton::X2),
        _ => None,
    }
}

/// [`MouseButton`] → wire byte. Total — every variant maps to a code.
pub fn mouse_button_to_byte(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => mouse_button_code::LEFT,
        MouseButton::Right => mouse_button_code::RIGHT,
        MouseButton::Middle => mouse_button_code::MIDDLE,
        MouseButton::X1 => mouse_button_code::X1,
        MouseButton::X2 => mouse_button_code::X2,
    }
}

/// Wire byte → [`KeyState`]. Returns `None` on unrecognized values.
pub fn byte_to_key_state(b: u8) -> Option<KeyState> {
    match b {
        key_state_code::UP => Some(KeyState::Up),
        key_state_code::DOWN => Some(KeyState::Down),
        _ => None,
    }
}

/// [`KeyState`] → wire byte.
pub fn key_state_to_byte(s: KeyState) -> u8 {
    match s {
        KeyState::Up => key_state_code::UP,
        KeyState::Down => key_state_code::DOWN,
    }
}

/// Convert an `InputSource`-emitted [`SourceEvent`] into the wire
/// [`Message`] that the server should forward to its peer.
///
/// Returns `None` for `SourceEvent::CursorAt` — that's a server-internal
/// signal for the M6 edge state machine, never on the wire. Also returns
/// `None` for any future variant we don't yet route (caller treats `None`
/// as "drop silently").
///
/// **Modifier remap is NOT applied here** — the modifier byte goes out
/// as-is. The M6 state machine calls this from RemoteActive; the M7
/// server runtime that owns the state machine should call
/// [`source_event_to_message_remapped`] instead so the configured
/// `[modifiers]` mapping (Cmd→Ctrl by default) takes effect.
pub fn source_event_to_message(ev: SourceEvent) -> Option<Message> {
    match ev {
        SourceEvent::MouseRel { dx, dy } => Some(Message::MouseMoveRel { dx, dy }),
        SourceEvent::MouseButton { button, state } => Some(Message::MouseButton {
            button: mouse_button_to_byte(button),
            state: key_state_to_byte(state),
        }),
        SourceEvent::MouseWheel { dx, dy } => Some(Message::MouseWheel { dx, dy }),
        SourceEvent::Key {
            hid_usage,
            state,
            mods,
        } => Some(Message::KeyEvent {
            hid_usage,
            state: key_state_to_byte(state),
            modifiers: mods.to_wire(),
        }),
        SourceEvent::CursorAt { .. } => None,
    }
}

/// Like [`source_event_to_message`] but applies a [`ModRemap`] to the
/// HID code **and** the `mods` byte of `SourceEvent::Key` events.
/// Mouse / wheel / button variants pass through unchanged.
///
/// This is the M7 hook the server runtime should call from its
/// RemoteActive forwarding path. With default remap (`cmd → Ctrl`):
/// - Pressing the Cmd key emits a wire `KeyEvent { hid: LCtrl, mods: 0 }`
///   (HID-level remap so the receiver sees a Ctrl press, not a Win press).
/// - Pressing `C` while holding Cmd emits `KeyEvent { hid: C, mods: CTRL }`
///   (chord-level remap on the `mods` byte).
pub fn source_event_to_message_remapped(ev: SourceEvent, remap: &ModRemap) -> Option<Message> {
    match ev {
        SourceEvent::Key {
            hid_usage,
            state,
            mods,
        } => Some(Message::KeyEvent {
            hid_usage: remap.apply_to_hid(hid_usage),
            state: key_state_to_byte(state),
            modifiers: remap.apply_to_modmask(mods).to_wire(),
        }),
        // Non-key events are unaffected by modifier remap; delegate.
        other => source_event_to_message(other),
    }
}

/// Apply a `KeyEvent` wire `Message` to an [`InputSink`].
///
/// Returns `true` iff the message was a `KeyEvent` and dispatch was
/// attempted. Caller uses the `false` return to fall through to non-key
/// dispatch (mouse, clipboard, edge).
///
/// Wire-byte → enum conversions are tolerant: an unknown `state` byte
/// logs a `warn!` and drops the event rather than panicking. The
/// `modifiers` byte goes through [`ModMask::from_wire`] so reserved bits
/// 4-7 are silently dropped — a misbehaving peer can't smuggle data
/// through them.
pub fn apply_key_to_sink<S: InputSink>(msg: &Message, sink: &mut S) -> bool {
    match msg {
        Message::KeyEvent {
            hid_usage,
            state,
            modifiers,
        } => {
            match byte_to_key_state(*state) {
                Some(st) => sink.inject_key(*hid_usage, st, ModMask::from_wire(*modifiers)),
                None => tracing::warn!(
                    hid_usage,
                    state = *state,
                    "dropping KeyEvent with unknown state byte"
                ),
            }
            true
        }
        _ => false,
    }
}

/// Apply a mouse-shaped wire `Message` to an [`InputSink`].
///
/// Returns `true` iff the message was a mouse variant and dispatch was
/// attempted. Caller uses the `false` return to fall through to non-mouse
/// dispatch (keyboard in M5, clipboard in M8, edge in M6).
///
/// Wire-byte → enum conversions are tolerant: an unknown `button` or
/// `state` byte logs a `warn!` and drops the event rather than panicking,
/// because a single malformed frame should not tear down the connection
/// (the codec already accepts the bytes as well-formed; this is a
/// higher-level dictionary check).
pub fn apply_mouse_to_sink<S: InputSink>(msg: &Message, sink: &mut S) -> bool {
    match msg {
        Message::MouseMoveRel { dx, dy } => {
            sink.inject_mouse_rel(i32::from(*dx), i32::from(*dy));
            true
        }
        Message::MouseButton { button, state } => {
            match (byte_to_mouse_button(*button), byte_to_key_state(*state)) {
                (Some(btn), Some(st)) => sink.inject_mouse_button(btn, st),
                _ => tracing::warn!(
                    button = *button,
                    state = *state,
                    "dropping MouseButton with unknown button/state byte"
                ),
            }
            true
        }
        Message::MouseWheel { dx, dy } => {
            sink.inject_mouse_wheel(*dx, *dy);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{InputSink, KeyState, ModMask, MouseButton};

    /// Recording sink used for the dispatch-side tests.
    #[derive(Default)]
    struct RecorderSink {
        rel: Vec<(i32, i32)>,
        buttons: Vec<(MouseButton, KeyState)>,
        wheels: Vec<(i16, i16)>,
        keys: Vec<(u16, KeyState, ModMask)>,
    }

    impl InputSink for RecorderSink {
        fn inject_mouse_rel(&mut self, dx: i32, dy: i32) {
            self.rel.push((dx, dy));
        }
        fn inject_mouse_button(&mut self, btn: MouseButton, state: KeyState) {
            self.buttons.push((btn, state));
        }
        fn inject_mouse_wheel(&mut self, dx: i16, dy: i16) {
            self.wheels.push((dx, dy));
        }
        fn inject_key(&mut self, h: u16, s: KeyState, m: ModMask) {
            self.keys.push((h, s, m));
        }
        fn warp_cursor_abs(&mut self, _x: i32, _y: i32) {}
        fn hide_cursor(&mut self) {}
        fn show_cursor(&mut self) {}
    }

    #[test]
    fn button_codes_roundtrip() {
        for btn in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::X1,
            MouseButton::X2,
        ] {
            assert_eq!(byte_to_mouse_button(mouse_button_to_byte(btn)), Some(btn));
        }
        // Unknown byte → None.
        assert_eq!(byte_to_mouse_button(99), None);
    }

    #[test]
    fn key_state_codes_roundtrip() {
        for s in [KeyState::Up, KeyState::Down] {
            assert_eq!(byte_to_key_state(key_state_to_byte(s)), Some(s));
        }
        assert_eq!(byte_to_key_state(2), None);
    }

    #[test]
    fn source_event_translates_mouse_variants() {
        assert_eq!(
            source_event_to_message(SourceEvent::MouseRel { dx: -3, dy: 7 }),
            Some(Message::MouseMoveRel { dx: -3, dy: 7 })
        );
        assert_eq!(
            source_event_to_message(SourceEvent::MouseButton {
                button: MouseButton::Middle,
                state: KeyState::Down,
            }),
            Some(Message::MouseButton {
                button: mouse_button_code::MIDDLE,
                state: key_state_code::DOWN,
            })
        );
        assert_eq!(
            source_event_to_message(SourceEvent::MouseWheel { dx: 1, dy: -1 }),
            Some(Message::MouseWheel { dx: 1, dy: -1 })
        );
        // CursorAt is server-internal.
        assert_eq!(
            source_event_to_message(SourceEvent::CursorAt { x: 100, y: 200 }),
            None
        );
    }

    #[test]
    fn apply_mouse_dispatches_each_variant() {
        let mut sink = RecorderSink::default();

        assert!(apply_mouse_to_sink(
            &Message::MouseMoveRel { dx: 4, dy: -2 },
            &mut sink
        ));
        assert!(apply_mouse_to_sink(
            &Message::MouseButton {
                button: mouse_button_code::RIGHT,
                state: key_state_code::DOWN,
            },
            &mut sink
        ));
        assert!(apply_mouse_to_sink(
            &Message::MouseWheel { dx: 0, dy: 3 },
            &mut sink
        ));
        // Non-mouse → false, sink untouched.
        assert!(!apply_mouse_to_sink(
            &Message::Heartbeat { seq: 1 },
            &mut sink
        ));

        assert_eq!(sink.rel, vec![(4, -2)]);
        assert_eq!(sink.buttons, vec![(MouseButton::Right, KeyState::Down)]);
        assert_eq!(sink.wheels, vec![(0, 3)]);
    }

    #[test]
    fn apply_key_dispatches_keyevent() {
        let mut sink = RecorderSink::default();

        // hid=0x04 (A), down, with Shift+Ctrl modifiers
        let mods_byte = ModMask::SHIFT.0 | ModMask::CTRL.0;
        assert!(apply_key_to_sink(
            &Message::KeyEvent {
                hid_usage: 0x04,
                state: key_state_code::DOWN,
                modifiers: mods_byte,
            },
            &mut sink
        ));
        // Up event without modifiers.
        assert!(apply_key_to_sink(
            &Message::KeyEvent {
                hid_usage: 0x04,
                state: key_state_code::UP,
                modifiers: 0,
            },
            &mut sink
        ));
        // Non-key → false, sink untouched.
        assert!(!apply_key_to_sink(
            &Message::Heartbeat { seq: 1 },
            &mut sink
        ));

        assert_eq!(
            sink.keys,
            vec![
                (0x04, KeyState::Down, ModMask(mods_byte)),
                (0x04, KeyState::Up, ModMask(0)),
            ]
        );
    }

    #[test]
    fn apply_key_drops_unknown_state_byte() {
        let mut sink = RecorderSink::default();
        assert!(apply_key_to_sink(
            &Message::KeyEvent {
                hid_usage: 0x04,
                state: 99, // not Up or Down
                modifiers: 0,
            },
            &mut sink
        ));
        assert!(sink.keys.is_empty());
    }

    #[test]
    fn source_event_translates_key_variant() {
        let mods = ModMask::SHIFT;
        assert_eq!(
            source_event_to_message(SourceEvent::Key {
                hid_usage: 0x16, // 'S'
                state: KeyState::Down,
                mods,
            }),
            Some(Message::KeyEvent {
                hid_usage: 0x16,
                state: key_state_code::DOWN,
                modifiers: mods.0,
            })
        );
    }

    #[test]
    fn apply_mouse_drops_unknown_button_byte() {
        let mut sink = RecorderSink::default();
        // Unknown button byte: dispatch is "true" (we recognized this as a
        // MouseButton frame), but the sink is not called.
        assert!(apply_mouse_to_sink(
            &Message::MouseButton {
                button: 99,
                state: key_state_code::DOWN,
            },
            &mut sink
        ));
        assert!(sink.buttons.is_empty());
    }
}
