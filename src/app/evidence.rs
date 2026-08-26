use std::collections::{BTreeSet, HashMap};

pub(super) const MAX_NODE_IDS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SampleMeta {
    pub original_bytes: usize,
    pub original_lines: usize,
    pub truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CapturedSample {
    pub text: String,
    pub meta: SampleMeta,
}

pub(super) fn capture(text: &str, max_bytes: usize) -> CapturedSample {
    let (head, tail) = split_ends(text, max_bytes);
    let mut stored = String::with_capacity(head.len() + tail.len());
    stored.push_str(head);
    stored.push_str(tail);
    CapturedSample {
        text: stored,
        meta: SampleMeta {
            original_bytes: text.len(),
            original_lines: line_count(text),
            truncated: text.len() > max_bytes,
        },
    }
}

fn line_count(s: &str) -> usize {
    if s.is_empty() {
        0
    } else {
        s.bytes().filter(|b| *b == b'\n').count() + usize::from(!s.ends_with('\n'))
    }
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    if i > s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

fn split_ends(s: &str, max: usize) -> (&str, &str) {
    if s.len() <= max {
        return (s, "");
    }
    if max == 0 {
        return ("", "");
    }
    let tail_budget = max / 2;
    let head_budget = max - tail_budget;
    let head_end = floor_boundary(s, head_budget);
    if tail_budget == 0 {
        return (&s[..head_end], "");
    }
    let tail_start = ceil_boundary(s, s.len() - tail_budget);
    if tail_start <= head_end {
        return (&s[..floor_boundary(s, max)], "");
    }
    (&s[..head_end], &s[tail_start..])
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct NodeSet {
    ids: BTreeSet<String>,
    capped: bool,
}

impl NodeSet {
    pub(super) fn singleton(id: String) -> Self {
        let mut nodes = Self::default();
        nodes.insert(id);
        nodes
    }

    pub(super) fn insert(&mut self, id: String) {
        if self.ids.contains(&id) {
            return;
        }
        if self.ids.len() < MAX_NODE_IDS {
            self.ids.insert(id);
        } else {
            self.capped = true;
        }
    }

    pub(super) fn merge(&mut self, other: Self) {
        for id in other.ids {
            self.insert(id);
        }
        self.capped |= other.capped;
    }

    pub(super) fn ids(&self) -> impl Iterator<Item = &String> {
        self.ids.iter()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(super) fn count(&self) -> u64 {
        self.ids.len() as u64 + u64::from(self.capped)
    }

    pub(super) fn capped(&self) -> bool {
        self.capped
    }
}

struct StoredSample {
    text: String,
    tick: u64,
}

pub(super) struct SampleStore {
    budget: usize,
    used: usize,
    next_tick: u64,
    samples: HashMap<u64, StoredSample>,
}

impl SampleStore {
    pub(super) fn new(budget: usize) -> Self {
        Self {
            budget,
            used: 0,
            next_tick: 1,
            samples: HashMap::new(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn budget(&self) -> usize {
        self.budget
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn used(&self) -> usize {
        self.used
    }

    pub(super) fn contains(&self, group_id: u64) -> bool {
        self.samples.contains_key(&group_id)
    }

    pub(super) fn get(&self, group_id: u64) -> Option<&str> {
        self.samples
            .get(&group_id)
            .map(|sample| sample.text.as_str())
    }

    pub(super) fn insert(&mut self, group_id: u64, text: String) -> Vec<u64> {
        let _ = self.take(group_id);
        self.put(
            group_id,
            StoredSample {
                text,
                tick: self.next_tick,
            },
        );
        self.next_tick += 1;
        let mut evicted = Vec::new();
        while self.used > self.budget {
            let Some(victim) = self.victim_except(group_id) else {
                let _ = self.take(group_id);
                evicted.push(group_id);
                break;
            };
            let _ = self.take(victim);
            evicted.push(victim);
        }
        evicted.sort_unstable();
        evicted
    }

    pub(super) fn touch(&mut self, group_id: u64) -> bool {
        let Some(sample) = self.samples.get_mut(&group_id) else {
            return false;
        };
        sample.tick = self.next_tick;
        self.next_tick += 1;
        true
    }

    pub(super) fn merge(&mut self, keep_id: u64, drop_id: u64) {
        let keep = self.take(keep_id);
        let drop = self.take(drop_id);
        if let Some(sample) = keep.or(drop) {
            self.put(keep_id, sample);
        }
    }

    fn take(&mut self, group_id: u64) -> Option<StoredSample> {
        let sample = self.samples.remove(&group_id)?;
        self.used -= sample.text.len();
        Some(sample)
    }

    fn put(&mut self, group_id: u64, sample: StoredSample) {
        self.used += sample.text.len();
        self.samples.insert(group_id, sample);
    }

    fn victim_except(&self, keep_id: u64) -> Option<u64> {
        self.samples
            .iter()
            .filter(|(id, _)| **id != keep_id)
            .min_by_key(|(id, sample)| (sample.tick, *id))
            .map(|(id, _)| *id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_keeps_short_sample_intact() {
        let captured = capture("hello\nworld", 64);
        assert_eq!(captured.text, "hello\nworld");
        assert_eq!(captured.meta.original_bytes, 11);
        assert_eq!(captured.meta.original_lines, 2);
        assert!(!captured.meta.truncated);
    }

    #[test]
    fn capture_preserves_beginning_and_ending() {
        let mut text = String::from("HEAD-MESSAGE\n");
        text.push_str(&"x".repeat(200));
        text.push_str("\nTAIL-EXCEPTION\n");
        let captured = capture(&text, 40);
        assert!(captured.meta.truncated);
        assert_eq!(captured.meta.original_bytes, text.len());
        assert_eq!(captured.meta.original_lines, 3);
        assert!(
            captured.text.starts_with("HEAD-MESSAGE\n"),
            "{}",
            captured.text
        );
        assert!(
            captured.text.ends_with("TAIL-EXCEPTION\n"),
            "{}",
            captured.text
        );
        assert!(captured.text.len() <= 40);
    }

    #[test]
    fn capture_stays_on_char_boundaries() {
        let text = "é".repeat(20);
        let captured = capture(&text, 5);
        assert!(captured.meta.truncated);
        assert!(captured.text.is_char_boundary(captured.text.len()));
        assert!(captured.text.len() <= 5);
        assert!(captured.text.starts_with('é') || captured.text.is_empty());
    }

    #[test]
    fn lru_evicts_oldest_then_lowest_id() {
        let mut store = SampleStore::new(4);
        assert!(store.insert(2, "aa".into()).is_empty());
        assert!(store.insert(1, "bb".into()).is_empty());
        let evicted = store.insert(3, "cc".into());
        assert_eq!(evicted, [2]);
        assert!(!store.contains(2));
        assert!(store.contains(1));
        assert!(store.contains(3));
        assert_eq!(store.used(), 4);
    }

    #[test]
    fn touch_refreshes_recency_without_changing_bytes() {
        let mut store = SampleStore::new(4);
        store.insert(1, "aa".into());
        store.insert(2, "bb".into());
        assert!(store.touch(1));
        let evicted = store.insert(3, "cc".into());
        assert_eq!(evicted, [2]);
        assert_eq!(store.get(1), Some("aa"));
        assert_eq!(store.used(), 4);
    }

    #[test]
    fn merge_keeps_oldest_available_sample_once() {
        let mut store = SampleStore::new(8);
        store.insert(1, "old".into());
        store.insert(2, "new".into());
        store.merge(1, 2);
        assert_eq!(store.get(1), Some("old"));
        assert!(!store.contains(2));
        assert_eq!(store.used(), 3);

        let mut store = SampleStore::new(3);
        store.insert(1, "old".into());
        store.insert(2, "new".into());
        assert!(!store.contains(1));
        store.merge(1, 2);
        assert_eq!(store.get(1), Some("new"));
        assert_eq!(store.used(), 3);
    }

    #[test]
    fn nodes_are_exact_through_cap_then_bounded() {
        let mut nodes = NodeSet::default();
        for i in 0..300 {
            nodes.insert(format!("n{i}"));
        }
        assert_eq!(nodes.len(), MAX_NODE_IDS);
        assert!(nodes.capped());
        assert!(nodes.count() >= MAX_NODE_IDS as u64);
        assert_eq!(nodes.ids().count(), MAX_NODE_IDS);
        nodes.insert("n0".into());
        assert_eq!(nodes.len(), MAX_NODE_IDS);
    }

    #[test]
    fn node_merge_stays_bounded() {
        let mut left = NodeSet::default();
        let mut right = NodeSet::default();
        for i in 0..200 {
            left.insert(format!("a{i}"));
            right.insert(format!("b{i}"));
        }
        left.merge(right);
        assert_eq!(left.len(), MAX_NODE_IDS);
        assert!(left.capped());
        assert!(left.count() >= MAX_NODE_IDS as u64);
    }
}
