use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::{Read, Write};
const MAX_FRAME: usize = 1024 * 1024;
/// Largest embedded surface edge, in physical pixels. Bounds hostile or
/// corrupt geometry before it reaches `CreateWindowExW`/`SetRect`.
pub const MAX_EDGE: i32 = 32_768;

/// Where the helper places its surface: physical pixels, relative to the
/// parent window's client area.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl Rect {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (0..=MAX_EDGE).contains(&self.x)
                && (0..=MAX_EDGE).contains(&self.y)
                && (1..=MAX_EDGE).contains(&self.width)
                && (1..=MAX_EDGE).contains(&self.height),
            "invalid preview surface bounds"
        );
        Ok(())
    }
}

/// Keys the embedded handler swallowed that belong to the Filex viewer.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Close,
    Previous,
    Next,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Thumbnail(Vec<u16>),
    /// Host `path`'s registered preview handler in a child of `parent`.
    Present {
        path: Vec<u16>,
        parent: isize,
        rect: Rect,
        /// Surface fill (0x00BBGGRR) shown before the handler paints.
        background: u32,
        request_id: u64,
    },
    Resize(Rect),
    /// Unload the handler and hide the surface; the helper stays warm.
    Hide,
    /// Move keyboard focus from the handler back to the parent window.
    ReturnFocus,
    Close,
}
#[derive(Serialize, Deserialize, Debug)]
pub enum Event {
    Pixels {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    },
    /// `kind` is `native` (a handler is showing) or `unavailable`.
    Ready {
        kind: String,
        request_id: u64,
    },
    Key(Key),
    Heartbeat,
    Closed,
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
                path, parent, rect, ..
            } => {
                ensure!(*parent != 0, "preview surface needs a parent window");
                rect.validate()?;
                validate_path(path)
            }
            Self::Resize(rect) => rect.validate(),
            Self::Hide | Self::ReturnFocus | Self::Close => Ok(()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn present(path: Vec<u16>, parent: isize, rect: Rect) -> Request {
        Request::Present {
            path,
            parent,
            rect,
            background: 0,
            request_id: 1,
        }
    }
    const RECT: Rect = Rect {
        x: 0,
        y: 48,
        width: 640,
        height: 480,
    };
    #[test]
    fn framing_rejects_oversize_truncated_and_invalid_messages() {
        assert!(read::<Request>(&mut &u32::MAX.to_le_bytes()[..]).is_err());
        assert!(read::<Request>(&mut &[8, 0, 0, 0, 42][..]).is_err());
        let mut event_bytes = Vec::new();
        write(&mut event_bytes, &Event::Key(Key::Next)).unwrap();
        assert!(matches!(
            read::<Event>(&mut event_bytes.as_slice()).unwrap(),
            Event::Key(Key::Next)
        ));
        let mut bytes = Vec::new();
        write(&mut bytes, &Request::Thumbnail(vec![0xd800, 65])).unwrap();
        let decoded: Request = read(&mut bytes.as_slice()).unwrap();
        assert!(matches!(decoded, Request::Thumbnail(v) if v == vec![0xd800, 65]));
    }
    #[test]
    fn validates_native_path_parent_and_surface_bounds() {
        assert!(validate_path(&[65, 0, 66]).is_err());
        assert!(validate_path(&vec![65; 32768]).is_err());
        assert!(present(vec![65], 7, RECT).validate().is_ok());
        assert!(present(vec![65], 0, RECT).validate().is_err());
        assert!(present(vec![], 7, RECT).validate().is_err());
        for bad in [
            Rect { width: 0, ..RECT },
            Rect { height: -5, ..RECT },
            Rect { x: -1, ..RECT },
            Rect {
                width: MAX_EDGE + 1,
                ..RECT
            },
        ] {
            assert!(present(vec![65], 7, bad).validate().is_err(), "{bad:?}");
            assert!(Request::Resize(bad).validate().is_err(), "{bad:?}");
        }
    }
}
