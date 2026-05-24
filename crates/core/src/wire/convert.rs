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

use crate::platform::{InputSink, KeyState, MouseButton, SourceEvent};
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
            modifiers: mods.0,
        }),
        SourceEvent::CursorAt { .. } => None,
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
        fn inject_key(&mut self, _h: u16, _s: KeyState, _m: ModMask) {}
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
