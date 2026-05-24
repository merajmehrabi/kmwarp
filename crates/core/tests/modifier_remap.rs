//! M7 acceptance: modifier remap end-to-end + stuck-key drain.
//!
//! Asserts the two halves of the M7 acceptance criterion (PLAN.md M7,
//! SPEC M7):
//! 1. **Cmd+C on Mac → Ctrl+C on the wire.** Two flavours: a bare
//!    Cmd press (modifier KeyEvent's HID is remapped) and a Cmd+C
//!    chord (the C frame's `modifiers` byte is remapped).
//! 2. **SIGKILL mid-Shift-hold → no stuck Shift on Windows.**
//!    `HeldKeys::drain` populated with Shift returns `[LSHIFT]`,
//!    leaving the tracker empty.

use kmwarp_core::config::Config;
use kmwarp_core::hid::usage;
use kmwarp_core::modmap::{ModRemap, ModTarget};
use kmwarp_core::platform::{KeyState, ModMask, SourceEvent};
use kmwarp_core::stuck_keys::HeldKeys;
use kmwarp_core::wire::{key_state_code, source_event_to_message_remapped, Message};

// ──────────────────────────────────────────────────────────────────
// Cmd+C → Ctrl+C
// ──────────────────────────────────────────────────────────────────

#[test]
fn cmd_keypress_becomes_ctrl_keypress_on_wire() {
    // Default remap: cmd → Ctrl. Pressing the LEFT Cmd key on the
    // Mac should emit a KeyEvent carrying the LCtrl HID, not LGUI.
    let remap = ModRemap::default();
    let ev = SourceEvent::Key {
        hid_usage: usage::LEFT_GUI,
        state: KeyState::Down,
        mods: ModMask::META,
    };
    let msg = source_event_to_message_remapped(ev, &remap).expect("Key → Message");
    match msg {
        Message::KeyEvent {
            hid_usage,
            state,
            modifiers,
        } => {
            assert_eq!(hid_usage, usage::LEFT_CTRL, "HID must be LCtrl after remap");
            assert_eq!(state, key_state_code::DOWN);
            // The modifier byte should also be remapped: META → CTRL.
            assert_eq!(modifiers, ModMask::CTRL.to_wire());
        }
        other => panic!("unexpected message: {other:?}"),
    }
}

#[test]
fn cmd_plus_c_chord_becomes_ctrl_plus_c() {
    // Pressing C while Cmd is held: the C event has hid=C and
    // mods=META. The wire frame should carry hid=C (unchanged) and
    // modifiers=CTRL.
    let remap = ModRemap::default();
    let msg = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::C,
            state: KeyState::Down,
            mods: ModMask::META,
        },
        &remap,
    )
    .unwrap();
    match msg {
        Message::KeyEvent {
            hid_usage,
            state,
            modifiers,
        } => {
            assert_eq!(hid_usage, usage::C);
            assert_eq!(state, key_state_code::DOWN);
            assert_eq!(modifiers & ModMask::CTRL.0, ModMask::CTRL.0);
            assert_eq!(modifiers & ModMask::META.0, 0);
        }
        _ => unreachable!(),
    }
}

#[test]
fn right_cmd_remaps_to_right_ctrl() {
    let remap = ModRemap::default();
    let msg = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::RIGHT_GUI,
            state: KeyState::Up,
            mods: ModMask::default(),
        },
        &remap,
    )
    .unwrap();
    let hid = match msg {
        Message::KeyEvent { hid_usage, .. } => hid_usage,
        _ => unreachable!(),
    };
    assert_eq!(hid, usage::RIGHT_CTRL);
}

#[test]
fn option_passes_through_to_alt() {
    // Default remap: option → Alt (identity in terms of HID, since
    // macOS Option already IS HID Alt).
    let remap = ModRemap::default();
    let msg = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::LEFT_ALT,
            state: KeyState::Down,
            mods: ModMask::ALT,
        },
        &remap,
    )
    .unwrap();
    match msg {
        Message::KeyEvent {
            hid_usage,
            modifiers,
            ..
        } => {
            assert_eq!(hid_usage, usage::LEFT_ALT);
            assert_eq!(modifiers, ModMask::ALT.to_wire());
        }
        _ => unreachable!(),
    }
}

