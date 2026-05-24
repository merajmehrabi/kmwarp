//! Tracker for currently-held keys, used by the stuck-key recovery path.
//!
//! Two-layer story:
//! 1. [`HeldKeys`] tracks the set of HID usage codes currently down on
//!    the destination side. The server's RemoteActive path inserts on
//!    every `KeyEvent { Down }` it forwards, and removes on every `Up`.
//! 2. On any RemoteActive exit — edge transition, peer `Bye`, heartbeat
//!    timeout, TCP RST, SIGTERM — the runtime calls
//!    [`HeldKeys::drain_release_actions`] and queues the returned
//!    [`Action`]s synchronously into the encoder before the connection
//!    tears down. The Windows side sees a clean release for every key
//!    the user was still holding.
//!
//! The data structure itself stays in `core` (no platform deps) so the
//! same tracker covers Mac→Win, Win→Mac (future), and any test harness
//! that wants to verify the invariant.

use std::collections::HashSet;

use crate::edge::Action;
use crate::wire::{key_state_code, Message};

#[derive(Default, Debug, Clone)]
pub struct HeldKeys {
    keys: HashSet<u16>,
}

impl HeldKeys {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, hid: u16) {
        self.keys.insert(hid);
    }

    pub fn remove(&mut self, hid: u16) {
        self.keys.remove(&hid);
    }

    pub fn is_held(&self, hid: u16) -> bool {
        self.keys.contains(&hid)
    }

    /// Drain all currently-held HID codes into a `Vec`, leaving the
    /// tracker empty. Returns the raw HID set — callers that need
    /// wire-side release actions should use
    /// [`HeldKeys::drain_release_actions`] instead.
    pub fn drain_held(&mut self) -> Vec<u16> {
        self.keys.drain().collect()
    }

    /// **M7 entry point.** Drain every held key into a `Vec<Action>`
    /// of `Action::SendMessage(Message::KeyEvent { state: Up,
    /// modifiers: 0 })`, ready for the server runtime to push into
    /// the encoder.
    ///
    /// The modifier byte is set to 0 because we're synthesizing
    /// releases that should land on the destination side without any
    /// implicit chord — if Shift itself is one of the held keys, its
    /// release event still goes out cleanly.
    ///
    /// Per PLAN.md §M7, this is called from:
    /// - every `RemoteActive ↔ LocalActive` edge transition,
    /// - the `Bye` / heartbeat-timeout / TCP-RST / SIGTERM shutdown
    ///   paths,
    ///
    /// **synchronously** before the encoder task exits.
    pub fn drain_release_actions(&mut self) -> Vec<Action> {
        self.keys
            .drain()
            .map(|hid| {
                Action::SendMessage(Message::KeyEvent {
                    hid_usage: hid,
                    state: key_state_code::UP,
                    modifiers: 0,
                })
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn insert_then_drain_yields_held_keys_and_empties_set() {
        let mut h = HeldKeys::new();
        assert!(h.is_empty());
        h.insert(0x04); // a
        h.insert(0x05); // b
        h.insert(0x04); // duplicate, set-like
        assert_eq!(h.len(), 2);
        assert!(h.is_held(0x04));
        assert!(h.is_held(0x05));
        assert!(!h.is_held(0x06));

        let mut drained = h.drain_held();
        drained.sort_unstable();
        assert_eq!(drained, vec![0x04, 0x05]);
        assert!(h.is_empty());
        assert!(!h.is_held(0x04));
    }

    #[test]
    fn remove_clears_individual_keys() {
        let mut h = HeldKeys::new();
        h.insert(0xE1); // left shift
        h.insert(0xE0); // left ctrl
        h.remove(0xE1);
        assert!(!h.is_held(0xE1));
        assert!(h.is_held(0xE0));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn drain_release_actions_yields_one_keyup_per_held() {
        let mut h = HeldKeys::new();
        h.insert(0xE1); // LShift
        h.insert(0x04); // A
        h.insert(0x05); // B
        let actions = h.drain_release_actions();
        assert_eq!(actions.len(), 3);
        // Tracker is empty after drain.
        assert!(h.is_empty());

        // Each action must be a SendMessage with KeyEvent{state: Up,
        // modifiers: 0} carrying one of the held HIDs.
        let mut emitted_hids: HashSet<u16> = HashSet::new();
        for a in actions {
            match a {
                Action::SendMessage(Message::KeyEvent {
                    hid_usage,
                    state,
                    modifiers,
                }) => {
                    assert_eq!(state, key_state_code::UP);
                    assert_eq!(modifiers, 0);
                    emitted_hids.insert(hid_usage);
                }
                other => panic!("unexpected action: {other:?}"),
            }
        }
        assert_eq!(emitted_hids, HashSet::from([0xE1, 0x04, 0x05]));
    }

    #[test]
    fn drain_release_actions_on_empty_is_empty() {
        let mut h = HeldKeys::new();
        assert!(h.drain_release_actions().is_empty());
    }
}
