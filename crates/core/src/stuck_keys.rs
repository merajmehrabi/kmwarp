//! Tracker for currently-held keys, used by the stuck-key recovery path.
//!
//! Skeleton in M2/M3 prep; M7 finalizes the disconnect/transition hooks
//! that synthesize release events for everything left in the set. The
//! data structure is locked in early so both server and client can already
//! track key state during M5 without an API change.

use std::collections::HashSet;

#[derive(Default, Debug)]
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

    /// Drain all currently-held HID codes into a `Vec`, leaving the tracker
    /// empty. M7 wraps this in a helper that emits `Action::ForwardKey`s
    /// with `KeyState::Up`.
    pub fn drain_held(&mut self) -> Vec<u16> {
        self.keys.drain().collect()
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
}
