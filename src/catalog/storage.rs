//! Immutable, aligned, checksummed little-endian segment containers.
use anyhow::{Result, ensure};
use bytemuck::Pod;
use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, Write},
    ops::Deref,
    path::Path,
    sync::Arc,
};

/// The on-disk segment format tag. A daemon and a segment worker must agree
/// on it; it travels in the build request so a mismatch fails before any
/// work rather than after (FIL-21).
pub const SEGMENT_FORMAT: &[u8; 8] = b"FXSEG004";

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Span {
    offset: u64,
    len: u64,
}
pub enum Packed<T: Pod> {
    Owned(Vec<T>),
    Mapped {
        map: Arc<Mmap>,
        span: Span,
        marker: std::marker::PhantomData<T>,
    },
}
impl<T: Pod> From<Vec<T>> for Packed<T> {
    fn from(v: Vec<T>) -> Self {
        Self::Owned(v)
    }
}
impl<T: Pod> std::fmt::Debug for Packed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Packed").field("len", &self.len()).finish()
    }
}
impl<T: Pod> Deref for Packed<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        match self {
            Self::Owned(v) => v,
            Self::Mapped { map, span, .. } => {
                bytemuck::cast_slice(&map[span.offset as usize..(span.offset + span.len) as usize])
            }
        }
    }
}
impl<T: Pod> Packed<T> {
    pub(crate) fn remap(&mut self, reader: &Reader) -> Result<()> {
        if let Self::Mapped { span, .. } = self {
            *self = Self::load(reader, *span)?;
        }
        Ok(())
    }

    pub fn capacity(&self) -> usize {
        match self {
            Self::Owned(v) => v.capacity(),
            _ => self.len(),
        }
    }
    pub fn save(&self, writer: &mut Writer) -> Result<Span> {
        writer.block(bytemuck::cast_slice(self))
    }
    pub fn load(reader: &Reader, span: Span) -> Result<Self> {
        let end = span
            .offset
            .checked_add(span.len)
            .ok_or_else(|| anyhow::anyhow!("span overflow"))?;
        ensure!(
            span.offset >= 64 && end <= reader.data_end && span.offset.is_multiple_of(64),
            "invalid segment span"
        );
        let bytes = &reader.map[span.offset as usize..end as usize];
        ensure!(
            bytemuck::try_cast_slice::<u8, T>(bytes).is_ok(),
            "invalid column alignment or width"
        );
        Ok(Self::Mapped {
            map: reader.map.clone(),
            span,
            marker: std::marker::PhantomData,
        })
    }
}

