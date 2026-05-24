//! M7 acceptance: modifier remap end-to-end + stuck-key drain.
//!
//! Asserts the two halves of the M7 acceptance criterion (PLAN.md M7,
//! SPEC M7):
//! 1. **Cmd+C on Mac → Ctrl+C on the wire.** A `SourceEvent::Key`
//!    with `ModMask::META` set, run through the default
//!    `ModifierConfig`-derived `ModRemap` and then
//!    `source_event_to_message_remapped`, must produce a wire
//!    `KeyEvent` whose `modifiers` byte has bit 1 (Ctrl) set and
//!    bit 3 (Meta) unset.
//! 2. **SIGKILL mid-Shift-hold → no stuck Shift on Windows.** A
//!    `HeldKeys` populated with the Shift HID, run through
//!    `drain_release_actions()`, must produce a `KeyEvent { Up }`
//!    action for Shift.

use kmwarp_core::config::{Config, ModRemap};
use kmwarp_core::edge::Action;
use kmwarp_core::hid::usage;
use kmwarp_core::platform::{KeyState, ModMask, SourceEvent};
use kmwarp_core::stuck_keys::HeldKeys;
use kmwarp_core::wire::{key_state_code, source_event_to_message_remapped, Message};

// ──────────────────────────────────────────────────────────────────
// Cmd+C → Ctrl+C
// ──────────────────────────────────────────────────────────────────

#[test]
fn cmd_c_on_mac_becomes_ctrl_c_on_wire() {
    let remap = ModRemap::default();

    // 'C' is HID 0x06 (usage::C). User presses Cmd + C on the Mac, so
    // the source event has the META modifier bit set.
    let ev = SourceEvent::Key {
        hid_usage: usage::C,
        state: KeyState::Down,
        mods: ModMask::META,
    };
    let msg = source_event_to_message_remapped(ev, &remap).expect("key produces a Message");

    match msg {
        Message::KeyEvent {
            hid_usage,
            state,
            modifiers,
        } => {
            assert_eq!(hid_usage, usage::C, "HID unchanged");
            assert_eq!(state, key_state_code::DOWN);

            // The whole point: modifier byte should be CTRL (bit 1),
            // NOT META (bit 3).
            assert_eq!(
                modifiers,
                ModMask::CTRL.to_wire(),
                "Cmd+C must arrive with Ctrl modifier (got byte 0b{modifiers:04b})"
            );
            assert!(modifiers & ModMask::CTRL.0 != 0);
            assert_eq!(modifiers & ModMask::META.0, 0);
        }
        other => panic!("expected KeyEvent, got {other:?}"),
    }
}

#[test]
fn cmd_shift_z_round_trips_to_ctrl_shift_z() {
    let remap = ModRemap::default();
    let mut chord = ModMask::default();
    chord.insert(ModMask::META);
    chord.insert(ModMask::SHIFT);

    let msg = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::Z,
            state: KeyState::Down,
            mods: chord,
        },
        &remap,
    )
    .unwrap();
    let modifiers = match msg {
        Message::KeyEvent { modifiers, .. } => modifiers,
        _ => unreachable!(),
    };
    assert!(modifiers & ModMask::CTRL.0 != 0, "Ctrl bit must be set");
    assert!(modifiers & ModMask::SHIFT.0 != 0, "Shift bit must be set");
    assert_eq!(
        modifiers & ModMask::META.0,
        0,
        "Meta bit must NOT be set (Cmd was remapped)"
    );
}

#[test]
fn option_arrow_passes_through_as_alt_arrow() {
    let remap = ModRemap::default();
    let msg = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::RIGHT_ARROW,
            state: KeyState::Down,
            mods: ModMask::ALT,
        },
        &remap,
    )
    .unwrap();
    let modifiers = match msg {
        Message::KeyEvent { modifiers, .. } => modifiers,
        _ => unreachable!(),
    };
    // Option → Alt is identity in the default remap.
    assert_eq!(modifiers, ModMask::ALT.to_wire());
}

#[test]
fn non_key_events_are_unaffected_by_remap() {
    let remap = ModRemap::default();
    let m = source_event_to_message_remapped(SourceEvent::MouseRel { dx: 5, dy: -3 }, &remap);
    assert_eq!(m, Some(Message::MouseMoveRel { dx: 5, dy: -3 }));

    // CursorAt is still server-internal — None.
    let m = source_event_to_message_remapped(SourceEvent::CursorAt { x: 0, y: 0 }, &remap);
    assert_eq!(m, None);
}

