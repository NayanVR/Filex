//! Independently bit-packed 256-value blocks, with constant blocks using no payload.
//! Values remain randomly accessible directly from the immutable mapping.
use super::storage::{Packed, Reader, Span, Writer};
use anyhow::{Result, ensure};
use bytemuck::Pod;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, marker::PhantomData};

const BLOCK: usize = 256;

pub trait Integer: Pod + Copy + Ord {
    fn ordered(self) -> u64;
    fn from_ordered(value: u64) -> Self;
    const MAX: u64;
}
impl Integer for u64 {
    fn ordered(self) -> u64 {
        self
    }
    fn from_ordered(value: u64) -> Self {
        value
    }
    const MAX: u64 = u64::MAX;
}
impl Integer for u32 {
    fn ordered(self) -> u64 {
        u64::from(self)
    }
    fn from_ordered(value: u64) -> Self {
        value as u32
    }
    const MAX: u64 = u32::MAX as u64;
}
impl Integer for i64 {
    fn ordered(self) -> u64 {
        (self as u64) ^ (1 << 63)
    }
    fn from_ordered(value: u64) -> Self {
        (value ^ (1 << 63)) as i64
    }
    const MAX: u64 = u64::MAX;
}

pub struct Column<T: Integer> {
    words: Packed<u64>,
    bases: Packed<u64>,
    offsets: Packed<u32>,
    widths: Packed<u8>,
    len: usize,
    marker: PhantomData<T>,
}

impl<T: Integer> Default for Column<T> {
    fn default() -> Self {
        Vec::new().into()
    }
}

#[derive(Serialize, Deserialize)]
pub struct ColumnImage {
    words: Span,
    bases: Span,
    offsets: Span,
    widths: Span,
    len: usize,
}

impl<T: Integer> From<Vec<T>> for Column<T> {
    fn from(values: Vec<T>) -> Self {
        let mut words = Vec::new();
        let mut bases = Vec::new();
        let mut offsets = Vec::new();
        let mut widths = Vec::new();
        for block in values.chunks(BLOCK) {
            let base = block.iter().map(|v| v.ordered()).min().unwrap();
            let max = block.iter().map(|v| v.ordered()).max().unwrap();
            let width = (64 - (max - base).leading_zeros()) as usize;
            let offset = words.len();
            // A segment already has a u32 entry/offset limit.
            offsets.push(u32::try_from(offset).expect("column exceeds word offset range"));
            bases.push(base);
            widths.push(width as u8);
            words.resize(offset + (block.len() * width).div_ceil(64), 0u64);
            if width == 0 {
                continue;
            }
            for (i, value) in block.iter().enumerate() {
                let delta = value.ordered() - base;
                let bit = i * width;
                words[offset + bit / 64] |= delta << (bit % 64);
                if bit % 64 + width > 64 {
                    words[offset + bit / 64 + 1] |= delta >> (64 - bit % 64);
                }
            }
        }
        words.shrink_to_fit();
        bases.shrink_to_fit();
        offsets.shrink_to_fit();
        widths.shrink_to_fit();
        Self {
            words: words.into(),
            bases: bases.into(),
            offsets: offsets.into(),
            widths: widths.into(),
            len: values.len(),
            marker: PhantomData,
        }
    }
}

