//! Edge-crossing state machine.
//!
//! Owns the `LocalActive` / `RemoteActive` toggle that decides whether
//! the Mac's input drives the Mac (local) or the Windows peer (remote).
//! Everything in this module is pure: no `tokio`, no `tracing` spans on
//! the hot path, no clock reads. The server's runtime feeds it
//! [`SourceEvent`]s from the local source and [`Message`]s from the
//! wire, gets back an ordered list of [`Action`]s, and executes them.
//!
//! Keeping the transition logic pure means the M6 acceptance test
//! ("no stuck keys after 50 round trips") can be exercised in a few
//! microseconds of `cargo test` rather than 50 manual cursor crossings.
//!
//! # Time
//!
//! Every entry point takes `now_ms: u64` — a monotonic millisecond
//! reading the platform layer obtains from `Instant::elapsed().as_millis()`
//! or similar. Using `u64` (not `Instant`) makes scripted tests trivial
//! and keeps the state machine `no_std`-compatible if we ever want it.
//!
//! # Layout (v1, hardcoded)
//!
//! Windows is right of Mac: cursor crosses to remote when
//! `x >= cfg.local_screen_w`. Multi-monitor topology editing is v1.1
//! (PLAN.md §Out-of-scope). The spec corner case `x == local_screen_w`
//! IS a crossing — CG event taps deliver cursor positions in points
//! that can momentarily land exactly at or beyond the screen width
//! during fast motion.
//!
//! # Edge-thrash protection
//!
//! - **Back-warp** on entering `RemoteActive`: warp the local cursor
//!   `cfg.back_warp_px` (5 by default) back from the edge so the very
//!   next mouse event can't immediately re-trigger.
//! - **Cooldown**: drop crossings for `cfg.thrash_cooldown_ms` ms
//!   (50 ms default) after any transition.
//!
//! # Stuck keys
//!
//! Out of scope for this module. The state machine does NOT track held
//! keys — that's [`crate::stuck_keys::HeldKeys`], composed externally
//! by the M7 disconnect / transition wiring.

use smallvec::SmallVec;

use crate::platform::SourceEvent;
use crate::wire::{convert::source_event_to_message, Message};

/// Which side currently owns the cursor.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Mac drives. Tap is not swallowing; local events pass through
    /// to the OS; nothing is forwarded to the peer.
    LocalActive,
    /// Windows drives. Tap is swallowing; mouse + key events are
    /// forwarded to the peer.
    RemoteActive,
}

/// Layout + tuning parameters. v1 hardcodes the right-of-Mac topology;
/// v1.1 (M11 config UI) makes layout configurable but the *shape* of
/// this struct stays.
#[derive(Copy, Clone, Debug)]
pub struct EdgeConfig {
    /// Mac screen width in physical pixels. Cursor crossing happens at
    /// `x >= local_screen_w`.
    pub local_screen_w: u32,
    pub local_screen_h: u32,
    /// Windows screen dimensions. Sent to the peer in `HelloAck` and
    /// used by the client's cursor-leave detection; the server-side
    /// state machine references them for diagnostics only.
    pub remote_screen_w: u32,
    pub remote_screen_h: u32,
    /// Hysteresis window in ms — drop further crossings within this
    /// window after a transition.
    pub thrash_cooldown_ms: u32,
    /// Pixels back from the edge to warp the local cursor on entering
    /// `RemoteActive`. Defends against the back-warp racing with a
    /// fast mouse motion.
    pub back_warp_px: i32,
}

impl Default for EdgeConfig {
    fn default() -> Self {
        Self {
            local_screen_w: 1920,
            local_screen_h: 1080,
            remote_screen_w: 2560,
            remote_screen_h: 1440,
            thrash_cooldown_ms: 50,
            back_warp_px: 5,
        }
    }
}

/// Inline capacity for the action vector returned by `on_*` methods.
/// Transitions emit at most 4 actions; steady-state forwarding emits 1.
pub const INLINE_ACTIONS: usize = 4;