#[test]
fn custom_config_swap_round_trip() {
    // User configures the opposite of the default: cmd → Meta (no
    // remap), control → Ctrl (identity). Means Cmd+C on the source
    // SHOULD arrive as Meta+C on the wire (i.e. a Windows app that
    // listens on Win+C would see it).
    let toml = r#"
        [modifiers]
        cmd = "meta"
        option = "alt"
        control = "ctrl"
        shift = "shift"
    "#;
    let cfg = Config::parse(toml).expect("parse");
    let remap = cfg.modifiers.to_remap();
    let m = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::C,
            state: KeyState::Down,
            mods: ModMask::META,
        },
        &remap,
    )
    .unwrap();
    let modifiers = match m {
        Message::KeyEvent { modifiers, .. } => modifiers,
        _ => unreachable!(),
    };
    assert_eq!(modifiers, ModMask::META.to_wire());
}

// ──────────────────────────────────────────────────────────────────
// Stuck-key drain
// ──────────────────────────────────────────────────────────────────

#[test]
fn sigkill_mid_shift_hold_drains_to_a_shift_release_action() {
    // Server's RemoteActive forwarding path inserts every Down it
    // forwards. Simulate: user held LShift, then the server is about
    // to tear down (peer Bye / RST / SIGTERM).
    let mut held = HeldKeys::new();
    held.insert(usage::LEFT_SHIFT);

    let actions = held.drain_release_actions();
    assert_eq!(actions.len(), 1);

    match &actions[0] {
        Action::SendMessage(Message::KeyEvent {
            hid_usage,
            state,
            modifiers,
        }) => {
            assert_eq!(*hid_usage, usage::LEFT_SHIFT);
            assert_eq!(*state, key_state_code::UP, "must be a release");
            assert_eq!(*modifiers, 0, "drain releases use no chord");
        }
        other => panic!("expected SendMessage(KeyEvent up), got {other:?}"),
    }
    assert!(held.is_empty());
}

#[test]
fn drain_releases_full_chord_held_under_remoteactive() {
    // User was holding Cmd+Shift+A when control transferred or the
    // session died. Each held key gets a clean release.
    let mut held = HeldKeys::new();
    held.insert(usage::LEFT_GUI); // Cmd-equivalent HID
    held.insert(usage::LEFT_SHIFT);
    held.insert(usage::A);

    let actions = held.drain_release_actions();
    assert_eq!(actions.len(), 3);

    // Every action is a SendMessage(KeyEvent{Up, modifiers=0}).
    let mut released_hids = Vec::new();
    for a in actions {
        match a {
            Action::SendMessage(Message::KeyEvent {
                hid_usage,
                state,
                modifiers,
            }) => {
                assert_eq!(state, key_state_code::UP);
                assert_eq!(modifiers, 0);
                released_hids.push(hid_usage);
            }
            other => panic!("unexpected action: {other:?}"),
        }
    }
    released_hids.sort();
    let mut expected = vec![usage::LEFT_GUI, usage::LEFT_SHIFT, usage::A];
    expected.sort();
    assert_eq!(released_hids, expected);
}

#[test]
fn drain_on_empty_held_set_is_a_noop() {
    let mut held = HeldKeys::new();
    let actions = held.drain_release_actions();
    assert!(actions.is_empty());
}

// ──────────────────────────────────────────────────────────────────
// Combined: hold a chord → remap on forward → drain on tear-down
// ──────────────────────────────────────────────────────────────────

#[test]
fn forward_with_remap_then_drain_produces_clean_release_for_held_key() {
    // Server-side simulation. User holds LShift while typing,
    // server forwards `KeyEvent{Down}` events through the remap,
    // and on tear-down drains.
    let remap = ModRemap::default();
    let mut held = HeldKeys::new();

    // 1. Shift down → forward (insert into held).
    let _ = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::LEFT_SHIFT,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        &remap,
    );
    held.insert(usage::LEFT_SHIFT);

    // 2. A down → forward.
    let _ = source_event_to_message_remapped(
        SourceEvent::Key {
            hid_usage: usage::A,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        &remap,
    );
    held.insert(usage::A);

    // 3. Connection drops. Drain.
    let actions = held.drain_release_actions();
    assert_eq!(actions.len(), 2);

    let hids: Vec<u16> = actions
        .iter()
        .map(|a| match a {
            Action::SendMessage(Message::KeyEvent { hid_usage, .. }) => *hid_usage,
            _ => panic!(),
        })
        .collect();
    let mut sorted = hids.clone();
    sorted.sort();
    assert_eq!(sorted, vec![usage::A, usage::LEFT_SHIFT]);
    assert!(held.is_empty());
}
