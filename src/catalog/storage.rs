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
        header[..8].copy_from_slice(b"FXSEG004");
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
        ensure!(
            matches!(&map[..8], b"FXSEG002" | b"FXSEG003" | b"FXSEG004"),
            "unsupported segment version"
        );
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