/// Side-effects the runtime should perform after `on_local_event` /
/// `on_wire_message`.
///
/// `Send*` variants split into typed control-flow actions and a
/// generic `SendMessage` for per-event forwarding so the executor can
/// pattern-match the typed variants without inspecting a `Message`
/// payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send a `Message::TakeControl { entry_y }` to the peer. Always
    /// first in a `LocalActive → RemoteActive` group so the peer
    /// warps its cursor before any forwarded motion lands.
    SendTakeControl { entry_y: u16 },

    /// Send a `Message::ReleaseControl { exit_y }` to the peer.
    /// Reserved for the future Mac-as-client direction; not emitted
    /// by the v1 server-side machine.
    SendReleaseControl { exit_y: u16 },

    /// Warp the **local** cursor to the given physical-pixel position.
    /// Used for the back-warp on entering `RemoteActive`, and for
    /// repositioning at the edge on returning to `LocalActive`.
    WarpLocalCursor { x: i32, y: i32 },

    /// `CGDisplayHideCursor` — server-side macOS call.
    HideLocalCursor,
    /// `CGDisplayShowCursor` — server-side macOS call.
    ShowLocalCursor,

    /// Start swallowing tap events (CGEventTap callback returns NULL).
    /// Entering `RemoteActive`.
    StartSwallow,
    /// Stop swallowing tap events. Returning to `LocalActive`.
    StopSwallow,

    /// Forward a wire message to the peer. The executor enqueues it
    /// into the connection writer.
    SendMessage(Message),
}

/// The state machine itself.
#[derive(Debug)]
pub struct StateMachine {
    state: State,
    cfg: EdgeConfig,
    last_transition_ms: u64,
    /// Last cursor position seen via `CursorAt`. Updated in every
    /// state; the LocalActive crossing decision reads its `x` from
    /// the incoming `CursorAt` directly, but `last_cursor.y` is the
    /// fallback for diagnostic/reconnect paths.
    last_cursor: (i32, i32),
}

impl StateMachine {
    /// Fresh state machine. Starts in `LocalActive` with the cursor
    /// assumed at the middle of the local screen (the platform layer
    /// will overwrite this on its first `CursorAt`).
    pub fn new(cfg: EdgeConfig) -> Self {
        let mid_x = (cfg.local_screen_w as i32) / 2;
        let mid_y = (cfg.local_screen_h as i32) / 2;
        Self {
            state: State::LocalActive,
            cfg,
            last_transition_ms: 0,
            last_cursor: (mid_x, mid_y),
        }
    }

    /// Current state. Useful for trace logging and tests.
    pub fn state(&self) -> State {
        self.state
    }

    /// Read-only access to the active layout config.
    pub fn cfg(&self) -> &EdgeConfig {
        &self.cfg
    }

    /// Process one event from the local [`InputSource`](crate::InputSource).
    pub fn on_local_event(
        &mut self,
        event: SourceEvent,
        now_ms: u64,
    ) -> SmallVec<[Action; INLINE_ACTIONS]> {
        let mut actions = SmallVec::new();
        match (self.state, event) {
            // ── LocalActive ─────────────────────────────────────
            (State::LocalActive, SourceEvent::CursorAt { x, y }) => {
                self.last_cursor = (x, y);
                if !self.cooldown_elapsed(now_ms) {
                    return actions;
                }
                // Spec: `x == local_screen_w` IS a crossing. CG event
                // taps occasionally deliver positions exactly at or
                // past the screen width during fast motion.
                if x >= self.cfg.local_screen_w as i32 {
                    let entry_y = clamp_to_u16(y);
                    let back_x =
                        (self.cfg.local_screen_w as i32).saturating_sub(self.cfg.back_warp_px);
                    actions.push(Action::SendTakeControl { entry_y });
                    actions.push(Action::WarpLocalCursor { x: back_x, y });
                    actions.push(Action::HideLocalCursor);
                    actions.push(Action::StartSwallow);
                    self.state = State::RemoteActive;
                    self.last_transition_ms = now_ms;
                }
            }
            // LocalActive ignores mouse/key — the tap isn't swallowing,
            // so they pass straight to the local OS.
            (State::LocalActive, _) => {}

            // ── RemoteActive ────────────────────────────────────
            (State::RemoteActive, SourceEvent::CursorAt { x, y }) => {
                // Defensive: the cursor is hidden + back-warped while
                // remote, so we shouldn't see CursorAt here. Update
                // last_cursor for the trace path and emit nothing.
                self.last_cursor = (x, y);
                tracing::trace!(?x, ?y, "CursorAt while RemoteActive; ignoring");
            }
            (State::RemoteActive, ev) => {
                // Mouse / key forwarding goes through the central
                // helper so the wire-byte dictionary stays in one
                // place (`wire::convert`).
                if let Some(msg) = source_event_to_message(ev) {
                    actions.push(Action::SendMessage(msg));
                }
            }
        }
        actions
    }

