//! The app ↔ updater wire protocol over `\\.\pipe\filex-updater`.
//!
//! Deliberately tiny: no command carries a URL, path or argument — the
//! updater only ever installs what its compiled-in manifest URL and public
//! key authorize. Frames are a little-endian `u32` length then JSON.

use std::io::{Read, Write};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change. Both sides refuse a mismatch.
pub const PROTOCOL: u32 = 1;

/// Upper bound on one frame; real frames are well under 1 KiB.
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    /// Identify the updater: protocol and version.
    Hello,
    /// Report the current [`Phase`].
    Status,
    /// Check for (and download + verify) a newer release.
    Check,
    /// Install the verified release; checks first if none is held.
    Apply,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub protocol: u32,
    pub command: Command,
}

/// What the updater is doing. The app renders it; `Applying` is the app's
/// cue to stop its daemon and quit.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Phase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Downloading {
        version: String,
    },
    /// A verified payload is held in memory, ready to install.
    Ready {
        version: String,
    },
    /// The install is under way; Filex processes are about to be closed.
    Applying {
        version: String,
    },
    Failed {
        message: String,
    },
}

impl Phase {
    /// Whether work is in flight (the service must not idle-exit).
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Phase::Checking | Phase::Downloading { .. } | Phase::Applying { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Hello { protocol: u32, version: String },
    Status(Phase),
    Error(String),
}

pub fn write_frame<T: Serialize>(stream: &mut impl Write, message: &T) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(bytes.len() <= MAX_FRAME, "updater frame exceeds limit");
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

pub fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut impl Read) -> Result<T> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header) as usize;
    ensure!(len <= MAX_FRAME, "updater frame exceeds limit");
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buf = Vec::new();
        let request = Request {
            protocol: PROTOCOL,
            command: Command::Apply,
        };
        write_frame(&mut buf, &request).unwrap();
        let parsed: Request = read_frame(&mut buf.as_slice()).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn oversized_frames_are_rejected_before_allocation() {
        let mut buf = ((MAX_FRAME + 1) as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(b"{}");
        assert!(read_frame::<Request>(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn unknown_commands_fail_to_parse() {
        // A client can't smuggle e.g. an install path: the enum is closed.
        let json = br#"{"protocol":1,"command":{"Install":"C:\\evil.msi"}}"#;
        let mut buf = (json.len() as u32).to_le_bytes().to_vec();
        buf.extend_from_slice(json);
        assert!(read_frame::<Request>(&mut buf.as_slice()).is_err());
    }

    #[test]
    fn busy_phases() {
        assert!(Phase::Checking.is_busy());
        assert!(
            Phase::Applying {
                version: "1.0.0".into()
            }
            .is_busy()
        );
        assert!(
            !Phase::Ready {
                version: "1.0.0".into()
            }
            .is_busy()
        );
        assert!(!Phase::Idle.is_busy());
    }
}
