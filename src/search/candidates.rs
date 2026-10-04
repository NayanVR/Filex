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
