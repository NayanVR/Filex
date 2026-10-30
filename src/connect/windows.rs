//! Windows: connect with `WNetAddConnection2W`. SMB only — Windows' NFS
//! client is an optional feature that mounts to drive letters through
//! its own tools.
//!
//! The pure parts (error messages, choosing a drive letter, the folder to
//! open) build everywhere so they're tested on every CI runner.
use super::ConnectError;
use crate::network::ShareAddress;
use std::path::PathBuf;

/// Win32 errors `WNetAddConnection2W` reports that deserve their own
/// message (winerror.h).
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_BAD_NETPATH: u32 = 53;
const ERROR_BAD_NET_NAME: u32 = 67;
const ERROR_ALREADY_ASSIGNED: u32 = 85;
const ERROR_BAD_DEVICE: u32 = 1200;
const ERROR_SESSION_CREDENTIAL_CONFLICT: u32 = 1219;
const ERROR_CANCELLED: u32 = 1223;
const ERROR_NO_NETWORK: u32 = 1222;
const ERROR_LOGON_FAILURE: u32 = 1326;

/// What a failed connect means for someone looking at the dialog.
pub(super) fn error_for(code: u32, address: &ShareAddress) -> ConnectError {
    let name = address.display_name();
    let host = &address.host;
    ConnectError::Failed(match code {
        ERROR_CANCELLED => return ConnectError::Cancelled,
        ERROR_BAD_NETPATH => format!("Couldn’t find the server “{host}”."),
        // Also what Windows reports for a host name that doesn't resolve.
        ERROR_BAD_NET_NAME => format!(
            "Couldn’t find {}. Check the server and share names.",
            address
                .share_root()
                .to_unc()
                .unwrap_or_else(|| host.clone())
        ),
        ERROR_ACCESS_DENIED | ERROR_LOGON_FAILURE => {
            format!("Access to {name} was denied. Check the user name and password.")
        }
        ERROR_SESSION_CREDENTIAL_CONFLICT => format!(
            "You’re already connected to “{host}” as a different user. \
             Disconnect its other shares first."
        ),
        ERROR_ALREADY_ASSIGNED | ERROR_BAD_DEVICE => {
            "That drive letter is already in use.".to_owned()
        }
        ERROR_NO_NETWORK => "The network isn’t available.".to_owned(),
        _ => format!("Couldn’t connect to {name} (error {code})."),
    })
}

/// The drive letter Explorer's "Map network drive" offers first: the
/// highest free one, from Z down to D (A–C are floppies and the system
/// disk by convention). `used` is `GetLogicalDrives`' bitmask, bit 0 = A.
pub(super) fn free_drive_letter(used: u32) -> Option<char> {
    (3..26u8)
        .rev()
        .find(|bit| used & (1 << bit) == 0)
        .map(|bit| (b'A' + bit) as char)
}

/// The folder to open once connected: the requested path under the drive
/// letter when one was mapped, else the UNC path itself. Joined with `\`
/// explicitly rather than `PathBuf::push`, which uses the host's separator,
/// so the result is the same Windows path wherever it's computed.
pub(super) fn connected_folder(address: &ShareAddress, letter: Option<char>) -> Option<PathBuf> {
    let Some(letter) = letter else {
        return address.to_unc().map(PathBuf::from);
    };
    let below: Vec<&str> = address
        .segments
        .iter()
        .skip(1)
        .map(String::as_str)
        .collect();
    Some(PathBuf::from(format!("{letter}:\\{}", below.join("\\"))))
}

#[cfg(windows)]
pub(super) struct WNetConnector;

