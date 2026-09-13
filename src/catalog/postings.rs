//! Delta-varint postings in file-rank (ascending ordinal) order.
use super::storage::{Packed, Reader, Span, Writer};
use anyhow::{Result, ensure};

#[derive(Default)]
pub struct Postings {
    offsets: Packed<u32>,
    bytes: Packed<u8>,
}
impl Postings {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.offsets.remap(reader)?;
        self.bytes.remap(reader)?;
        Ok(())
    }

    pub fn build(lists: impl IntoIterator<Item = Vec<u32>>) -> Result<Self> {
        let mut offsets = Vec::new();
        let mut bytes = Vec::new();
        for list in lists {
            ensure!(bytes.len() <= u32::MAX as usize, "postings too large");
            offsets.push(bytes.len() as u32);
            let mut previous = 0;
            for (i, value) in list.into_iter().enumerate() {
                ensure!(
                    i == 0 || value > previous,
                    "postings must be strictly increasing"
                );
                let mut delta = value - previous;
                previous = value;
                while delta >= 128 {
                    bytes.push(delta as u8 | 128);
                    delta >>= 7;
                }
                bytes.push(delta as u8);
            }
        }
        ensure!(bytes.len() <= u32::MAX as usize, "postings too large");
        offsets.push(bytes.len() as u32);
        offsets.shrink_to_fit();
        bytes.shrink_to_fit();
        Ok(Self {
            offsets: offsets.into(),
            bytes: bytes.into(),
        })
    }
    pub fn save(&self, writer: &mut Writer) -> Result<(Span, Span)> {
        Ok((self.offsets.save(writer)?, self.bytes.save(writer)?))
    }
    pub fn load(reader: &Reader, spans: (Span, Span)) -> Result<Self> {
        let result = Self {
            offsets: Packed::load(reader, spans.0)?,
            bytes: Packed::load(reader, spans.1)?,
        };
        ensure!(
            result.offsets.first() == Some(&0)
                && result.offsets.last().copied() == Some(result.bytes.len() as u32),
            "invalid posting offsets"
        );
        for bounds in result.offsets.windows(2) {
            ensure!(bounds[0] <= bounds[1], "unordered posting offsets");
            let mut value = 0u32;
            let mut shift = 0;
            let mut previous = 0u32;
            let mut first = true;
            for &byte in &result.bytes[bounds[0] as usize..bounds[1] as usize] {
                ensure!(shift < 32 && (shift != 28 || byte < 16), "invalid varint");
                value |= u32::from(byte & 127) << shift;
                if byte & 128 == 0 {
                    ensure!(first || value > 0, "duplicate posting ordinal");
                    first = false;
                    previous = previous
                        .checked_add(value)
                        .ok_or_else(|| anyhow::anyhow!("posting overflow"))?;
                    value = 0;
                    shift = 0;
                } else {
                    shift += 7;
                }
            }
            ensure!(shift == 0, "truncated varint");
        }
        Ok(result)
    }
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn encoded_len(&self, key: usize) -> usize {
        (self.offsets[key + 1] - self.offsets[key]) as usize
    }
    pub fn get(&self, name: u32) -> impl Iterator<Item = u32> + '_ {
        let mut bytes = &self.bytes
            [self.offsets[name as usize] as usize..self.offsets[name as usize + 1] as usize];
        let mut previous = 0u32;
        std::iter::from_fn(move || {
            if bytes.is_empty() {
                return None;
            }
            let mut value = 0u32;
            let mut shift = 0;
            loop {
                let byte = bytes[0];
                bytes = &bytes[1..];
                value |= u32::from(byte & 127) << shift;
                if byte & 128 == 0 {
                    break;
                }
                shift += 7;
            }
            previous += value;
            Some(previous)
        })
    }
    pub fn bytes(&self) -> usize {
        self.offsets.capacity() * 4 + self.bytes.capacity()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delta_roundtrip_and_rank_validation() {
        let lists = vec![vec![], vec![0, 127, 128, 16384, u32::MAX], vec![1, 1000000]];
        let postings = Postings::build(lists.clone()).unwrap();
        for (id, list) in lists.iter().enumerate() {
            assert_eq!(postings.get(id as u32).collect::<Vec<_>>(), *list);
        }
        assert!(Postings::build([vec![4, 3]]).is_err());
        assert!(Postings::build([vec![0, 0]]).is_err());
    }
}
