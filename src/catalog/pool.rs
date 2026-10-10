//! Independently compressed text pages with a bounded decoded-page cache.
//! Logical offsets stay stable, including names that cross page boundaries.
use super::storage::{Packed, Reader, Span, Writer};
use anyhow::{Result, ensure};
use flate2::{Compression, Decompress, FlushDecompress, Status, write::DeflateEncoder};
use memmap2::MmapMut;
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::VecDeque,
    io::Write,
    ops::{Deref, Range},
    sync::{Arc, Mutex},
};

const PAGE: usize = 65536;
const CACHE_PAGES: usize = 16;
type Cache = Mutex<VecDeque<(usize, Arc<Vec<u8>>)>>;

pub(crate) enum ValidationText<'a> {
    Borrowed(&'a [u8]),
    Mapped(MmapMut),
}
impl Deref for ValidationText<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Borrowed(bytes) => bytes,
            Self::Mapped(map) => map,
        }
    }
}

// Disk pages favor compression ratio. This non-persisted representation favors
// decoding speed and replaces their resident working set after fresh mapping.
#[derive(Default)]
pub(crate) struct FastPages {
    data: Option<MmapMut>,
    len: usize,
    offsets: Vec<usize>,
}
impl FastPages {
    fn new(decoded_len: usize) -> Result<Self> {
        let pages = decoded_len.div_ceil(PAGE);
        let capacity = pages
            .checked_mul(lz4_flex::block::get_maximum_output_size(PAGE))
            .ok_or_else(|| anyhow::anyhow!("runtime text capacity overflow"))?;
        Ok(Self {
            data: if capacity == 0 {
                None
            } else {
                Some(MmapMut::map_anon(capacity)?)
            },
            len: 0,
            offsets: Vec::with_capacity(pages),
        })
    }
    fn push(&mut self, bytes: &[u8]) {
        // Reserve virtual address space once, but touch only encoded bytes.
        // Growing/shrinking a large Vec left freed buffers in allocator caches.
        self.offsets.push(self.len);
        let maximum = lz4_flex::block::get_maximum_output_size(bytes.len());
        self.len += lz4_flex::block::compress_into(
            bytes,
            &mut self.data.as_mut().unwrap()[self.len..self.len + maximum],
        )
        .expect("sized runtime page buffer");
    }
    fn finish(&mut self) {
        self.offsets.shrink_to_fit();
    }
    fn decode(&self, id: usize, len: usize) -> Vec<u8> {
        let end = self.offsets.get(id + 1).copied().unwrap_or(self.len);
        let mut output = vec![0; len];
        let written = lz4_flex::block::decompress_into(
            &self.data.as_ref().unwrap()[self.offsets[id]..end],
            &mut output,
        )
        .expect("internally encoded LZ4 page");
        assert_eq!(written, len);
        output
    }
}