pub struct Writer {
    file: File,
}
impl Writer {
    pub fn create(path: &Path) -> Result<Self> {
        ensure!(
            cfg!(target_endian = "little"),
            "segment requires little endian"
        );
        let mut file = File::options()
            .create_new(true)
            .write(true)
            .read(true)
            .open(path)?;
        file.write_all(&[0; 64])?;
        Ok(Self { file })
    }
    pub fn block(&mut self, bytes: &[u8]) -> Result<Span> {
        let pos = self.file.stream_position()?;
        let padding = (64 - pos % 64) % 64;
        self.file.write_all(&vec![0; padding as usize])?;
        let offset = pos + padding;
        self.file.write_all(bytes)?;
        Ok(Span {
            offset,
            len: bytes.len() as u64,
        })
    }
    pub fn finish<T: Serialize>(mut self, metadata: &T) -> Result<()> {
        let span = self.block(&serde_json::to_vec(metadata)?)?;
        let data_end = self.file.stream_position()?;
        self.file.rewind()?;
        let mut header = [0; 64];
        header[..8].copy_from_slice(SEGMENT_FORMAT);
        header[8..16].copy_from_slice(&span.offset.to_le_bytes());
        header[16..24].copy_from_slice(&span.len.to_le_bytes());
        self.file.write_all(&header)?;
        self.file.flush()?;
        self.file.rewind()?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = self.file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        self.file.seek(std::io::SeekFrom::Start(data_end))?;
        self.file.write_all(&hash.finalize())?;
        self.file.sync_all()?;
        Ok(())
    }
}
pub struct Reader {
    file: File,
    map: Arc<Mmap>,
    data_end: u64,
    metadata: Span,
}
impl Reader {
    /// # Safety
    /// The generation owner must prevent external mutation for the map lifetime.
    pub unsafe fn open(path: &Path) -> Result<Self> {
        ensure!(
            cfg!(target_endian = "little"),
            "segment requires little endian"
        );
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        ensure!(len >= 96, "truncated segment");
        let data_end = len - 32;
        let mut hash = Sha256::new();
        let mut remaining = data_end;
        let mut buffer = [0u8; 65536];
        while remaining > 0 {
            let n = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..n])?;
            hash.update(&buffer[..n]);
            remaining -= n as u64;
        }
        let mut expected = [0u8; 32];
        file.read_exact(&mut expected)?;
        ensure!(
            hash.finalize().as_slice() == expected,
            "segment checksum mismatch"
        );
        let map = Arc::new(unsafe { Mmap::map(&file)? });
        ensure!(&map[..8] == SEGMENT_FORMAT, "unsupported segment version");
        let metadata = Span {
            offset: u64::from_le_bytes(map[8..16].try_into()?),
            len: u64::from_le_bytes(map[16..24].try_into()?),
        };
        ensure!(
            metadata.offset >= 64 && metadata.offset.checked_add(metadata.len) == Some(data_end),
            "invalid metadata span"
        );
        Ok(Self {
            file,
            map,
            data_end,
            metadata,
        })
    }
    /// A fresh view of the same immutable file releases validation residency
    /// portably, including platforms where DONTNEED is merely advisory.
    pub(crate) fn fresh_mapping(&self) -> Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            map: Arc::new(unsafe { Mmap::map(&self.file)? }),
            data_end: self.data_end,
            metadata: self.metadata,
        })
    }
    pub fn metadata<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(
            &self.map[self.metadata.offset as usize..self.data_end as usize],
        )?)
    }
}
impl<T: Pod> Default for Packed<T> {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

/// Large builds use temporary dictionaries and sort buffers. Return allocator
/// caches between phases instead of retaining them in the long-lived daemon.
pub(crate) fn release_builder_memory() {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // Null selects all malloc zones; zero requests maximal release of
        // unused allocations. Live Rust allocations remain valid.
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_segment(path: &Path, blocks: &[&[u8]]) -> Vec<Span> {
        let mut writer = Writer::create(path).unwrap();
        let spans = blocks.iter().map(|b| writer.block(b).unwrap()).collect();
        writer.finish(&serde_json::json!({ "k": 7 })).unwrap();
        spans
    }
    fn open(path: &Path) -> Reader {
        unsafe { Reader::open(path) }.unwrap()
    }
    fn open_err(path: &Path) -> String {
        match unsafe { Reader::open(path) } {
            Ok(_) => panic!("segment unexpectedly opened"),
            Err(err) => format!("{err:#}"),
        }
    }
    fn load_err<T: Pod>(reader: &Reader, offset: u64, len: u64) -> String {
        format!(
            "{:#}",
            Packed::<T>::load(reader, Span { offset, len }).unwrap_err()
        )
    }
    /// Writes a container by hand: header with `tag` and the metadata span,
    /// `body` from byte 64, then a valid SHA-256 trailer over everything.
    fn write_raw(path: &Path, tag: &[u8; 8], metadata: Span, body: &[u8]) {
        let mut bytes = vec![0u8; 64];
        bytes[..8].copy_from_slice(tag);
        bytes[8..16].copy_from_slice(&metadata.offset.to_le_bytes());
        bytes[16..24].copy_from_slice(&metadata.len.to_le_bytes());
        bytes.extend_from_slice(body);
        let digest = Sha256::digest(&bytes);
        bytes.extend_from_slice(&digest);
        std::fs::write(path, bytes).unwrap();
    }
    fn segment_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("segment.fx2")
    }

