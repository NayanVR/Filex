use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{Read, Write};
const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_PATHS: usize = 64;
#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Thumbnail(Vec<u16>),
    Present {
        paths: Vec<Vec<u16>>,
        selected: usize,
        owner: isize,
        request_id: u64,
    },
    Close,
}
#[derive(Serialize, Deserialize, Debug)]
pub enum Event {
    Pixels {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    },
    Ready {
        kind: String,
        request_id: u64,
    },
    Heartbeat,
    Closed,
    Open(Vec<u16>),
    Error(String),
}
pub fn write<T: Serialize>(writer: &mut impl Write, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= MAX_FRAME, "preview message too large");
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}
pub fn read<T: DeserializeOwned>(reader: &mut impl Read) -> Result<T> {
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let size = u32::from_le_bytes(size) as usize;
    ensure!(
        size > 0 && size <= MAX_FRAME,
        "invalid preview message size"
    );
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).context("invalid preview message")
}
pub fn validate_path(path: &[u16]) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 32_767 && !path.contains(&0),
        "invalid preview path"
    );
    Ok(())
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Thumbnail(path) => validate_path(path),
            Self::Present {
                paths, selected, ..
            } => {
                ensure!(
                    !paths.is_empty() && paths.len() <= MAX_PATHS && *selected < paths.len(),
                    "invalid preview selection"
                );
                paths.iter().try_for_each(|p| validate_path(p))
            }
            Self::Close => Ok(()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn framing_rejects_oversize_truncated_and_invalid_messages() {
        assert!(read::<Request>(&mut &u32::MAX.to_le_bytes()[..]).is_err());
        assert!(read::<Request>(&mut &[8, 0, 0, 0, 42][..]).is_err());
        let mut event_bytes = Vec::new();
        write(&mut event_bytes, &Event::Closed).unwrap();
        assert!(matches!(
            read::<Event>(&mut event_bytes.as_slice()).unwrap(),
            Event::Closed
        ));
        let mut bytes = Vec::new();
        write(&mut bytes, &Request::Thumbnail(vec![0xd800, 65])).unwrap();
        let decoded: Request = read(&mut bytes.as_slice()).unwrap();
        assert!(matches!(decoded, Request::Thumbnail(v) if v == vec![0xd800, 65]));
    }
    #[test]
    fn validates_native_path_and_selection_bounds() {
        assert!(validate_path(&[65, 0, 66]).is_err());
        assert!(validate_path(&vec![65; 32768]).is_err());
        assert!(
            Request::Present {
                paths: vec![vec![65]],
                selected: 1,
                owner: 0,
                request_id: 1,
            }
            .validate()
            .is_err()
        );
        assert!(
            Request::Present {
                paths: vec![vec![65]; MAX_PATHS + 1],
                selected: 0,
                owner: 0,
                request_id: 1,
            }
            .validate()
            .is_err()
        );
    }
}