pub(crate) enum BytePool {
    Plain(Packed<u8>),
    Deflate {
        data: Packed<u8>,
        offsets: Packed<u32>,
        len: usize,
        cache: Cache,
        fast: FastPages,
        resident: Option<Vec<u8>>,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum PoolImage {
    Plain(Span),
    Deflate {
        deflate: Span,
        page_offsets: Span,
        decoded_len: usize,
        page_size: usize,
    },
}
impl BytePool {
    pub fn plain(bytes: Vec<u8>) -> Self {
        Self::Plain(bytes.into())
    }
    pub fn build(bytes: Vec<u8>) -> Result<Self> {
        Self::build_with_access(bytes, false)
    }
    pub fn build_search(bytes: Vec<u8>) -> Result<Self> {
        Self::build_with_access(bytes, true)
    }
    fn build_with_access(bytes: Vec<u8>, resident: bool) -> Result<Self> {
        let mut data = Vec::new();
        let mut offsets = vec![0u32];
        for page in bytes.chunks(PAGE) {
            let mut encoder = DeflateEncoder::new(Vec::new(), Compression::best());
            encoder.write_all(page)?;
            data.extend(encoder.finish()?);
            offsets.push(u32::try_from(data.len())?);
        }
        if data.len() + offsets.len() * 4 >= bytes.len() {
            return Ok(Self::plain(bytes));
        }
        data.shrink_to_fit();
        offsets.shrink_to_fit();
        let mut fast = if resident {
            FastPages::default()
        } else {
            FastPages::new(bytes.len())?
        };
        if !resident {
            for page in bytes.chunks(PAGE) {
                fast.push(page);
            }
            fast.finish();
        }
        let len = bytes.len();
        let resident = resident.then_some(bytes);
        Ok(Self::Deflate {
            data: data.into(),
            offsets: offsets.into(),
            len,
            cache: Mutex::default(),
            fast,
            resident,
        })
    }
    pub fn len(&self) -> usize {
        match self {
            Self::Plain(data) => data.len(),
            Self::Deflate { len, .. } => *len,
        }
    }
    pub fn bytes(&self) -> usize {
        match self {
            Self::Plain(data) => data.capacity(),
            Self::Deflate {
                data,
                offsets,
                fast,
                resident,
                ..
            } => {
                data.capacity()
                    + offsets.capacity() * 4
                    + fast.len
                    + fast.offsets.capacity() * std::mem::size_of::<usize>()
                    + resident.as_ref().map_or(0, Vec::capacity)
            }
        }
    }
    pub fn read(&self, range: Range<usize>) -> Cow<'_, [u8]> {
        assert!(range.start <= range.end && range.end <= self.len());
        match self {
            Self::Plain(data) => Cow::Borrowed(&data[range]),
            Self::Deflate {
                resident: Some(data),
                ..
            } => Cow::Borrowed(&data[range]),
            Self::Deflate { .. } => {
                let mut result = Vec::with_capacity(range.len());
                let mut at = range.start;
                while at < range.end {
                    let page = self.page(at / PAGE);
                    let end = range.end.min((at / PAGE + 1) * PAGE);
                    result.extend_from_slice(&page[at % PAGE..at % PAGE + end - at]);
                    at = end;
                }
                Cow::Owned(result)
            }
        }
    }
    /// Startup validation needs random access to all native names. An anonymous
    /// mapping is released on drop instead of becoming a large malloc cache.
    pub fn validation_text(&self) -> Result<ValidationText<'_>> {
        match self {
            Self::Plain(data) => Ok(ValidationText::Borrowed(data)),
            Self::Deflate {
                resident: Some(data),
                ..
            } => Ok(ValidationText::Borrowed(data)),
            _ if self.len() == 0 => Ok(ValidationText::Borrowed(&[])),
            Self::Deflate { .. } => {
                let mut map = MmapMut::map_anon(self.len())?;
                for (id, target) in map.chunks_mut(PAGE).enumerate() {
                    target.copy_from_slice(&self.page(id));
                }
                Ok(ValidationText::Mapped(map))
            }
        }
    }
    fn decode(&self, id: usize) -> Result<Vec<u8>> {
        let Self::Deflate {
            data, offsets, len, ..
        } = self
        else {
            unreachable!()
        };
        let input = &data[offsets[id] as usize..offsets[id + 1] as usize];
        let expected = PAGE.min(len - id * PAGE);
        // One extra byte detects expansion beyond the declared logical length.
        let mut output = vec![0; expected + 1];
        let mut decoder = Decompress::new(false);
        let status = decoder.decompress(input, &mut output, FlushDecompress::Finish)?;
        ensure!(
            status == Status::StreamEnd
                && decoder.total_in() == input.len() as u64
                && decoder.total_out() == expected as u64,
            "invalid compressed text page"
        );
        output.truncate(expected);
        Ok(output)
    }
    fn page(&self, id: usize) -> Arc<Vec<u8>> {
        let Self::Deflate {
            cache, fast, len, ..
        } = self
        else {
            unreachable!()
        };
        {
            let mut cache = cache.lock().unwrap();
            if let Some(at) = cache.iter().position(|(key, _)| *key == id) {
                let entry = cache.remove(at).unwrap();
                let result = entry.1.clone();
                cache.push_front(entry);
                return result;
            }
        }
        // All pages are checked at load; the immutable mapping cannot change.
        let page = Arc::new(fast.decode(id, PAGE.min(len - id * PAGE)));
        let mut cache = cache.lock().unwrap();
        if cache.len() == CACHE_PAGES {
            cache.pop_back();
        }
        cache.push_front((id, page.clone()));
        page
    }
    pub fn remap(&mut self, reader: &Reader) -> Result<()> {
        match self {
            Self::Plain(data) => data.remap(reader)?,
            Self::Deflate {
                data,
                offsets,
                cache,
                ..
            } => {
                data.remap(reader)?;
                offsets.remap(reader)?;
                cache.get_mut().unwrap().clear();
            }
        }
        Ok(())
    }
    pub fn save(&self, writer: &mut Writer) -> Result<PoolImage> {
        Ok(match self {
            Self::Plain(data) => PoolImage::Plain(data.save(writer)?),
            Self::Deflate {
                data, offsets, len, ..
            } => PoolImage::Deflate {
                deflate: data.save(writer)?,
                page_offsets: offsets.save(writer)?,
                decoded_len: *len,
                page_size: PAGE,
            },
        })
    }
    pub fn load(reader: &Reader, image: PoolImage) -> Result<Self> {
        Self::load_with_access(reader, image, false)
    }
    pub fn load_search(reader: &Reader, image: PoolImage) -> Result<Self> {
        Self::load_with_access(reader, image, true)
    }
    fn load_with_access(reader: &Reader, image: PoolImage, resident: bool) -> Result<Self> {
        let mut result = match image {
            PoolImage::Plain(span) => Self::Plain(Packed::load(reader, span)?),
            PoolImage::Deflate {
                deflate,
                page_offsets,
                decoded_len,
                page_size,
            } => {
                ensure!(
                    page_size == PAGE && decoded_len <= u32::MAX as usize,
                    "invalid text dimensions"
                );
                let data = Packed::load(reader, deflate)?;
                let offsets = Packed::<u32>::load(reader, page_offsets)?;
                ensure!(
                    data.len() <= u32::MAX as usize
                        && offsets.len() == decoded_len.div_ceil(PAGE) + 1
                        && offsets.first() == Some(&0)
                        && offsets.last().copied() == Some(data.len() as u32)
                        && offsets.windows(2).all(|p| p[0] < p[1]),
                    "invalid compressed text offsets"
                );
                Self::Deflate {
                    data,
                    offsets,
                    len: decoded_len,
                    cache: Mutex::default(),
                    fast: FastPages::default(),
                    resident: None,
                }
            }
        };
        if matches!(result, Self::Deflate { .. }) {
            let mut fast_pages = if resident {
                FastPages::default()
            } else {
                FastPages::new(result.len())?
            };
            let mut text = if resident {
                Vec::with_capacity(result.len())
            } else {
                Vec::new()
            };
            for id in 0..result.len().div_ceil(PAGE) {
                let page = result.decode(id)?;
                if resident {
                    text.extend_from_slice(&page);
                } else {
                    fast_pages.push(&page);
                }
            }
            fast_pages.finish();
            let Self::Deflate {
                fast,
                resident: decoded,
                ..
            } = &mut result
            else {
                unreachable!()
            };
            *fast = fast_pages;
            *decoded = resident.then_some(text);
        }
        Ok(result)
    }

