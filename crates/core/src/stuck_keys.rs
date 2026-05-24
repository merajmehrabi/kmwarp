//! Tracker for currently-held keys, used by the stuck-key recovery path.
//!
//! Two-layer story:
//! 1. [`HeldKeys`] tracks the set of HID usage codes currently down on
//!    the destination side. The server's RemoteActive path inserts on
//!    every `KeyEvent { Down }` it forwards, and removes on every `Up`.
//!    (Use [`HeldKeys::observe`] for one-line dispatch from a
//!    `KeyState`.)
//! 2. On any RemoteActive exit — edge transition, peer `Bye`, heartbeat
//!    timeout, TCP RST, SIGTERM — the runtime calls [`HeldKeys::drain`],
//!    constructs `KeyEvent { Up }` releases for each returned HID, and
//!    pushes them synchronously into the encoder before the connection
//!    tears down. The Windows side sees a clean release for every key
//!    the user was still holding.
//!
//! `BTreeSet`-backed: drain order is deterministic (sorted ascending by
//! HID), so test scripts can assert exact action sequences. The tracker
//! has zero platform deps and zero `edge::Action` knowledge — the
//! Action wrapping happens at the runtime composition layer.

use std::collections::BTreeSet;

use crate::platform::KeyState;

#[derive(Default, Debug, Clone)]
pub struct HeldKeys {
    keys: BTreeSet<u16>,
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

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// One-line dispatch helper: `Down → insert`, `Up → remove`. Cuts
    /// the boilerplate in the server/client forwarding loops.
    pub fn observe(&mut self, hid: u16, state: KeyState) {
        match state {
            KeyState::Down => self.insert(hid),
            KeyState::Up => self.remove(hid),
        }
    }

    /// Borrowed iteration in deterministic (sorted) order. Useful for
    /// tracing / diagnostics without draining the set.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.keys.iter().copied()
    }

    /// Return all currently-held HIDs in deterministic order and
    /// empty the tracker.
    ///
    /// Used at:
    /// - every `RemoteActive ↔ LocalActive` edge transition,
    /// - the `Bye` / heartbeat-timeout / TCP-RST / SIGTERM shutdown
    ///   paths,
    ///
    /// synchronously before the encoder task exits. The caller is
    /// expected to wrap each returned HID into a wire
    /// `Message::KeyEvent { state: Up, modifiers: 0 }` and push it
    /// through the connection writer.
    pub fn drain(&mut self) -> Vec<u16> {
        let taken = std::mem::take(&mut self.keys);
        taken.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_drain_returns_all_held_in_order() {
        let mut h = HeldKeys::new();
        h.insert(0x10);
        h.insert(0x04);
        h.insert(0xE1);
        h.insert(0x04); // dup, set-like
        assert_eq!(h.len(), 3);

        let drained = h.drain();
        // BTreeSet → drain returns sorted ascending.
        assert_eq!(drained, vec![0x04, 0x10, 0xE1]);
        assert!(h.is_empty());
    }

    #[test]
    fn observe_dispatches_insert_or_remove_by_state() {
        let mut h = HeldKeys::new();
        h.observe(0xE1, KeyState::Down);
        assert!(h.is_held(0xE1));
        h.observe(0x04, KeyState::Down);
        assert_eq!(h.len(), 2);
        h.observe(0xE1, KeyState::Up);
        assert!(!h.is_held(0xE1));
        assert!(h.is_held(0x04));
    }

    #[test]
    fn drain_leaves_empty() {
        let mut h = HeldKeys::new();
        h.insert(0x04);
        h.insert(0xE1);
        let _ = h.drain();
        assert!(h.is_empty());
        assert!(!h.is_held(0x04));
        assert!(!h.is_held(0xE1));
        // Second drain on an empty set is also fine.
        assert!(h.drain().is_empty());
    }

    #[test]
    fn remove_clears_individual_keys() {
        let mut h = HeldKeys::new();
        h.insert(0xE1);
        h.insert(0xE0);
        h.remove(0xE1);
        assert!(!h.is_held(0xE1));
        assert!(h.is_held(0xE0));
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn iter_yields_held_in_sorted_order_without_consuming() {
        let mut h = HeldKeys::new();
        for hid in [0xE1, 0x04, 0x10] {
            h.insert(hid);
        }
        let collected: Vec<u16> = h.iter().collect();
        assert_eq!(collected, vec![0x04, 0x10, 0xE1]);
        // Set is still populated after iter.
        assert_eq!(h.len(), 3);
    }
}
