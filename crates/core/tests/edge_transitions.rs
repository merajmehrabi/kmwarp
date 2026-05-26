//! M6 acceptance: scripted SourceEvent / wire-Message sequences
//! against the pure edge state machine.
//!
//! Tests assert directly on the returned `Action` vector; the executor
//! / sink layer lives in the server crate.

use kmwarp_core::edge::{Action, EdgeConfig, State, StateMachine};
use kmwarp_core::platform::{KeyState, ModMask, MouseButton, SourceEvent};
use kmwarp_core::wire::{key_state_code, mouse_button_code, Message};

/// Standard config used by most tests. Local 1920×1080, default
/// cooldown 50 ms, back-warp 5 px.
fn default_cfg() -> EdgeConfig {
    EdgeConfig::default()
}

/// Collect into a `Vec` for ergonomic asserts.
fn local(sm: &mut StateMachine, ev: SourceEvent, now_ms: u64) -> Vec<Action> {
    sm.on_local_event(ev, now_ms).into_iter().collect()
}

fn wire(sm: &mut StateMachine, msg: Message, now_ms: u64) -> Vec<Action> {
    sm.on_wire_message(&msg, now_ms).into_iter().collect()
}

// ──────────────────────────────────────────────────────────────────
// 1. starts_in_local_active
// ──────────────────────────────────────────────────────────────────
#[test]
fn starts_in_local_active() {
    let sm = StateMachine::new(default_cfg());
    assert_eq!(sm.state(), State::LocalActive);
}

// ──────────────────────────────────────────────────────────────────
// 2. local_to_remote_on_right_edge
// ──────────────────────────────────────────────────────────────────
#[test]
fn local_to_remote_on_right_edge() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);

    let actions = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32, // exactly at edge — IS a crossing per spec
            y: 500,
        },
        100,
    );

    assert_eq!(sm.state(), State::RemoteActive);
    assert_eq!(
        actions,
        vec![
            Action::SendTakeControl { entry_y: 500 },
            Action::WarpLocalCursor {
                x: cfg.local_screen_w as i32 - cfg.back_warp_px,
                y: 500,
            },
            Action::HideLocalCursor,
            Action::StartSwallow,
        ]
    );
}

#[test]
fn cursor_past_edge_also_transitions() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 25, // past the edge
            y: 100,
        },
        100,
    );
    assert_eq!(sm.state(), State::RemoteActive);
}

// ──────────────────────────────────────────────────────────────────
// 3. mouse_in_remote_forwards
// ──────────────────────────────────────────────────────────────────
#[test]
fn mouse_in_remote_forwards() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    // Force into Remote.
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        100,
    );
    assert_eq!(sm.state(), State::RemoteActive);

    // Mouse motion forwards as a SendMessage(Message::MouseMoveRel).
    let actions = local(&mut sm, SourceEvent::MouseRel { dx: 10, dy: 5 }, 150);
    assert_eq!(
        actions,
        vec![Action::SendMessage(Message::MouseMoveRel { dx: 10, dy: 5 })]
    );
}

#[test]
fn mouse_button_and_wheel_forward_via_send_message() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        100,
    );

    let actions = local(
        &mut sm,
        SourceEvent::MouseButton {
            button: MouseButton::Right,
            state: KeyState::Down,
        },
        150,
    );
    assert_eq!(
        actions,
        vec![Action::SendMessage(Message::MouseButton {
            button: mouse_button_code::RIGHT,
            state: key_state_code::DOWN,
        })]
    );

    let actions = local(&mut sm, SourceEvent::MouseWheel { dx: 0, dy: 3 }, 160);
    assert_eq!(
        actions,
        vec![Action::SendMessage(Message::MouseWheel { dx: 0, dy: 3 })]
    );
}

#[test]
fn key_forwards_via_send_message_in_remote() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        100,
    );

    let actions = local(
        &mut sm,
        SourceEvent::Key {
            hid_usage: 0x04, // 'A'
            state: KeyState::Down,
            mods: ModMask::SHIFT,
        },
        150,
    );
    assert_eq!(
        actions,
        vec![Action::SendMessage(Message::KeyEvent {
            hid_usage: 0x04,
            state: key_state_code::DOWN,
            modifiers: ModMask::SHIFT.to_wire(),
        })]
    );
}

