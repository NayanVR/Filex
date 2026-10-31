//! macOS: connect through NetFS, the framework behind Finder's "Connect to
//! Server", so the system's sign-in sheet and Keychain handle credentials
//! and the share lands under `/Volumes` like any Finder mount.
//!
//! The error messages build everywhere so they're tested on every CI
//! runner; the NetFS call itself is macOS-only.
use super::ConnectError;
use crate::network::ShareAddress;
use std::path::Path;

const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EBUSY: i32 = 16;
const EINVAL: i32 = 22;
const EACCES: i32 = 13;
const EEXIST: i32 = 17;
const ENETUNREACH: i32 = 51;
const ETIMEDOUT: i32 = 60;
const EHOSTUNREACH: i32 = 65;
const EAUTH: i32 = 80;
const ECANCELED: i32 = 89;
/// Carbon's `userCanceledErr`, which NetFS returns when the sign-in sheet
/// is dismissed.
const USER_CANCELED: i32 = -128;

/// What `NetFSMountURLSync`'s status means: `Ok` when the share is there
/// to open (mounted now, or already mounted — `EEXIST`).
pub(super) fn outcome(status: i32, address: &ShareAddress) -> Result<(), ConnectError> {
    let name = address.display_name();
    let host = &address.host;
    Err(ConnectError::Failed(match status {
        0 | EEXIST => return Ok(()),
        ECANCELED | USER_CANCELED => return Err(ConnectError::Cancelled),
        EAUTH | EACCES | EPERM => {
            format!("Access to {name} was denied. Check the user name and password.")
        }
        ENOENT => format!("“{host}” has no share called {name}."),
        ENETUNREACH | EHOSTUNREACH | ETIMEDOUT => format!("Couldn’t reach the server “{host}”."),
        _ => format!("Couldn’t connect to {name} (error {status})."),
    }))
}

/// What `unmount(2)` failing with `errno` means. `EINVAL` (not a mount
/// point) and `ENOENT` mean it's already gone.
pub(super) fn unmount_outcome(errno: Option<i32>, mount: &Path) -> Result<(), ConnectError> {
    match errno {
        None | Some(EINVAL) | Some(ENOENT) => Ok(()),
        Some(EBUSY) => Err(super::in_use(mount)),
        Some(EPERM) => Err(ConnectError::Failed(format!(
            "You aren’t allowed to eject {}.",
            crate::listing::location_label(mount)
        ))),
        Some(errno) => Err(ConnectError::Failed(format!(
            "Couldn’t eject {} (error {errno}).",
            crate::listing::location_label(mount)
        ))),
    }
}

#[cfg(target_os = "macos")]
pub(super) struct NetFsConnector;

#[cfg(target_os = "macos")]
impl super::ShareConnector for NetFsConnector {
    fn connect(
        &self,
        request: &super::ConnectRequest,
        _cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<std::path::PathBuf, ConnectError> {
        let address = &request.address;
        let status = mount(&address.share_root().to_url()).ok_or_else(|| {
            ConnectError::Failed(format!("“{}” isn’t a valid address.", address.to_url()))
        })?;
        outcome(status, address)?;
        super::locate_mounted(address)
    }

    /// Unmount with `unmount(2)` and no flags. Assumes macOS lets the user
    /// who mounted a volume (NetFS mounts as the signed-in user) unmount
    /// it without privileges, and that without `MNT_FORCE` it refuses with
    /// `EBUSY` while files on it are open.
    fn disconnect(&self, mount: &Path) -> Result<(), ConnectError> {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(mount.as_os_str().as_bytes())
            .map_err(|_| ConnectError::Failed("That path can’t be ejected.".into()))?;
        // SAFETY: `path` is a valid NUL-terminated C string for the call.
        let status = unsafe { libc::unmount(path.as_ptr(), 0) };
        let errno =
            (status != 0).then(|| std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        unmount_outcome(errno, mount)
    }
}

#[cfg(target_os = "macos")]
mod ffi {
    use std::ffi::c_void;

    pub type CFTypeRef = *const c_void;
    /// `kCFStringEncodingUTF8`.
    pub const UTF8: u32 = 0x0800_0100;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub fn CFURLCreateWithBytes(
            allocator: CFTypeRef,
            bytes: *const u8,
            length: isize,
            encoding: u32,
            base: CFTypeRef,
        ) -> CFTypeRef;
        pub fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "NetFS", kind = "framework")]
    unsafe extern "C" {
        pub fn NetFSMountURLSync(
            url: CFTypeRef,
            mountpath: CFTypeRef,
            user: CFTypeRef,
            passwd: CFTypeRef,
            open_options: CFTypeRef,
            mount_options: CFTypeRef,
            mountpoints: *mut CFTypeRef,
        ) -> i32;
    }
}

