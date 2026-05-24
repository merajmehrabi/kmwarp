//! Edge-crossing state machine.
//!
//! Owns the `LocalActive` / `RemoteActive` toggle that decides whether
//! the Mac's input drives the Mac (local) or the Windows peer (remote).
//! Everything in this module is pure: no `tokio`, no `tracing` spans on
//! the hot path, no clock reads. The server's runtime feeds it
//! [`SourceEvent`]s (and incoming `ReleaseControl` frames) plus the
//! current `Instant`, gets back an ordered list of [`Action`]s, and
//! executes them.
//!
//! The PLAN.md design point: keeping the transition logic pure means
//! the M6 acceptance test ("no stuck keys after 50 round trips") can
//! be exercised in a few microseconds of `cargo test` rather than 50
//! manual cursor crossings.
//!
//! # Layout assumption (v1, hardcoded)
//!
//! Windows screen is to the right of the Mac: cursor crosses to remote
//! when `x >= mac_screen_width`, comes back when the client sends
//! `ReleaseControl` (the client polls its own cursor). Multi-monitor
//! topology editing is v1.1 — see PLAN.md §Out-of-scope.
//!
//! # Edge-thrash protection
//!
//! Two mechanisms, both required per PLAN.md §M6 risks:
//! - **Back-warp**: on transition into `RemoteActive`, warp the local
//!   cursor 5 px back from the edge so the very next mouse event from
//!   the user can't immediately re-trigger the same crossing.
//! - **Cooldown**: ignore further crossings for 50 ms after any
//!   transition. The clock is injected (`now: Instant` parameter on
//!   every entry point) so the test suite can drive transitions in
//!   sub-millisecond deterministic time.
//!
//! # Stuck-key safety
//!
//! `RemoteActive` is the only state during which the server forwards
//! key events; while in that state the machine tracks held keys in a
//! [`HeldKeys`]. On `RemoteActive → LocalActive` transition (release
//! control), the held set is drained and a `ForwardKey { state: Up }`
//! action is emitted for every key still down. M7 extends the same
//! drain to disconnect paths.

use std::time::{Duration, Instant};

use smallvec::SmallVec;

use crate::platform::{KeyState, ModMask, MouseButton, SourceEvent};
use crate::stuck_keys::HeldKeys;

/// Time after a transition during which further crossings are ignored.
///
/// Per PLAN.md §M6 risks: prevents flicker when the user noodles the
/// cursor across the boundary too fast for the back-warp to land.
pub const EDGE_COOLDOWN: Duration = Duration::from_millis(50);

/// How far back from the edge to warp the local cursor on transition
/// into `RemoteActive`. Per PLAN.md §M6 risks.
pub const BACK_WARP_PX: i32 = 5;

/// Which side currently owns the cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Mac drives. Input events pass through to the local OS; nothing
    /// is forwarded to the peer.
    LocalActive,
    /// Windows drives. Mouse + key events from the Mac are forwarded
    /// to the peer; the local OS sees nothing because the tap is in
    /// swallow mode.
    RemoteActive,
}

/// Inline-capacity for the action vector. Transitions emit at most ~5
/// actions (drain-keys × N + StopSwallow + ShowCursor + Warp + state-
/// change tracking); steady-state RemoteActive forwarding emits 1.
/// Sized to fit transitions without spilling to heap for typical cases.
pub const INLINE_ACTIONS: usize = 4;

