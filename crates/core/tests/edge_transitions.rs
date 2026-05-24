//! M6 acceptance: scripted SourceEvent sequences against the edge
//! state machine, asserting both the action sequence per event and
//! the "no stuck keys after 50 round trips" invariant.
//!
//! Tests are purely on `Action` values returned from the state
//! machine; the mock `InputSink` recorder lives in the server's
//! integration tests (it's the layer that *executes* actions).

use std::time::{Duration, Instant};

use kmwarp_core::edge::{Action, State, StateMachine, BACK_WARP_PX, EDGE_COOLDOWN};
use kmwarp_core::platform::{KeyState, ModMask, MouseButton, SourceEvent};

const MAC_SCREEN_W: u32 = 2560;

/// Helper: collect actions into a `Vec` for ergonomic asserts.
fn run(sm: &mut StateMachine, ev: SourceEvent, now: Instant) -> Vec<Action> {
    sm.on_source_event(ev, MAC_SCREEN_W, now)
        .into_iter()
        .collect()
}

fn release(sm: &mut StateMachine, exit_y: u16, now: Instant) -> Vec<Action> {
    sm.on_release_control(exit_y, MAC_SCREEN_W, now)
        .into_iter()
        .collect()
}

#[test]
fn cursor_crossing_right_edge_emits_full_transition() {
    let mut sm = StateMachine::new();
    let t = Instant::now();

    let actions = run(
        &mut sm,
        SourceEvent::CursorAt {
            x: MAC_SCREEN_W as i32 + 5, // just past the edge
            y: 720,
        },
        t,
    );

    assert_eq!(sm.state(), State::RemoteActive);
    assert_eq!(
        actions,
        vec![
            Action::SendTakeControl { entry_y: 720 },
            Action::WarpLocal {
                x: MAC_SCREEN_W as i32 - BACK_WARP_PX,
                y: 720,
            },
            Action::HideCursor,
            Action::StartSwallow,
        ]
    );
}

#[test]
fn cursor_exactly_at_edge_triggers_transition() {
    let mut sm = StateMachine::new();
    let t = Instant::now();
    let actions = run(
        &mut sm,
        SourceEvent::CursorAt {
            x: MAC_SCREEN_W as i32, // x == screen_w → cross
            y: 100,
        },
        t,
    );
    assert_eq!(sm.state(), State::RemoteActive);
    assert!(matches!(
        actions.first(),
        Some(Action::SendTakeControl { entry_y: 100 })
    ));
}

#[test]
fn cursor_inside_screen_does_not_transition() {
    let mut sm = StateMachine::new();
    let t = Instant::now();
    let actions = run(
        &mut sm,
        SourceEvent::CursorAt {
            x: MAC_SCREEN_W as i32 - 1,
            y: 100,
        },
        t,
    );
    assert!(actions.is_empty());
    assert_eq!(sm.state(), State::LocalActive);
}

#[test]
fn cooldown_blocks_immediate_re_crossing() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();

    // Cross right.
    let _ = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 100 }, t0);
    assert_eq!(sm.state(), State::RemoteActive);

    // Peer hands control back immediately (next millisecond).
    let _ = release(&mut sm, 100, t0 + Duration::from_millis(1));
    assert_eq!(sm.state(), State::LocalActive);

    // Within the 50 ms cooldown, another cross attempt is ignored.
    let actions = run(
        &mut sm,
        SourceEvent::CursorAt { x: 3000, y: 100 },
        t0 + Duration::from_millis(10),
    );
    assert!(
        actions.is_empty(),
        "edge crossing during cooldown should be ignored"
    );
    assert_eq!(sm.state(), State::LocalActive);
}

#[test]
fn after_cooldown_re_crossing_works() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();

    let _ = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 100 }, t0);
    let _ = release(&mut sm, 100, t0 + Duration::from_millis(1));

    // Past the cooldown window.
    let later = t0 + EDGE_COOLDOWN + Duration::from_millis(5);
    let actions = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 200 }, later);
    assert_eq!(sm.state(), State::RemoteActive);
    assert!(!actions.is_empty());
}

