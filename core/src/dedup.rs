//! Replay protection: bounded set of processed event ids.
//!
//! NIP-GART: "Receivers MUST track processed event ids to prevent
//! relay-replay-driven duplicate alarms." The set is bounded (FIFO eviction)
//! and serializable so it can be persisted across app restarts.

use std::collections::{HashSet, VecDeque};

use nostr::EventId;

#[derive(Debug)]
pub struct SeenIds {
    capacity: usize,
    order: VecDeque<EventId>,
    set: HashSet<EventId>,
}

impl SeenIds {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    pub fn contains(&self, id: &EventId) -> bool {
        self.set.contains(id)
    }

    /// Insert an id, evicting the oldest entry when full.
    /// Returns `false` if the id was already present.
    pub fn insert(&mut self, id: EventId) -> bool {
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Hex ids, oldest first — for persistence.
    pub fn to_vec(&self) -> Vec<String> {
        self.order.iter().map(|id| id.to_hex()).collect()
    }

    /// Restore from persisted hex ids (silently skipping malformed entries).
    pub fn from_vec(capacity: usize, ids: &[String]) -> Self {
        let mut s = Self::new(capacity);
        for hex in ids {
            if let Ok(id) = EventId::from_hex(hex) {
                s.insert(id);
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> EventId {
        EventId::from_slice(&[n; 32]).unwrap()
    }

    #[test]
    fn dedup_and_eviction() {
        let mut s = SeenIds::new(3);
        assert!(s.insert(id(1)));
        assert!(!s.insert(id(1)), "duplicate insert returns false");
        assert!(s.insert(id(2)));
        assert!(s.insert(id(3)));
        assert!(s.insert(id(4)), "evicts oldest");
        assert!(!s.contains(&id(1)), "oldest evicted");
        assert!(s.contains(&id(4)));
        assert_eq!(s.len(), 3);
    }

    #[test]
    fn persistence_roundtrip() {
        let mut s = SeenIds::new(10);
        s.insert(id(1));
        s.insert(id(2));
        let v = s.to_vec();
        let restored = SeenIds::from_vec(10, &v);
        assert!(restored.contains(&id(1)));
        assert!(restored.contains(&id(2)));
        assert_eq!(restored.len(), 2);

        // malformed entries are skipped
        let restored = SeenIds::from_vec(10, &["nothex".into(), id(3).to_hex()]);
        assert_eq!(restored.len(), 1);
    }
}
