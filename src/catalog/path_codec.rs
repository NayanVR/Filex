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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// One field per adapter, so a single JSON round-trip exercises all four.
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Paths {
        #[serde(with = "crate::catalog::path_codec")]
        one: PathBuf,
        #[serde(with = "crate::catalog::path_codec::optional")]
        maybe: Option<PathBuf>,
        #[serde(with = "crate::catalog::path_codec::list")]
        many: Vec<PathBuf>,
        #[serde(with = "crate::catalog::path_codec::optional_list")]
        maybe_many: Option<Vec<PathBuf>>,
    }

    fn round_trip(value: &Paths) -> Paths {
        let json = serde_json::to_string(value).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    fn assert_name_round_trips(name: &OsStr) {
        assert_eq!(os_name(&raw_name(name)), name, "{name:?}");
    }

    #[test]
    fn ascii_utf8_and_empty_paths_round_trip_through_every_adapter() {
        for path in ["/tmp/plain.txt", "/tmp/é/日本", ""] {
            let path = PathBuf::from(path);
            let value = Paths {
                one: path.clone(),
                maybe: Some(path.clone()),
                many: vec![path.clone()],
                maybe_many: Some(vec![path.clone()]),
            };
            assert_eq!(round_trip(&value), value);
        }
    }

    #[test]
    fn none_and_empty_lists_round_trip() {
        let value = Paths {
            one: PathBuf::from("/a"),
            maybe: None,
            many: Vec::new(),
            maybe_many: None,
        };
        assert_eq!(round_trip(&value), value);

        let empty_some = Paths {
            maybe_many: Some(Vec::new()),
            ..value
        };
        assert_eq!(round_trip(&empty_some), empty_some);
    }

    #[test]
    fn multi_element_lists_keep_order() {
        let paths: Vec<PathBuf> = ["/c", "/a", "/b/日本", ""]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let value = Paths {
            one: PathBuf::from("/x"),
            maybe: Some(PathBuf::from("/y")),
            many: paths.clone(),
            maybe_many: Some(paths),
        };
        assert_eq!(round_trip(&value), value);
    }

    #[test]
    fn utf8_names_encode_as_their_utf8_bytes() {
        for text in ["", "a", "é/日本"] {
            assert_eq!(raw_name(OsStr::new(text)), text.as_bytes());
            assert_eq!(os_name(text.as_bytes()), OsStr::new(text));
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_non_utf8_bytes_round_trip() {
        use std::os::unix::ffi::OsStrExt;
        let raw = [b'a', 0xFF, b'b'];
        assert_eq!(raw_name(&os_name(&raw)), raw);
        let name = OsStr::from_bytes(&raw);
        assert_name_round_trips(name);

        let value = Paths {
            one: PathBuf::from(name),
            maybe: Some(PathBuf::from(name)),
            many: vec![PathBuf::from(name)],
            maybe_many: Some(vec![PathBuf::from(name)]),
        };
        assert_eq!(round_trip(&value), value);
    }

    #[cfg(unix)]
    #[test]
    fn unix_every_single_byte_name_round_trips() {
        for byte in 0..=u8::MAX {
            assert_eq!(raw_name(&os_name(&[byte])), [byte]);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_lone_surrogate_uses_the_utf16_marker_and_round_trips() {
        use std::os::windows::ffi::OsStringExt;
        let name = OsString::from_wide(&[0xD800]);
        let raw = raw_name(&name);
        assert_eq!(raw, [0xFF, 0xFE, 0x00, 0xD8]);
        assert_eq!(os_name(&raw), name);

        let mixed = OsString::from_wide(&[u16::from(b'a'), 0xDC00, u16::from(b'b')]);
        assert!(raw_name(&mixed).starts_with(&[0xFF, 0xFE]));
        assert_name_round_trips(&mixed);

        let value = Paths {
            one: PathBuf::from(&mixed),
            maybe: Some(PathBuf::from(&mixed)),
            many: vec![PathBuf::from(&mixed)],
            maybe_many: Some(vec![PathBuf::from(&mixed)]),
        };
        assert_eq!(round_trip(&value), value);
    }

    #[cfg(windows)]
    #[test]
    fn windows_valid_utf16_is_stored_as_plain_utf8_without_marker() {
        for text in ["", "C:\\plain", "C:\\é\\日本", "\u{FEFF}bom", "\u{1F600}"] {
            let raw = raw_name(OsStr::new(text));
            assert_eq!(raw, text.as_bytes());
            // UTF-8 never starts with 0xFF, so a leading 0xFF byte can only
            // be the UTF-16 marker.
            assert_ne!(raw.first(), Some(&0xFF), "{text:?}");
            assert_name_round_trips(OsStr::new(text));
        }
    }
}