#[test]
fn release_control_emits_unwind_actions() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();

    // Transition into RemoteActive first.
    let _ = run(
        &mut sm,
        SourceEvent::CursorAt {
            x: MAC_SCREEN_W as i32 + 1,
            y: 100,
        },
        t0,
    );

    // Far future to avoid cooldown coupling.
    let t1 = t0 + EDGE_COOLDOWN + Duration::from_millis(10);
    let actions = release(&mut sm, 555, t1);

    assert_eq!(sm.state(), State::LocalActive);
    assert_eq!(
        actions,
        vec![
            Action::StopSwallow,
            Action::ShowCursor,
            Action::WarpLocal {
                x: MAC_SCREEN_W as i32 - BACK_WARP_PX,
                y: 555,
            },
        ]
    );
}

#[test]
fn release_control_in_local_active_is_noop() {
    let mut sm = StateMachine::new();
    let actions = release(&mut sm, 100, Instant::now());
    assert!(actions.is_empty());
    assert_eq!(sm.state(), State::LocalActive);
}

#[test]
fn mouse_in_local_active_is_not_forwarded() {
    let mut sm = StateMachine::new();
    for ev in [
        SourceEvent::MouseRel { dx: 5, dy: 0 },
        SourceEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        },
        SourceEvent::MouseWheel { dx: 0, dy: 1 },
    ] {
        let actions = run(&mut sm, ev, Instant::now());
        assert!(actions.is_empty(), "LocalActive should not forward mouse");
    }
}

#[test]
fn key_in_local_active_is_not_forwarded() {
    let mut sm = StateMachine::new();
    let actions = run(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0x04,
            state: KeyState::Down,
            mods: ModMask::default(),
        },
        Instant::now(),
    );
    assert!(actions.is_empty());
    // Held set must remain empty — we never started tracking.
    assert!(sm.held_keys_empty());
}

#[test]
fn mouse_in_remote_active_is_forwarded() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();
    // Force into RemoteActive.
    let _ = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 0 }, t0);

    let actions = run(
        &mut sm,
        SourceEvent::MouseRel { dx: 7, dy: -3 },
        t0 + Duration::from_millis(1),
    );
    assert_eq!(actions, vec![Action::ForwardMouseRel { dx: 7, dy: -3 }]);

    let actions = run(
        &mut sm,
        SourceEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Down,
        },
        t0 + Duration::from_millis(2),
    );
    assert_eq!(
        actions,
        vec![Action::ForwardMouseButton {
            button: MouseButton::Right,
            state: KeyState::Down,
        }]
    );

    let actions = run(
        &mut sm,
        SourceEvent::MouseWheel { dx: 0, dy: 4 },
        t0 + Duration::from_millis(3),
    );
    assert_eq!(actions, vec![Action::ForwardMouseWheel { dx: 0, dy: 4 }]);
}

#[test]
fn key_in_remote_active_tracks_held_set() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();
    let _ = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 0 }, t0);

    // Shift down + A down.
    let _ = run(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0xE1,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        t0 + Duration::from_millis(1),
    );
    let _ = run(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0x04,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        t0 + Duration::from_millis(2),
    );
    assert!(!sm.held_keys_empty(), "two keys should be held");

    // A up.
    let _ = run(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0x04,
            state: KeyState::Up,
            mods: ModMask::SHIFT,
        },
        t0 + Duration::from_millis(3),
    );
    // Shift still held.
    assert!(!sm.held_keys_empty());

    // Shift up.
    let _ = run(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0xE1,
            state: KeyState::Up,
            mods: ModMask::default(),
        },
        t0 + Duration::from_millis(4),
    );
    assert!(sm.held_keys_empty(), "all keys released");
}

#[test]
fn release_control_drains_held_keys_before_giving_up_control() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();
    let _ = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 0 }, t0);

    // User holds Shift+A and then control goes back to the Mac.
    let _ = run(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0xE1,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        t0 + Duration::from_millis(1),
    );
    let _ = run(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0x04,
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        t0 + Duration::from_millis(2),
    );

    let t1 = t0 + EDGE_COOLDOWN + Duration::from_millis(10);
    let actions = release(&mut sm, 100, t1);

    // First N actions must be ForwardKey{Up} for each held key (order
    // may vary because HashSet drain isn't ordered); after that the
    // standard unwind sequence appears.
    let key_ups: Vec<&Action> = actions
        .iter()
        .filter(|a| {
            matches!(
                a,
                Action::ForwardKey {
                    state: KeyState::Up,
                    ..
                }
            )
        })
        .collect();
    assert_eq!(key_ups.len(), 2, "exactly two keys to release");

    // Tail must be StopSwallow → ShowCursor → WarpLocal.
    assert!(matches!(actions[actions.len() - 3], Action::StopSwallow));
    assert!(matches!(actions[actions.len() - 2], Action::ShowCursor));
    assert!(matches!(
        actions[actions.len() - 1],
        Action::WarpLocal { .. }
    ));

    assert!(sm.held_keys_empty(), "drain must leave held set empty");
    assert_eq!(sm.state(), State::LocalActive);
}