// ──────────────────────────────────────────────────────────────────
// 4. release_control_returns_to_local
// ──────────────────────────────────────────────────────────────────
#[test]
fn release_control_returns_to_local() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        100,
    );

    // Past cooldown so the release isn't dropped.
    let t = 100 + u64::from(cfg.thrash_cooldown_ms) + 10;
    let actions = wire(&mut sm, Message::ReleaseControl { exit_y: 700 }, t);

    assert_eq!(sm.state(), State::LocalActive);
    assert_eq!(
        actions,
        vec![
            Action::StopSwallow,
            Action::ShowLocalCursor,
            Action::WarpLocalCursor {
                x: cfg.local_screen_w as i32 - 1,
                y: 700,
            },
        ]
    );
}

#[test]
fn release_control_in_local_active_is_noop() {
    let mut sm = StateMachine::new(default_cfg());
    let actions = wire(&mut sm, Message::ReleaseControl { exit_y: 100 }, 1000);
    assert!(actions.is_empty());
    assert_eq!(sm.state(), State::LocalActive);
}

// ──────────────────────────────────────────────────────────────────
// 5. thrash_cooldown_blocks_rapid_transitions
// ──────────────────────────────────────────────────────────────────
#[test]
fn thrash_cooldown_blocks_rapid_transitions() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);

    // Drive into Remote at t=100.
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        100,
    );
    assert_eq!(sm.state(), State::RemoteActive);

    // Release at t=120 — only 20 ms gap. R→L is peer-initiated by an
    // explicit ReleaseControl (cursor_watch fires once then disables
    // itself), so it must ALWAYS land — dropping it would strand the SM
    // in RemoteActive forever. Cooldown intentionally does NOT block this
    // direction.
    let actions = wire(&mut sm, Message::ReleaseControl { exit_y: 100 }, 120);
    assert!(!actions.is_empty(), "release lands regardless of cooldown");
    assert_eq!(sm.state(), State::LocalActive);

    // Now back in LocalActive at t=120. The L→R cooldown still applies:
    // within 50 ms of the release, a crossing is ignored.
    let actions = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        140,
    );
    assert!(actions.is_empty(), "cross within cooldown is ignored");
    assert_eq!(sm.state(), State::LocalActive);

    // After the cooldown clears, the same event transitions.
    let actions = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        260,
    );
    assert!(!actions.is_empty(), "cross after cooldown transitions");
    assert_eq!(sm.state(), State::RemoteActive);
}

// ──────────────────────────────────────────────────────────────────
// 6. fifty_round_trips_no_panic
// ──────────────────────────────────────────────────────────────────
#[test]
fn fifty_round_trips_no_panic() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    let mut now: u64 = 0;
    let cool = u64::from(cfg.thrash_cooldown_ms) + 1;

    for round in 0..50 {
        now += cool;

        // Cross right → Remote.
        let cross_actions = local(
            &mut sm,
            SourceEvent::CursorAt {
                x: cfg.local_screen_w as i32 + 1,
                y: 100,
            },
            now,
        );
        assert!(
            !cross_actions.is_empty(),
            "round {round}: cross should transition"
        );
        assert_eq!(sm.state(), State::RemoteActive);

        // Action sequence shape is the same every round:
        assert!(matches!(cross_actions[0], Action::SendTakeControl { .. }));
        assert!(matches!(cross_actions[1], Action::WarpLocalCursor { .. }));
        assert!(matches!(cross_actions[2], Action::HideLocalCursor));
        assert!(matches!(cross_actions[3], Action::StartSwallow));

        now += cool;

        // Release → Local.
        let release_actions = wire(&mut sm, Message::ReleaseControl { exit_y: 100 }, now);
        assert!(
            !release_actions.is_empty(),
            "round {round}: release should transition"
        );
        assert_eq!(sm.state(), State::LocalActive);

        assert!(matches!(release_actions[0], Action::StopSwallow));
        assert!(matches!(release_actions[1], Action::ShowLocalCursor));
        assert!(matches!(release_actions[2], Action::WarpLocalCursor { .. }));
    }
}

// ──────────────────────────────────────────────────────────────────
// 7. cursor_below_edge_in_local_does_nothing
// ──────────────────────────────────────────────────────────────────
#[test]
fn cursor_below_edge_in_local_does_nothing() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);

    let actions = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 - 1, // one pixel inside
            y: 500,
        },
        100,
    );

    assert!(actions.is_empty());
    assert_eq!(sm.state(), State::LocalActive);
}

// ──────────────────────────────────────────────────────────────────
// Extra safety nets
// ──────────────────────────────────────────────────────────────────

