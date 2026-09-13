//! Lossless native path encoding for local IPC and persistence.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::{Path, PathBuf};
pub fn serialize<S: Serializer>(path: &Path, s: S) -> Result<S::Ok, S::Error> {
    super::segment::raw_name(path.as_os_str()).serialize(s)
}
pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
    Ok(PathBuf::from(super::segment::os_name(
        &Vec::<u8>::deserialize(d)?,
    )))
}
pub mod optional {
    use super::*;
    pub fn serialize<S: Serializer>(path: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
        path.as_ref()
            .map(|p| super::super::segment::raw_name(p.as_os_str()))
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<PathBuf>, D::Error> {
        Ok(Option::<Vec<u8>>::deserialize(d)?
            .map(|b| PathBuf::from(super::super::segment::os_name(&b))))
    }
}
pub mod list {
    use super::*;
    pub fn serialize<S: Serializer>(paths: &[PathBuf], s: S) -> Result<S::Ok, S::Error> {
        paths
            .iter()
            .map(|p| super::super::segment::raw_name(p.as_os_str()))
            .collect::<Vec<_>>()
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<PathBuf>, D::Error> {
        Ok(Vec::<Vec<u8>>::deserialize(d)?
            .into_iter()
            .map(|b| PathBuf::from(super::super::segment::os_name(&b)))
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
                    .map(|p| super::super::segment::raw_name(p.as_os_str()))
                    .collect::<Vec<_>>()
            })
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<PathBuf>>, D::Error> {
        Ok(Option::<Vec<Vec<u8>>>::deserialize(d)?.map(|v| {
            v.into_iter()
                .map(|b| PathBuf::from(super::super::segment::os_name(&b)))
                .collect()
        }))
    }
}