#[cfg(windows)]
impl super::ShareConnector for WNetConnector {
    fn connect(
        &self,
        request: &super::ConnectRequest,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<PathBuf, ConnectError> {
        let address = &request.address;
        let Some(share) = address.share_root().to_unc() else {
            return Err(ConnectError::Failed(
                "Windows connects to SMB shares (\\\\server\\share or smb://…) only.".into(),
            ));
        };
        let letter =
            if request.map_drive_letter {
                // SAFETY: GetLogicalDrives takes no arguments and only reads.
                let used = unsafe { ::windows::Win32::Storage::FileSystem::GetLogicalDrives() };
                Some(free_drive_letter(used).ok_or_else(|| {
                    ConnectError::Failed("There’s no free drive letter left.".into())
                })?)
            } else {
                None
            };
        add_connection(&share, letter, address.user.as_deref())
            .map_err(|code| error_for(code, address))?;
        connected_folder(address, letter)
            .ok_or_else(|| ConnectError::Failed("Couldn’t open the share.".into()))
    }
}

/// Connect `share` (`\\server\share`), optionally as drive `letter`.
///
/// Assumes `WNetAddConnection2W`'s documented behaviour: with
/// `CONNECT_INTERACTIVE` and no password it tries the signed-in user's
/// (or the saved) credentials first and shows the system credential
/// prompt only if those are refused, blocking until it's answered. It
/// returns `NO_ERROR` when the share is already connected with the same
/// credentials. Without `CONNECT_UPDATE_PROFILE` a drive letter lasts
/// until sign-out, like Explorer's "Reconnect at sign-in" left unticked.
#[cfg(windows)]
fn add_connection(share: &str, letter: Option<char>, user: Option<&str>) -> Result<(), u32> {
    use ::windows::Win32::NetworkManagement::WNet::{
        CONNECT_INTERACTIVE, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W,
    };
    use ::windows::core::{HSTRING, PCWSTR, PWSTR};

    let mut remote: Vec<u16> = share.encode_utf16().chain([0]).collect();
    let mut local: Option<Vec<u16>> =
        letter.map(|l| format!("{l}:").encode_utf16().chain([0]).collect());
    let resource = NETRESOURCEW {
        dwType: RESOURCETYPE_DISK,
        lpRemoteName: PWSTR(remote.as_mut_ptr()),
        lpLocalName: local
            .as_mut()
            .map_or(PWSTR::null(), |l| PWSTR(l.as_mut_ptr())),
        ..Default::default()
    };
    let user = user.map(HSTRING::from);
    let user = user.as_ref().map_or(PCWSTR::null(), |u| PCWSTR(u.as_ptr()));
    // SAFETY: `resource` points at NUL-terminated buffers that outlive the
    // call; null password and user name mean "use the current or saved
    // credentials, else prompt".
    let status =
        unsafe { WNetAddConnection2W(&resource, PCWSTR::null(), user, CONNECT_INTERACTIVE) };
    if status.0 == 0 { Ok(()) } else { Err(status.0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nas() -> ShareAddress {
        ShareAddress::parse(r"\\nas\Media\Movies").unwrap()
    }

    /// Exercises the real `WNetAddConnection2W` call. Ignored by default:
    /// it waits on name resolution for a host that doesn't exist.
    #[cfg(windows)]
    #[test]
    #[ignore]
    fn connecting_to_a_missing_server_reports_it() {
        use crate::connect::ShareConnector as _;
        let request = super::super::ConnectRequest {
            address: ShareAddress::parse(r"\\filex-no-such-host-59\share").unwrap(),
            map_drive_letter: false,
        };
        let result = WNetConnector.connect(&request, &Default::default());
        assert!(
            matches!(&result, Err(ConnectError::Failed(m)) if m.starts_with("Couldn’t find")),
            "{result:?}"
        );
    }

    #[test]
    fn the_highest_free_letter_is_offered_first() {
        assert_eq!(free_drive_letter(0b100), Some('Z'), "only C: in use");
        let z = 1 << 25;
        assert_eq!(free_drive_letter(z | 0b100), Some('Y'));
        let d_to_z = ((1u32 << 26) - 1) & !0b111;
        assert_eq!(free_drive_letter(d_to_z), None);
        assert_eq!(free_drive_letter(d_to_z & !(1 << 3)), Some('D'));
    }

    #[test]
    fn the_folder_keeps_the_path_below_the_share() {
        assert_eq!(
            connected_folder(&nas(), Some('Z')),
            Some(PathBuf::from(r"Z:\Movies"))
        );
        assert_eq!(
            connected_folder(&nas(), None),
            Some(PathBuf::from(r"\\nas\Media\Movies"))
        );
        let nfs = ShareAddress::parse("nfs://fs1/srv").unwrap();
        assert_eq!(connected_folder(&nfs, None), None);
    }

    #[test]
    fn errors_read_plainly_and_a_dismissed_prompt_is_a_cancel() {
        assert_eq!(error_for(ERROR_CANCELLED, &nas()), ConnectError::Cancelled);
        for (code, expected) in [
            (ERROR_BAD_NETPATH, "Couldn’t find the server “nas”."),
            (
                ERROR_BAD_NET_NAME,
                r"Couldn’t find \\nas\Media. Check the server and share names.",
            ),
            (
                ERROR_LOGON_FAILURE,
                "Access to Media (nas) was denied. Check the user name and password.",
            ),
            (
                ERROR_ALREADY_ASSIGNED,
                "That drive letter is already in use.",
            ),
            (9999, "Couldn’t connect to Media (nas) (error 9999)."),
        ] {
            assert_eq!(
                error_for(code, &nas()),
                ConnectError::Failed(expected.into())
            );
        }
    }
}