    #[test]
    fn roundtrip_columns_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        let small: Vec<u32> = vec![1, 2, 3];
        let wide: Vec<u64> = vec![u64::MAX, 0, 42, 7];
        let mut writer = Writer::create(&path).unwrap();
        let a = Packed::from(small.clone()).save(&mut writer).unwrap();
        let odd = writer.block(&[9; 5]).unwrap();
        let b = Packed::from(wide.clone()).save(&mut writer).unwrap();
        writer
            .finish(&serde_json::json!({ "name": "seg" }))
            .unwrap();
        for span in [a, odd, b] {
            assert!(
                span.offset >= 64 && span.offset.is_multiple_of(64),
                "{span:?}"
            );
        }
        assert_eq!((odd.offset, odd.len), (a.offset + 64, 5));
        assert_eq!(b.offset, odd.offset + 64);

        let reader = open(&path);
        let meta: serde_json::Value = reader.metadata().unwrap();
        assert_eq!(meta, serde_json::json!({ "name": "seg" }));
        assert_eq!(&*Packed::<u32>::load(&reader, a).unwrap(), &small[..]);
        assert_eq!(&*Packed::<u64>::load(&reader, b).unwrap(), &wide[..]);
    }

    #[test]
    fn empty_column_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        let mut writer = Writer::create(&path).unwrap();
        let span = Packed::<u64>::default().save(&mut writer).unwrap();
        writer.finish(&()).unwrap();
        assert_eq!((span.offset, span.len), (64, 0));
        let reader = open(&path);
        assert!(Packed::<u64>::load(&reader, span).unwrap().is_empty());
    }

    #[test]
    fn create_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        std::fs::write(&path, b"existing").unwrap();
        assert!(Writer::create(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
    }

    #[test]
    fn open_rejects_truncated_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        std::fs::write(&path, [0u8; 95]).unwrap();
        assert!(open_err(&path).contains("truncated segment"));
    }

    #[test]
    fn open_rejects_checksum_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        write_segment(&path, &[&[1, 2, 3, 4]]);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[64] ^= 0xff;
        std::fs::write(&path, bytes).unwrap();
        assert!(open_err(&path).contains("segment checksum mismatch"));
    }

    #[test]
    fn open_rejects_other_format_tag_with_valid_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        let metadata = Span { offset: 64, len: 2 };
        write_raw(&path, b"FXSEG003", metadata, b"{}");
        assert!(open_err(&path).contains("unsupported segment version"));
        // The same container with the current tag opens, so only the tag differed.
        write_raw(&path, SEGMENT_FORMAT, metadata, b"{}");
        open(&path);
    }

    #[test]
    fn open_rejects_metadata_span_not_ending_at_data_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        let short = Span { offset: 64, len: 1 };
        write_raw(&path, SEGMENT_FORMAT, short, b"{}");
        assert!(open_err(&path).contains("invalid metadata span"));
        let in_header = Span {
            offset: 32,
            len: 34,
        };
        write_raw(&path, SEGMENT_FORMAT, in_header, b"{}");
        assert!(open_err(&path).contains("invalid metadata span"));
        let overflow = Span {
            offset: 64,
            len: u64::MAX,
        };
        write_raw(&path, SEGMENT_FORMAT, overflow, b"{}");
        assert!(open_err(&path).contains("invalid metadata span"));
    }

    #[test]
    fn load_rejects_bad_spans() {
        let dir = tempfile::tempdir().unwrap();
        let path = segment_path(&dir);
        let spans = write_segment(&path, &[&[0u8; 16]]);
        assert_eq!((spans[0].offset, spans[0].len), (64, 16));
        let reader = open(&path);
        assert!(load_err::<u32>(&reader, u64::MAX, 2).contains("span overflow"));
        assert!(load_err::<u32>(&reader, 0, 16).contains("invalid segment span"));
        assert!(load_err::<u32>(&reader, 65, 4).contains("invalid segment span"));
        let past_end = reader.data_end - 64 + 1;
        assert!(load_err::<u32>(&reader, 64, past_end).contains("invalid segment span"));
        assert!(load_err::<u32>(&reader, 64, 3).contains("invalid column alignment or width"));
        assert!(load_err::<u64>(&reader, 64, 12).contains("invalid column alignment or width"));
        // The boundary itself is accepted.
        assert!(
            Packed::<u8>::load(
                &reader,
                Span {
                    offset: 64,
                    len: reader.data_end - 64,
                }
            )
            .is_ok()
        );
    }
}
