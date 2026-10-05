//! Lossless native path encoding for local IPC and persistence.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Decode a stored name back to the OS string. Unix: names are raw bytes.
/// Windows: valid-UTF-16 names are stored as UTF-8; anything else (unpaired
/// surrogates) as a `FF FE` marker followed by little-endian UTF-16 units.
/// UTF-8 can never begin with `FF`, so the marker is unambiguous.
pub fn os_name(raw: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(raw.to_vec())
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        if raw.starts_with(&[255, 254]) {
            OsString::from_wide(
                &raw[2..]
                    .chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]))
                    .collect::<Vec<_>>(),
            )
        } else {
            OsString::from(String::from_utf8_lossy(raw).into_owned())
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        OsString::from(String::from_utf8_lossy(raw).into_owned())
    }
}
/// Encode an OS name losslessly for storage; inverse of [`os_name`].
pub fn raw_name(name: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        name.as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        if let Some(text) = name.to_str() {
            text.as_bytes().to_vec()
        } else {
            let mut bytes = vec![255, 254];
            for unit in name.encode_wide() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            bytes
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        name.to_string_lossy().as_bytes().to_vec()
    }
}

pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    raw_name(path.as_os_str()).serialize(s)
}
pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
    Ok(PathBuf::from(os_name(&Vec::<u8>::deserialize(d)?)))
}
pub mod optional {
    use super::*;
    pub fn serialize<S: Serializer>(path: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
        path.as_ref()
            .map(|p| super::raw_name(p.as_os_str()))
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
        Ok(Option::<Vec<u8>>::deserialize(d)?.map(|b| PathBuf::from(super::os_name(&b))))
    }
}
pub mod list {
    use super::*;
    pub fn serialize<S: Serializer>(paths: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
        paths
            .iter()
            .map(|p| super::raw_name(p.as_os_str()))
            .collect::<Vec<_>>()
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<Vec<u8>>::deserialize(d)?
            .into_iter()
            .map(|b| PathBuf::from(super::os_name(&b)))
            .collect())
    }
}
pub mod optional_list {
    use super::*;
    pub fn serialize<S: Serializer>(paths: &Option<Vec<PathBuf>>, s: S) -> Result<S::Ok, S::Error> {
        paths
            .as_ref()
            .map(|v| {
                v.iter()
                    .map(|p| super::raw_name(p.as_os_str()))
                    .collect::<Vec<_>>()
            })
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<PathBuf>>, D::Error> {
        Ok(Option::<Vec<Vec<u8>>>::deserialize(d)?.map(|v| {
            v.into_iter()
                .map(|b| PathBuf::from(super::os_name(&b)))
                .collect()
        }))
    }
}