#[test]
fn cursor_at_in_remote_is_ignored_but_updates_last_cursor() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    // Drive into Remote.
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        100,
    );

    // A spurious CursorAt while remote should emit no actions.
    let actions = local(&mut sm, SourceEvent::CursorAt { x: 100, y: 200 }, 150);
    assert!(actions.is_empty());
    assert_eq!(sm.state(), State::RemoteActive);
}

#[test]
fn mouse_and_key_in_local_active_are_dropped() {
    let mut sm = StateMachine::new(default_cfg());
    for ev in [
        SourceEvent::MouseRel { dx: 5, dy: 0 },
        SourceEvent::MouseButton {
            button: MouseButton::Left,
            state: KeyState::Down,
        },
        SourceEvent::MouseWheel { dx: 0, dy: 1 },
        SourceEvent::Key {
            hid_usage: 0x04,
            state: KeyState::Down,
            mods: ModMask::default(),
        },
    ] {
        let actions = local(&mut sm, ev, 100);
        assert!(actions.is_empty(), "LocalActive should forward nothing");
    }
    assert_eq!(sm.state(), State::LocalActive);
}

#[test]
fn non_release_wire_messages_are_ignored() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    // Drive into Remote.
    let _ = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 0,
        },
        100,
    );

    let irrelevant = [
        Message::Heartbeat { seq: 1 },
        Message::Bye { reason_code: 0 },
        Message::EchoPing { ts_ns: 0 },
        Message::HelloAck {
            accepted: true,
            server_screen_px: (1, 1),
        },
        Message::MouseMoveRel { dx: 0, dy: 0 }, // mouse from peer — not our concern
    ];
    for msg in irrelevant {
        let actions = wire(&mut sm, msg, 200);
        assert!(actions.is_empty(), "non-release wire msgs ignored");
    }
    assert_eq!(sm.state(), State::RemoteActive);
}

#[test]
fn entry_y_clamps_to_u16() {
    let cfg = default_cfg();
    let mut sm = StateMachine::new(cfg);
    // y above u16::MAX — clamp to u16::MAX.
    let actions = local(
        &mut sm,
        SourceEvent::CursorAt {
            x: cfg.local_screen_w as i32 + 1,
            y: 100_000,
        },
        100,
    );
    assert_eq!(actions[0], Action::SendTakeControl { entry_y: u16::MAX });

    // Reset.
    let cfg2 = default_cfg();
    let mut sm2 = StateMachine::new(cfg2);
    let actions = local(
        &mut sm2,
        SourceEvent::CursorAt {
            x: cfg2.local_screen_w as i32 + 1,
            y: -10,
        },
        100,
    );
    assert_eq!(actions[0], Action::SendTakeControl { entry_y: 0 });
}

#[test]
fn custom_config_is_honored() {
    let cfg = EdgeConfig {
        local_screen_w: 2560,
        local_screen_h: 1440,
        remote_screen_w: 1920,
        remote_screen_h: 1080,
        thrash_cooldown_ms: 100,
        back_warp_px: 10,
    };
    let mut sm = StateMachine::new(cfg);

    // Cross at the wider screen's edge.
    let actions = local(&mut sm, SourceEvent::CursorAt { x: 2560, y: 50 }, 100);
    assert!(matches!(
        actions[1],
        Action::WarpLocalCursor { x: 2550, y: 50 }
    ));

    // Release always lands (R→L is not cooldown-gated). Sanity-check that
    // the WarpLocalCursor uses the wider screen's edge_x = 2559.
    let release_actions = wire(&mut sm, Message::ReleaseControl { exit_y: 77 }, 199);
    assert!(release_actions.iter().any(|a| matches!(
        a,
        Action::WarpLocalCursor { x: 2559, y: 77 }
    )));
    assert_eq!(sm.state(), State::LocalActive);

    // The L→R cooldown still applies — re-crossing within 100 ms is blocked.
    let blocked = local(&mut sm, SourceEvent::CursorAt { x: 2560, y: 50 }, 250);
    assert!(blocked.is_empty(), "cross within 100ms cooldown is blocked");
    assert_eq!(sm.state(), State::LocalActive);

    // After cooldown clears, re-cross works.
    let after = local(&mut sm, SourceEvent::CursorAt { x: 2560, y: 50 }, 350);
    assert!(!after.is_empty());
    assert_eq!(sm.state(), State::RemoteActive);
}