#[test]
fn non_key_events_are_unaffected_by_remap() {
    let remap = ModRemap::default();
    assert_eq!(
        source_event_to_message_remapped(SourceEvent::MouseRel { dx: 5, dy: -3 }, &remap),
        Some(Message::MouseMoveRel { dx: 5, dy: -3 })
    );
    assert_eq!(
        source_event_to_message_remapped(SourceEvent::CursorAt { x: 0, y: 0 }, &remap),
        None
    );
}

#[test]
fn custom_remap_from_toml_swaps_cmd_to_alt() {
    let toml = r#"
        [modifiers]
        cmd = "alt"
        option = "ctrl"
    "#;
    let cfg = Config::parse(toml).expect("parse");
    assert_eq!(cfg.modifiers.cmd, ModTarget::Alt);
    let msg = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::LEFT_GUI,
            state: KeyState::Down,
            mods: ModMask::META,
        },
        &cfg.modifiers,
    )
    .unwrap();
    let hid = match msg {
        Message::KeyEvent { hid_usage, .. } => hid_usage,
        _ => unreachable!(),
    };
    assert_eq!(hid, usage::LEFT_ALT);
}

// ──────────────────────────────────────────────────────────────────
// Stuck-key drain
// ──────────────────────────────────────────────────────────────────

#[test]
fn sigkill_mid_shift_hold_drains_to_a_shift_release() {
    // Server's RemoteActive forwarding observed Shift Down. Connection
    // tears down. `drain()` should return [LSHIFT].
    let mut held = HeldKeys::new();
    held.observe(usage::LEFT_SHIFT, KeyState::Down);
    let drained = held.drain();
    assert_eq!(drained, vec![usage::LEFT_SHIFT]);
    assert!(held.is_empty());
}

#[test]
fn drain_returns_full_chord_in_sorted_order() {
    let mut held = HeldKeys::new();
    held.observe(usage::A, KeyState::Down);
    held.observe(usage::LEFT_SHIFT, KeyState::Down);
    held.observe(usage::LEFT_GUI, KeyState::Down);
    held.observe(usage::Z, KeyState::Down);

    let drained = held.drain();
    // BTreeSet ordering: ascending by HID code.
    assert_eq!(
        drained,
        vec![usage::A, usage::Z, usage::LEFT_SHIFT, usage::LEFT_GUI]
    );
    assert!(held.is_empty());
}

#[test]
fn observe_up_removes_from_held_set() {
    let mut held = HeldKeys::new();
    held.observe(usage::LEFT_SHIFT, KeyState::Down);
    held.observe(usage::A, KeyState::Down);
    assert_eq!(held.len(), 2);

    held.observe(usage::LEFT_SHIFT, KeyState::Up);
    assert_eq!(held.len(), 1);
    assert!(!held.is_held(usage::LEFT_SHIFT));
    assert!(held.is_held(usage::A));
}

// ──────────────────────────────────────────────────────────────────
// Combined: forward chord with remap, then drain on tear-down
// ──────────────────────────────────────────────────────────────────

#[test]
fn forward_with_remap_then_drain_produces_clean_release_hids() {
    let remap = ModRemap::default();
    let mut held = HeldKeys::new();

    // Shift down + A down — server forwards, tracker records.
    let _ = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::LEFT_SHIFT,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        &remap,
    );
    held.observe(usage::LEFT_SHIFT, KeyState::Down);

    let _ = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::A,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        &remap,
    );
    held.observe(usage::A, KeyState::Down);

    // Connection drops. The runtime drains and synthesizes KeyEvent{Up}.
    let drained = held.drain();
    assert_eq!(drained, vec![usage::A, usage::LEFT_SHIFT]);
    assert!(held.is_empty());
}