    pub fn cursor(&self) -> Cursor<'_> {
        Cursor {
            pool: self,
            page: None,
            scratch: Vec::new(),
        }
    }
}

/// A scan holds its current page without copying names or locking the shared
/// cache for every candidate. Crossing names use one reusable scratch buffer.
pub(crate) struct Cursor<'a> {
    pool: &'a BytePool,
    page: Option<(usize, Arc<Vec<u8>>)>,
    scratch: Vec<u8>,
}
impl Cursor<'_> {
    pub fn read(&mut self, range: Range<usize>) -> &[u8] {
        assert!(range.start <= range.end && range.end <= self.pool.len());
        match self.pool {
            BytePool::Plain(bytes) => return &bytes[range],
            BytePool::Deflate {
                resident: Some(bytes),
                ..
            } => return &bytes[range],
            _ => {}
        }
        if range.is_empty() {
            return &[];
        }
        let id = range.start / PAGE;
        if (range.end - 1) / PAGE == id {
            if self.page.as_ref().is_none_or(|(key, _)| *key != id) {
                self.page = Some((id, self.pool.page(id)));
            }
            return &self.page.as_ref().unwrap().1
                [range.start % PAGE..range.start % PAGE + range.len()];
        }
        self.scratch.clear();
        let mut at = range.start;
        while at < range.end {
            let id = at / PAGE;
            if self.page.as_ref().is_none_or(|(key, _)| *key != id) {
                self.page = Some((id, self.pool.page(id)));
            }
            let end = range.end.min((id + 1) * PAGE);
            self.scratch
                .extend_from_slice(&self.page.as_ref().unwrap().1[at % PAGE..at % PAGE + end - at]);
            at = end;
        }
        &self.scratch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_and_native_access_keep_identical_disk_bytes() {
        let bytes: Vec<_> = (0..PAGE * 3 + 19).map(|n| (n % 251) as u8).collect();
        let dir = tempfile::tempdir().unwrap();
        for (file, pool) in [
            ("paged", BytePool::build(bytes.clone()).unwrap()),
            ("resident", BytePool::build_search(bytes.clone()).unwrap()),
        ] {
            let mut writer = Writer::create(&dir.path().join(file)).unwrap();
            let image = pool.save(&mut writer).unwrap();
            writer.finish(&image).unwrap();
        }
        assert_eq!(
            std::fs::read(dir.path().join("paged")).unwrap(),
            std::fs::read(dir.path().join("resident")).unwrap()
        );
        let reader = unsafe { Reader::open(&dir.path().join("paged")).unwrap() };
        let mut pool = BytePool::load_search(&reader, reader.metadata().unwrap()).unwrap();
        assert!(matches!(pool.read(0..bytes.len()), Cow::Borrowed(_)));
        pool.remap(&reader.fresh_mapping().unwrap()).unwrap();
        assert_eq!(&*pool.read(0..bytes.len()), &bytes);
        assert_eq!(
            pool.cursor().read(PAGE - 2..PAGE + 3),
            &bytes[PAGE - 2..PAGE + 3]
        );
    }
    #[test]
    fn pages_ranges_roundtrip_and_bounded_cache() {
        let bytes: Vec<_> = (0..PAGE * 20 + 7).map(|n| (n % 251) as u8).collect();
        let pool = BytePool::build(bytes.clone()).unwrap();
        assert!(pool.bytes() < bytes.len() / 10);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pages");
        let mut writer = Writer::create(&path).unwrap();
        let image = pool.save(&mut writer).unwrap();
        writer.finish(&image).unwrap();
        let reader = unsafe { Reader::open(&path).unwrap() };
        let pool = BytePool::load(&reader, reader.metadata().unwrap()).unwrap();
        let mut cursor = pool.cursor();
        for range in [
            0..0,
            0..1,
            PAGE - 3..PAGE + 9,
            0..bytes.len(),
            bytes.len()..bytes.len(),
        ] {
            assert_eq!(&*pool.read(range.clone()), &bytes[range]);
        }
        for range in [
            0..0,
            0..7,
            PAGE - 4..PAGE + 4,
            PAGE..PAGE + 4,
            PAGE * 19 - 1..bytes.len(),
            bytes.len()..bytes.len(),
            3..7,
        ] {
            assert_eq!(cursor.read(range.clone()), &bytes[range]);
        }
        // A held page remains valid when another reader evicts it from the LRU.
        assert_eq!(cursor.read(3..7), &bytes[3..7]);
        for id in 1..20 {
            pool.read(id * PAGE..id * PAGE + 1);
        }
        assert_eq!(cursor.read(4..8), &bytes[4..8]);
        let plain = BytePool::plain(bytes.clone());
        assert_eq!(
            plain.cursor().read(PAGE - 1..PAGE + 1),
            &bytes[PAGE - 1..PAGE + 1]
        );
        if let BytePool::Deflate { cache, .. } = &pool {
            assert_eq!(cache.lock().unwrap().len(), CACHE_PAGES);
        }
        assert_eq!(&*BytePool::build(vec![]).unwrap().read(0..0), b"");
    }
    #[test]
    fn malformed_pages_fail_without_unbounded_expansion() {
        let pool = BytePool::build(vec![42; PAGE * 2]).unwrap();
        let BytePool::Deflate { data, offsets, .. } = pool else {
            panic!()
        };
        let wrong_len = BytePool::Deflate {
            data,
            offsets,
            len: PAGE + 1,
            cache: Mutex::default(),
            fast: FastPages::default(),
            resident: None,
        };
        assert!(wrong_len.decode(1).is_err());
        let truncated = BytePool::Deflate {
            data: vec![0xff].into(),
            offsets: vec![0, 1].into(),
            len: PAGE,
            cache: Mutex::default(),
            fast: FastPages::default(),
            resident: None,
        };
        assert!(truncated.decode(0).is_err());
    }
}