/// Mount `url` (`smb://…`, `nfs://…`) with NetFS and return its status;
/// `None` if the URL isn't one CoreFoundation accepts.
///
/// Assumes NetFS' documented defaults when every option is null: mount at
/// the default place under `/Volumes`, use Keychain credentials, and show
/// the system sign-in sheet when those don't work (UI allowed), blocking
/// until the user answers it. The mount points array it returns is
/// released unread; the caller finds the folder from a fresh drive list.
#[cfg(target_os = "macos")]
fn mount(url: &str) -> Option<i32> {
    use ffi::*;
    // SAFETY: the byte buffer outlives the call; a null allocator and base
    // URL mean the defaults. The returned URL is owned and released below.
    let cf_url = unsafe {
        CFURLCreateWithBytes(
            std::ptr::null(),
            url.as_ptr(),
            url.len() as isize,
            UTF8,
            std::ptr::null(),
        )
    };
    if cf_url.is_null() {
        return None;
    }
    let mut mountpoints: CFTypeRef = std::ptr::null();
    // SAFETY: `cf_url` is a valid CFURL; null mount path, credentials and
    // options select NetFS' defaults; `mountpoints` is a valid out pointer.
    let status = unsafe {
        NetFSMountURLSync(
            cf_url,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            &mut mountpoints,
        )
    };
    // SAFETY: both were created (or returned as owned) by CoreFoundation /
    // NetFS for us and are released exactly once.
    unsafe {
        if !mountpoints.is_null() {
            CFRelease(mountpoints);
        }
        CFRelease(cf_url);
    }
    Some(status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmount_errors_read_plainly() {
        let mount = Path::new("/Volumes/Media");
        assert_eq!(unmount_outcome(None, mount), Ok(()));
        assert_eq!(unmount_outcome(Some(EINVAL), mount), Ok(()), "already gone");
        assert_eq!(
            unmount_outcome(Some(EBUSY), mount),
            Err(super::super::in_use(mount))
        );
        assert_eq!(
            unmount_outcome(Some(EPERM), mount),
            Err(ConnectError::Failed(
                "You aren’t allowed to eject Media.".into()
            ))
        );
    }

    #[test]
    fn netfs_statuses_read_plainly() {
        let media = ShareAddress::parse("smb://nas/Media").unwrap();
        assert_eq!(outcome(0, &media), Ok(()));
        assert_eq!(outcome(EEXIST, &media), Ok(()), "already mounted");
        assert_eq!(outcome(USER_CANCELED, &media), Err(ConnectError::Cancelled));
        assert_eq!(outcome(ECANCELED, &media), Err(ConnectError::Cancelled));
        assert_eq!(
            outcome(EHOSTUNREACH, &media),
            Err(ConnectError::Failed(
                "Couldn’t reach the server “nas”.".into()
            ))
        );
        assert_eq!(
            outcome(EAUTH, &media),
            Err(ConnectError::Failed(
                "Access to Media (nas) was denied. Check the user name and password.".into()
            ))
        );
    }
}