/// Side-effects the runtime should perform after `on_source_event` /
/// `on_release_control`.
///
/// Variants split mouse and keyboard forwarding so the runtime doesn't
/// have to inspect a `Message`-shaped union; the server's encoder loop
/// can pattern-match each variant to a single `Message::*` enqueue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send a `Message::TakeControl { entry_y }` to the peer. Always
    /// the **first** action in a `LocalActive → RemoteActive` group so
    /// the peer warps its cursor before any forwarded motion arrives.
    SendTakeControl { entry_y: u16 },

    /// Send a `Message::ReleaseControl { exit_y }` to the peer. Not
    /// emitted by the v1 server-side state machine (the client emits
    /// these); reserved for the future Mac-as-client direction.
    SendReleaseControl { exit_y: u16 },

    /// Warp the **local** cursor to the given physical-pixel position.
    /// Used for the 5-px back-warp on entering `RemoteActive`, and for
    /// re-positioning at the edge on returning to `LocalActive`.
    WarpLocal { x: i32, y: i32 },

    /// `CGDisplayHideCursor` — server-side macOS call.
    HideCursor,
    /// `CGDisplayShowCursor` — server-side macOS call.
    ShowCursor,

    /// Start swallowing tap events (return NULL from the CGEventTap
    /// callback). Entering `RemoteActive`.
    StartSwallow,
    /// Stop swallowing tap events. Returning to `LocalActive`.
    StopSwallow,

    /// Forward a mouse motion to the peer over the wire.
    ForwardMouseRel { dx: i16, dy: i16 },
    /// Forward a mouse button up/down to the peer.
    ForwardMouseButton {
        button: MouseButton,
        state: KeyState,
    },
    /// Forward a scroll wheel event.
    ForwardMouseWheel { dx: i16, dy: i16 },
    /// Forward a key event. Modifier bits are carried verbatim; M7
    /// remap is applied at the wire boundary, not here.
    ForwardKey {
        hid_usage: u16,
        state: KeyState,
        mods: ModMask,
    },
}