#[test]
fn drain_held_keys_helper_returns_releases_for_each_held() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();
    let _ = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 0 }, t0);
    for hid in [0xE1u16, 0x04, 0x16] {
        let _ = run(
            &mut sm,
            SourceEvent::Key {
                hid_usage: hid,
                state: KeyState::Down,
                mods: ModMask::default(),
            },
            t0 + Duration::from_millis(1),
        );
    }
    assert_eq!(sm.drain_held_keys().len(), 3);
    assert!(sm.held_keys_empty());
}

/// **The M6 acceptance test, spec verbatim:**
/// > No stuck keys after 50 round trips.
///
/// Scripted scenario: 50 times, cross the edge, hold a key,
/// receive ReleaseControl, assert no held keys leak across
/// transitions and the state returns to LocalActive each time.
#[test]
fn no_stuck_keys_after_50_round_trips() {
    let mut sm = StateMachine::new();
    let mut t = Instant::now();

    for round in 0..50 {
        // Skip past cooldown so the next cross is allowed (after the
        // previous round's release set last_transition).
        t += EDGE_COOLDOWN + Duration::from_millis(1);

        // Cross right.
        let actions = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 100 }, t);
        assert!(
            !actions.is_empty(),
            "round {round}: cross should transition"
        );
        assert_eq!(sm.state(), State::RemoteActive);
        t += Duration::from_millis(1);

        // Hold Shift + a random letter.
        let _ = run(
            &mut sm,
            SourceEvent::Key {
                hid_usage: 0xE1,
                state: KeyState::Down,
                mods: ModMask::SHIFT,
            },
            t,
        );
        t += Duration::from_millis(1);
        let _ = run(
            &mut sm,
            SourceEvent::Key {
                hid_usage: 0x04 + (round as u16 % 26),
                state: KeyState::Down,
                mods: ModMask::SHIFT,
            },
            t,
        );
        t += Duration::from_millis(1);

        // Peer hands control back. Drain MUST release both held keys.
        t += EDGE_COOLDOWN + Duration::from_millis(1); // skip cooldown
        let actions = release(&mut sm, 100, t);

        let key_ups = actions
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::ForwardKey {
                        state: KeyState::Up,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            key_ups, 2,
            "round {round}: expected exactly 2 ForwardKey{{Up}} on release"
        );
        assert!(
            sm.held_keys_empty(),
            "round {round}: held set must be empty after release"
        );
        assert_eq!(sm.state(), State::LocalActive);

        t += Duration::from_millis(1);
    }
}

#[test]
fn cursor_at_in_remote_active_is_ignored() {
    let mut sm = StateMachine::new();
    let t0 = Instant::now();
    // Cross.
    let _ = run(&mut sm, SourceEvent::CursorAt { x: 3000, y: 0 }, t0);
    // CursorAt arriving while remote is active should be a no-op.
    let actions = run(
        &mut sm,
        SourceEvent::CursorAt { x: 100, y: 100 },
        t0 + Duration::from_millis(1),
    );
    assert!(actions.is_empty());
    assert_eq!(sm.state(), State::RemoteActive);
}

#[test]
fn transition_actions_are_in_correct_order() {
    // Spec requires SendTakeControl first (so peer warps before any
    // forwarded motion arrives), then WarpLocal, then HideCursor,
    // then StartSwallow.
    let mut sm = StateMachine::new();
    let actions = run(
        &mut sm,
        SourceEvent::CursorAt { x: 3000, y: 100 },
        Instant::now(),
    );
    assert!(matches!(actions[0], Action::SendTakeControl { .. }));
    assert!(matches!(actions[1], Action::WarpLocal { .. }));
    assert!(matches!(actions[2], Action::HideCursor));
    assert!(matches!(actions[3], Action::StartSwallow));
}