    /// Process one wire frame received from the peer.
    pub fn on_wire_message(
        &mut self,
        msg: &Message,
        now_ms: u64,
    ) -> SmallVec<[Action; INLINE_ACTIONS]> {
        let mut actions = SmallVec::new();
        // Only ReleaseControl matters to the state machine; everything
        // else (Heartbeat, Hello*, Bye, Echo*, ClipboardText, ...) is
        // handled elsewhere in the runtime.
        if let Message::ReleaseControl { exit_y } = msg {
            if self.state != State::RemoteActive {
                return actions;
            }
            // NOTE: cooldown intentionally NOT applied here. The cooldown
            // only protects against rapid L→R bouncing on the Mac edge
            // (where the user's hand at the edge could rapid-fire CursorAt
            // events). R→L is peer-initiated by an explicit ReleaseControl,
            // sent at most once per cursor_watch leave-detection. Dropping
            // it would strand the SM in RemoteActive forever — the
            // cursor_watch already disabled itself after sending, so no
            // retry will come.
            // Spec: warp local cursor to `local_screen_w - 1` so the
            // visual handoff is continuous.
            let edge_x = (self.cfg.local_screen_w as i32).saturating_sub(1);
            actions.push(Action::StopSwallow);
            actions.push(Action::ShowLocalCursor);
            actions.push(Action::WarpLocalCursor {
                x: edge_x,
                y: i32::from(*exit_y),
            });
            self.state = State::LocalActive;
            self.last_transition_ms = now_ms;
        }
        actions
    }

    /// True iff at least `thrash_cooldown_ms` ms have passed since the
    /// last transition (or this is the first transition).
    fn cooldown_elapsed(&self, now_ms: u64) -> bool {
        if self.last_transition_ms == 0 {
            return true;
        }
        now_ms.saturating_sub(self.last_transition_ms) >= u64::from(self.cfg.thrash_cooldown_ms)
    }
}

/// Clamp an `i32` cursor coordinate into the `u16` field used by
/// `TakeControl` / `ReleaseControl`. Saturating on both ends.
fn clamp_to_u16(v: i32) -> u16 {
    v.clamp(0, i32::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    //! Quick smoke tests live next to the impl; the full M6 scenarios
    //! (transitions, forwarding, cooldown, 50-round-trip invariant)
    //! live in `crates/core/tests/edge_transitions.rs`.

    use super::*;

    #[test]
    fn starts_in_local_active() {
        let sm = StateMachine::new(EdgeConfig::default());
        assert_eq!(sm.state(), State::LocalActive);
    }

    #[test]
    fn clamp_to_u16_saturates() {
        assert_eq!(clamp_to_u16(-1), 0);
        assert_eq!(clamp_to_u16(0), 0);
        assert_eq!(clamp_to_u16(1000), 1000);
        assert_eq!(clamp_to_u16(70_000), u16::MAX);
    }

    #[test]
    fn config_default_matches_spec() {
        let c = EdgeConfig::default();
        assert_eq!(c.local_screen_w, 1920);
        assert_eq!(c.local_screen_h, 1080);
        assert_eq!(c.remote_screen_w, 2560);
        assert_eq!(c.remote_screen_h, 1440);
        assert_eq!(c.thrash_cooldown_ms, 50);
        assert_eq!(c.back_warp_px, 5);
    }
}
