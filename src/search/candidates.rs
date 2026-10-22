//! Temporary file-ordinal sets for exhaustive queries; never persisted.
#[derive(Clone)]
pub(crate) struct Candidates {
    words: Vec<u64>,
}
impl Candidates {
    pub fn empty(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
        }
    }
    pub fn insert(&mut self, slot: usize) {
        self.words[slot / 64] |= 1 << (slot % 64);
    }
    pub fn intersect(&mut self, other: &Self) {
        assert_eq!(self.words.len(), other.words.len());
        for (a, b) in self.words.iter_mut().zip(&other.words) {
            *a &= b;
        }
    }
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }
    pub fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(word, &bits)| {
            let mut bits = bits;
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let slot = word * 64 + bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(slot)
            })
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn collect(set: &Candidates) -> Vec<usize> {
        set.iter().collect()
    }

    fn from_slots(len: usize, slots: &[usize]) -> Candidates {
        let mut set = Candidates::empty(len);
        for &slot in slots {
            set.insert(slot);
        }
        set
    }

    #[test]
    fn empty_sets_have_no_members() {
        for len in [0, 1, 63, 64, 65, 128, 129] {
            let set = Candidates::empty(len);
            assert_eq!(set.count(), 0, "len {len}");
            assert_eq!(collect(&set), Vec::<usize>::new(), "len {len}");
        }
    }

    #[test]
    fn word_boundary_slots_iterate_in_ascending_order() {
        let slots = [0, 63, 64, 127, 128];
        // Insert out of order to check iteration sorts by slot, not insertion.
        let set = from_slots(129, &[128, 0, 127, 64, 63]);
        assert_eq!(collect(&set), slots);
        assert_eq!(set.count(), slots.len());
    }

    #[test]
    fn last_slot_of_a_partial_word_is_addressable() {
        let set = from_slots(65, &[64]);
        assert_eq!(collect(&set), [64]);
        let set = from_slots(1, &[0]);
        assert_eq!(collect(&set), [0]);
    }

    #[test]
    fn inserting_twice_is_idempotent() {
        let mut set = from_slots(129, &[63, 64]);
        set.insert(63);
        set.insert(64);
        assert_eq!(collect(&set), [63, 64]);
        assert_eq!(set.count(), 2);
    }

    #[test]
    fn intersect_keeps_only_common_slots() {
        let mut a = from_slots(200, &[0, 5, 63, 64, 100, 199]);
        let b = from_slots(200, &[5, 64, 101, 199]);
        a.intersect(&b);
        assert_eq!(collect(&a), [5, 64, 199]);
        assert_eq!(a.count(), 3);
    }

    #[test]
    fn intersect_with_empty_is_empty() {
        let mut a = from_slots(129, &[0, 63, 64, 128]);
        a.intersect(&Candidates::empty(129));
        assert_eq!(a.count(), 0);
        assert_eq!(collect(&a), Vec::<usize>::new());
    }

    #[test]
    fn intersect_with_itself_is_identity() {
        let slots = [1, 63, 64, 127, 128];
        let mut a = from_slots(129, &slots);
        let copy = a.clone();
        a.intersect(&copy);
        assert_eq!(collect(&a), slots);
    }

    #[test]
    #[should_panic]
    fn intersect_rejects_mismatched_lengths() {
        // 64 slots is one word, 65 is two: the word counts differ.
        let mut a = Candidates::empty(64);
        a.intersect(&Candidates::empty(65));
    }

    /// xorshift64: deterministic and dependency-free.
    fn next(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    fn random_pair(len: usize, inserts: usize, state: &mut u64) -> (Candidates, BTreeSet<usize>) {
        let mut set = Candidates::empty(len);
        let mut model = BTreeSet::new();
        for _ in 0..inserts {
            let slot = (next(state) % len as u64) as usize;
            set.insert(slot);
            model.insert(slot);
        }
        (set, model)
    }

    #[test]
    fn matches_btreeset_model() {
        let mut state = 0x9E37_79B9_7F4A_7C15;
        for len in [1, 64, 65, 1000, 1024, 1031] {
            for inserts in [0, 1, 10, 300, 2000] {
                let (mut a, model_a) = random_pair(len, inserts, &mut state);
                let (b, model_b) = random_pair(len, inserts, &mut state);
                assert_eq!(collect(&a), model_a.iter().copied().collect::<Vec<_>>());
                assert_eq!(a.count(), model_a.len());
                a.intersect(&b);
                let expected: Vec<usize> = model_a.intersection(&model_b).copied().collect();
                assert_eq!(collect(&a), expected, "len {len}, inserts {inserts}");
                assert_eq!(a.count(), expected.len());
            }
        }
    }
}
