//! Delta-varint postings with packed offsets and optional consecutive-ordinal runs.
use super::columns::{Column, ColumnImage};
use super::storage::{Packed, Reader, Span, Writer};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Default)]
pub struct Postings {
    offsets: Column<u32>,
    bytes: Packed<u8>,
    counts: Option<Packed<u32>>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum PostingsImage {
    Delta((ColumnImage, Span)),
    Runs {
        runs: (ColumnImage, Span),
        counts: Span,
    },
}
impl Postings {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        self.offsets.remap(reader)?;
        self.bytes.remap(reader)?;
        if let Some(counts) = &mut self.counts {
            counts.remap(reader)?;
        }
        Ok(())
    }

    pub fn build(lists: impl IntoIterator<Item = Vec<u32>>) -> Result<Self> {
        Self::build_internal(lists, false)
    }
    pub fn build_runs(lists: impl IntoIterator<Item = Vec<u32>>) -> Result<Self> {
        Self::build_internal(lists, true)
    }
    fn build_internal(lists: impl IntoIterator<Item = Vec<u32>>, runs: bool) -> Result<Self> {
        let mut counts = Vec::new();
        let mut offsets = Vec::new();
        let mut bytes = Vec::new();
        for list in lists {
            ensure!(bytes.len() <= u32::MAX as usize, "postings too large");
            offsets.push(bytes.len() as u32);
            ensure!(
                list.windows(2).all(|p| p[0] < p[1]),
                "postings must be strictly increasing"
            );
            if runs {
                counts.push(u32::try_from(list.len())?);
            }
            let mut previous = 0;
            let mut i = 0;
            while i < list.len() {
                if runs && i > 0 && list[i] - previous == 1 {
                    let start = i;
                    while i < list.len() && list[i] - previous == 1 {
                        previous = list[i];
                        i += 1;
                    }
                    let count = i - start;
                    if count >= 3 {
                        bytes.push(0);
                        put(&mut bytes, count as u32);
                    } else {
                        bytes.extend(std::iter::repeat_n(1, count));
                    }
                } else {
                    put(&mut bytes, list[i] - previous);
                    previous = list[i];
                    i += 1;
                }
            }
        }
        ensure!(bytes.len() <= u32::MAX as usize, "postings too large");
        offsets.push(bytes.len() as u32);
        offsets.shrink_to_fit();
        bytes.shrink_to_fit();
        Ok(Self {
            offsets: offsets.into(),
            bytes: bytes.into(),
            counts: runs.then(|| counts.into()),
        })
    }
    pub fn save(&self, writer: &mut Writer) -> Result<PostingsImage> {
        let spans = (self.offsets.save(writer)?, self.bytes.save(writer)?);
        Ok(match &self.counts {
            Some(counts) => PostingsImage::Runs {
                runs: spans,
                counts: counts.save(writer)?,
            },
            None => PostingsImage::Delta(spans),
        })
    }
    pub fn load(reader: &Reader, image: PostingsImage) -> Result<Self> {
        let (spans, counts) = match image {
            PostingsImage::Delta(spans) => (spans, None),
            PostingsImage::Runs { runs, counts } => {
                (runs, Some(Packed::<u32>::load(reader, counts)?))
            }
        };
        let result = Self {
            offsets: Column::load(reader, spans.0)?,
            bytes: Packed::load(reader, spans.1)?,
            counts,
        };
        ensure!(
            result.bytes.len() <= u32::MAX as usize
                && result.offsets.first() == Some(0)
                && result.offsets.last() == Some(result.bytes.len() as u32)
                && result.offsets.pairs().all(|p| p[0] <= p[1]),
            "invalid posting offsets"
        );
        ensure!(
            result
                .counts
                .as_ref()
                .is_none_or(|counts| counts.len() == result.len()),
            "invalid posting counts"
        );
        for (id, bounds) in result.offsets.pairs().enumerate() {
            ensure!(bounds[0] <= bounds[1], "unordered posting offsets");
            let mut bytes = &result.bytes[bounds[0] as usize..bounds[1] as usize];
            let mut previous = 0u32;
            let mut count = 0u64;
            while !bytes.is_empty() {
                let delta = take(&mut bytes)?;
                if count > 0 && delta == 0 {
                    ensure!(result.counts.is_some(), "duplicate posting ordinal");
                    let run = take(&mut bytes)?;
                    ensure!(run > 0, "empty posting run");
                    previous = previous
                        .checked_add(run)
                        .ok_or_else(|| anyhow::anyhow!("posting overflow"))?;
                    count += u64::from(run);
                } else {
                    previous = previous
                        .checked_add(delta)
                        .ok_or_else(|| anyhow::anyhow!("posting overflow"))?;
                    count += 1;
                }
            }
            if let Some(counts) = &result.counts {
                ensure!(count == u64::from(counts[id]), "posting count mismatch");
            }
        }
        Ok(result)
    }
    pub fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Cardinality is exact for run lists; delta byte length is an upper bound.
    pub fn cardinality_bound(&self, key: usize) -> usize {
        self.counts.as_ref().map_or_else(
            || (self.offsets.get(key + 1) - self.offsets.get(key)) as usize,
            |counts| counts[key] as usize,
        )
    }
    pub fn get(&self, name: u32) -> impl Iterator<Item = u32> + '_ {
        let mut bytes = &self.bytes[self.offsets.get(name as usize) as usize
            ..self.offsets.get(name as usize + 1) as usize];
        let mut previous = 0u32;
        let mut first = true;
        let mut remaining = 0u32;
        std::iter::from_fn(move || {
            if remaining > 0 {
                remaining -= 1;
                previous += 1;
                return Some(previous);
            }
            if bytes.is_empty() {
                return None;
            }
            let delta = take(&mut bytes).expect("validated posting varint");
            if !first && delta == 0 {
                remaining = take(&mut bytes).expect("validated posting run") - 1;
                previous += 1;
            } else {
                previous += delta;
            }
            first = false;
            Some(previous)
        })
    }

    pub fn bytes(&self) -> usize {
        self.offsets.bytes()
            + self.bytes.capacity()
            + self.counts.as_ref().map_or(0, |c| c.capacity() * 4)
    }
}
fn put(bytes: &mut Vec<u8>, mut value: u32) {
    while value >= 128 {
        bytes.push(value as u8 | 128);
        value >>= 7;
    }
    bytes.push(value as u8);
}
fn take(bytes: &mut &[u8]) -> Result<u32> {
    let mut value = 0u32;
    for shift in (0..=28).step_by(7) {
        let (&byte, rest) = bytes
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("truncated varint"))?;
        *bytes = rest;
        ensure!(shift != 28 || byte < 16, "invalid varint");
        value |= u32::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    anyhow::bail!("invalid varint")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn runs_roundtrip_preserves_cardinality_and_extremes() {
        let lists = vec![
            vec![],
            vec![0],
            (0..10000).collect(),
            vec![
                1,
                2,
                3,
                7,
                8,
                9,
                10,
                11,
                u32::MAX - 2,
                u32::MAX - 1,
                u32::MAX,
            ],
        ];
        let postings = Postings::build_runs(lists.clone()).unwrap();
        assert!(postings.bytes() < 200);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs");
        let mut writer = Writer::create(&path).unwrap();
        let image = postings.save(&mut writer).unwrap();
        writer.finish(&image).unwrap();
        let reader = unsafe { Reader::open(&path).unwrap() };
        let mapped = Postings::load(&reader, reader.metadata().unwrap()).unwrap();
        for (id, list) in lists.iter().enumerate() {
            assert_eq!(mapped.get(id as u32).collect::<Vec<_>>(), *list);
            assert_eq!(mapped.cardinality_bound(id), list.len());
        }
    }
    #[test]
    fn rejects_bad_runs_counts_and_offsets() {
        for (bytes, offsets, count) in [
            (vec![0, 0], vec![0, 2], 2),                         // truncated run
            (vec![0, 0, 0], vec![0, 3], 1),                      // empty run
            (vec![0, 0, 4], vec![0, 3], 4),                      // incorrect cardinality
            (vec![255, 255, 255, 255, 15, 0, 1], vec![0, 7], 2), // overflow
            (vec![1], vec![0, 2, 1], 1),                         // out-of-bounds offset
        ] {
            let postings = Postings {
                bytes: bytes.into(),
                offsets: offsets.into(),
                counts: Some(vec![count].into()),
            };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("bad");
            let mut writer = Writer::create(&path).unwrap();
            let image = postings.save(&mut writer).unwrap();
            writer.finish(&image).unwrap();
            let reader = unsafe { Reader::open(&path).unwrap() };
            assert!(Postings::load(&reader, reader.metadata().unwrap()).is_err());
        }
    }
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