impl<T: Integer> Column<T> {
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn get(&self, index: usize) -> T {
        assert!(index < self.len());
        let block = index / BLOCK;
        let width = self.widths[block] as usize;
        let base = self.bases[block];
        if width == 0 {
            return T::from_ordered(base);
        }
        let bit = (index % BLOCK) * width;
        let word = self.offsets[block] as usize + bit / 64;
        let mut delta = self.words[word] >> (bit % 64);
        if bit % 64 + width > 64 {
            delta |= self.words[word + 1] << (64 - bit % 64);
        }
        if width < 64 {
            delta &= (1u64 << width) - 1;
        }
        T::from_ordered(base + delta)
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = T> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }
    pub fn pairs(&self) -> impl Iterator<Item = [T; 2]> + '_ {
        (1..self.len()).map(|i| [self.get(i - 1), self.get(i)])
    }
    pub fn first(&self) -> Option<T> {
        (!self.is_empty()).then(|| self.get(0))
    }
    pub fn last(&self) -> Option<T> {
        (!self.is_empty()).then(|| self.get(self.len() - 1))
    }
    pub fn bytes(&self) -> usize {
        self.words.capacity() * 8
            + self.bases.capacity() * 8
            + self.offsets.capacity() * 4
            + self.widths.capacity()
    }
    pub fn partition_point(&self, mut predicate: impl FnMut(T) -> bool) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if predicate(self.get(mid)) {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }
    pub fn binary_search_by(
        &self,
        mut compare: impl FnMut(T) -> Ordering,
    ) -> std::result::Result<usize, usize> {
        let at = self.partition_point(|v| compare(v) == Ordering::Less);
        if at < self.len() && compare(self.get(at)) == Ordering::Equal {
            Ok(at)
        } else {
            Err(at)
        }
    }
    pub fn binary_search_by_key<K: Ord>(
        &self,
        key: &K,
        mut f: impl FnMut(T) -> K,
    ) -> std::result::Result<usize, usize> {
        self.binary_search_by(|v| f(v).cmp(key))
    }
    pub fn binary_search(&self, key: &T) -> std::result::Result<usize, usize> {
        self.binary_search_by(|v| v.cmp(key))
    }
    pub fn save(&self, writer: &mut Writer) -> Result<ColumnImage> {
        Ok(ColumnImage {
            words: self.words.save(writer)?,
            bases: self.bases.save(writer)?,
            offsets: self.offsets.save(writer)?,
            widths: self.widths.save(writer)?,
            len: self.len,
        })
    }
    pub fn load(reader: &Reader, image: ColumnImage) -> Result<Self> {
        let ColumnImage {
            words,
            bases,
            offsets,
            widths,
            len,
        } = image;
        ensure!(
            len <= u32::MAX as usize,
            "column count exceeds ordinal range"
        );
        let words: Packed<u64> = Packed::load(reader, words)?;
        let bases: Packed<u64> = Packed::load(reader, bases)?;
        let offsets: Packed<u32> = Packed::load(reader, offsets)?;
        let widths: Packed<u8> = Packed::load(reader, widths)?;
        let blocks = len.div_ceil(BLOCK);
        ensure!(
            bases.len() == blocks && offsets.len() == blocks && widths.len() == blocks,
            "invalid column block lengths"
        );
        let mut end = 0;
        for block in 0..blocks {
            let width = widths[block] as usize;
            ensure!(
                width <= 64 && offsets[block] as usize == end && bases[block] <= T::MAX,
                "invalid column block header"
            );
            end += ((len - block * BLOCK).min(BLOCK) * width).div_ceil(64);
            ensure!(end <= words.len(), "truncated column block");
            // Validate decoded addition and type width before exposing accessors.
            for i in 0..(len - block * BLOCK).min(BLOCK) {
                if width == 0 {
                    break;
                }
                let bit = i * width;
                let word = offsets[block] as usize + bit / 64;
                let mut delta = words[word] >> (bit % 64);
                if bit % 64 + width > 64 {
                    delta |= words[word + 1] << (64 - bit % 64);
                }
                if width < 64 {
                    delta &= (1u64 << width) - 1;
                }
                ensure!(
                    bases[block].checked_add(delta).is_some_and(|v| v <= T::MAX),
                    "column value overflow"
                );
            }
        }
        ensure!(end == words.len(), "trailing column payload");
        Ok(Self {
            words,
            bases,
            offsets,
            widths,
            len,
            marker: PhantomData,
        })
    }
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.words.remap(reader)?;
        self.bases.remap(reader)?;
        self.offsets.remap(reader)?;
        self.widths.remap(reader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn roundtrip<T: Integer + std::fmt::Debug>(values: Vec<T>) {
        let column = Column::from(values.clone());
        assert_eq!(column.iter().collect::<Vec<_>>(), values);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("column");
        let mut writer = Writer::create(&path).unwrap();
        let image = column.save(&mut writer).unwrap();
        writer.finish(&image).unwrap();
        let reader = unsafe { Reader::open(&path).unwrap() };
        let loaded = Column::<T>::load(&reader, reader.metadata().unwrap()).unwrap();
        assert_eq!(loaded.iter().collect::<Vec<_>>(), values);
    }
    #[test]
    fn blocks_roundtrip_constants_extremes_signed_and_cross_word_widths() {
        roundtrip(Vec::<u64>::new());
        roundtrip(vec![u64::MAX; 1000]);
        roundtrip(vec![i64::MIN, -1, 0, 1, i64::MAX]);
        roundtrip((0..1031u32).map(|v| v * 13).collect());
        for width in 1..=64 {
            let mask = u64::MAX >> (64 - width);
            roundtrip(
                (0..777u64)
                    .map(|i| i.wrapping_mul(0x9e3779b97f4a7c15) & mask)
                    .collect(),
            );
        }
    }
    #[test]
    fn invalid_width_and_overflow_are_rejected() {
        for (base, width, value) in [(0, 65, 0), (u64::MAX, 1, 1), (u32::MAX as u64 + 1, 0, 0)] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("column");
            let mut writer = Writer::create(&path).unwrap();
            let image = ColumnImage {
                words: Packed::from(if width == 0 {
                    vec![]
                } else {
                    vec![value as u64]
                })
                .save(&mut writer)
                .unwrap(),
                bases: Packed::from(vec![base]).save(&mut writer).unwrap(),
                widths: Packed::from(vec![width as u8]).save(&mut writer).unwrap(),
                offsets: Packed::from(vec![0u32]).save(&mut writer).unwrap(),
                len: 1,
            };
            writer.finish(&image).unwrap();
            let reader = unsafe { Reader::open(&path).unwrap() };
            assert!(Column::<u32>::load(&reader, reader.metadata().unwrap()).is_err());
        }
    }
}