/// The state machine itself.
///
/// Not `Send + Sync` by accident — it's `!Sync` because it holds
/// mutable state, but it's perfectly fine to wrap in a `Mutex` if the
/// runtime ever needs to share it across tasks. v1 keeps it on a
/// single drain task.
#[derive(Debug)]
pub struct StateMachine {
    state: State,
    last_transition: Option<Instant>,
    held_keys: HeldKeys,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl StateMachine {
    /// Fresh state machine. Starts in `LocalActive` (Mac drives) with
    /// no held keys and no transition history.
    pub fn new() -> Self {
        Self {
            state: State::LocalActive,
            last_transition: None,
            held_keys: HeldKeys::new(),
        }
    }

    /// Current state. Useful for trace logging and tests.
    pub fn state(&self) -> State {
        self.state
    }

    /// True iff the held-key set is empty. Used by tests asserting the
    /// "no stuck keys after 50 round trips" invariant.
    pub fn held_keys_empty(&self) -> bool {
        self.held_keys.is_empty()
    }

    /// Process one event from the local [`InputSource`](crate::InputSource).
    ///
    /// `screen_w` is the **mac** screen width in physical pixels; the
    /// state machine uses it to detect cursor crossings of the right
    /// edge (hardcoded v1 layout). `now` is injected (not read from
    /// `Instant::now()`) so tests can drive transitions in deterministic
    /// time.
    pub fn on_source_event(
        &mut self,
        ev: SourceEvent,
        screen_w: u32,
        now: Instant,
    ) -> SmallVec<[Action; INLINE_ACTIONS]> {
        let mut actions = SmallVec::new();
        match (self.state, ev) {
            // ── LocalActive: watch cursor for right-edge crossing ──
            (State::LocalActive, SourceEvent::CursorAt { x, y }) => {
                if self.in_cooldown(now) {
                    return actions;
                }
                if x >= screen_w as i32 {
                    let entry_y = clamp_to_u16(y);
                    // Order matters: tell peer first so its cursor
                    // appears at the entry point before any forwarded
                    // motion lands.
                    actions.push(Action::SendTakeControl { entry_y });
                    let back_x = (screen_w as i32).saturating_sub(BACK_WARP_PX);
                    actions.push(Action::WarpLocal { x: back_x, y });
                    actions.push(Action::HideCursor);
                    actions.push(Action::StartSwallow);
                    self.state = State::RemoteActive;
                    self.last_transition = Some(now);
                }
            }
            // LocalActive ignores mouse/key — the local OS handles them.
            (State::LocalActive, _) => {}

            // ── RemoteActive: forward everything ──
            (State::RemoteActive, SourceEvent::MouseRel { dx, dy }) => {
                actions.push(Action::ForwardMouseRel { dx, dy });
            }
            (State::RemoteActive, SourceEvent::MouseButton { button, state }) => {
                actions.push(Action::ForwardMouseButton { button, state });
            }
            (State::RemoteActive, SourceEvent::MouseWheel { dx, dy }) => {
                actions.push(Action::ForwardMouseWheel { dx, dy });
            }
            (
                State::RemoteActive,
                SourceEvent::Key {
                    hid_usage,
                    state,
                    mods,
                },
            ) => {
                // Track held keys so we can synthesize releases on
                // transition / disconnect.
                match state {
                    KeyState::Down => self.held_keys.insert(hid_usage),
                    KeyState::Up => self.held_keys.remove(hid_usage),
                }
                actions.push(Action::ForwardKey {
                    hid_usage,
                    state,
                    mods,
                });
            }
            // While remote, the cursor is hidden + warped on the Mac;
            // CursorAt events still arrive (the OS doesn't stop tracking)
            // but they're not actionable until we return to LocalActive.
            (State::RemoteActive, SourceEvent::CursorAt { .. }) => {}
        }
        actions
    }

    /// Process an incoming `ReleaseControl { exit_y }` frame from the
    /// peer (i.e. the Windows client's cursor has reached its left
    /// edge and it's giving control back to us).
    ///
    /// Returns the action sequence the runtime should execute. In
    /// `LocalActive` (release came in spuriously, or as a no-op) this
    /// is empty.
    pub fn on_release_control(
        &mut self,
        exit_y: u16,
        screen_w: u32,
        now: Instant,
    ) -> SmallVec<[Action; INLINE_ACTIONS]> {
        let mut actions = SmallVec::new();
        if self.state != State::RemoteActive {
            // Spurious; nothing to do.
            return actions;
        }

        // Drain any still-held keys *before* we relinquish forwarding,
        // so the Windows side sees a clean release for every key the
        // user was holding. (Same drain happens on disconnect in M7.)
        for hid in self.held_keys.drain_held() {
            actions.push(Action::ForwardKey {
                hid_usage: hid,
                state: KeyState::Up,
                mods: ModMask::default(),
            });
        }

        actions.push(Action::StopSwallow);
        actions.push(Action::ShowCursor);
        // Position the local cursor at the edge at the y the peer
        // reported, so it visually "appears" where the Windows cursor
        // left from.
        let edge_x = (screen_w as i32).saturating_sub(BACK_WARP_PX);
        actions.push(Action::WarpLocal {
            x: edge_x,
            y: i32::from(exit_y),
        });

        self.state = State::LocalActive;
        self.last_transition = Some(now);
        actions
    }

    /// Drain every held key into `ForwardKey { state: Up }` actions,
    /// regardless of state. M7 will hook this into the disconnect /
    /// shutdown paths so a TCP RST during a held chord doesn't leave
    /// a stuck modifier on the Windows side.
    pub fn drain_held_keys(&mut self) -> SmallVec<[Action; INLINE_ACTIONS]> {
        let mut actions = SmallVec::new();
        for hid in self.held_keys.drain_held() {
            actions.push(Action::ForwardKey {
                hid_usage: hid,
                state: KeyState::Up,
                mods: ModMask::default(),
            });
        }
        actions
    }

    fn in_cooldown(&self, now: Instant) -> bool {
        self.last_transition
            .map(|t| now.duration_since(t) < EDGE_COOLDOWN)
            .unwrap_or(false)
    }
}

/// Clamp an `i32` cursor coordinate into the `u16` field used by
/// `TakeControl` / `ReleaseControl`. Negative coords (above the top of
/// the screen) clamp to 0; >65535 clamps to `u16::MAX`. In practice
/// neither extreme should ever occur on a sane display, but the wire
/// field is `u16` so the conversion has to be lossless-or-saturating.
fn clamp_to_u16(v: i32) -> u16 {
    v.clamp(0, i32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    //! Quick smoke tests live next to the impl; the full M6
    //! acceptance test (50 round trips, scripted SourceEvent
    //! sequences, action-sequence comparison) lives in
    //! `crates/core/tests/edge_transitions.rs`.

    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn starts_in_local_active_with_no_held_keys() {
        let sm = StateMachine::new();
        assert_eq!(sm.state(), State::LocalActive);
        assert!(sm.held_keys_empty());
    }

    #[test]
    fn clamp_to_u16_saturates() {
        assert_eq!(clamp_to_u16(-1), 0);
        assert_eq!(clamp_to_u16(0), 0);
        assert_eq!(clamp_to_u16(1000), 1000);
        assert_eq!(clamp_to_u16(70_000), u16::MAX);
    }

    #[test]
    fn cursor_inside_screen_stays_local() {
        let mut sm = StateMachine::new();
        let actions = sm.on_source_event(SourceEvent::CursorAt { x: 500, y: 300 }, 2560, t0());
        assert!(actions.is_empty());
        assert_eq!(sm.state(), State::LocalActive);
    }
}
