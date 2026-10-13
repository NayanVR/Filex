//! Security descriptors for the updater's pipe and service object.
//!
//! Assumes the NT security model: SDDL strings are converted with
//! `ConvertStringSecurityDescriptorToSecurityDescriptorW`, whose buffer must
//! be released with `LocalFree`.

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::HSTRING;

/// Pipe DACL (protected — no inheritance):
/// - deny network logons (`NU`) everything;
/// - SYSTEM, Administrators and the pipe's owner: full access (the owner
///   ACE lets an unelevated test process own its own test pipe);
/// - interactive users (`IU`): `FILE_GENERIC_READ | FILE_WRITE_DATA`
///   (`0x12008b`) — notably *not* `FILE_APPEND_DATA`, which on a pipe means
///   `FILE_CREATE_PIPE_INSTANCE`, so users can't add instances.
pub const PIPE_SDDL: &str = "D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x12008b;;;IU)";

/// The access a client opens the pipe with — exactly what [`PIPE_SDDL`]
/// grants interactive users.
pub const PIPE_CLIENT_ACCESS: u32 = 0x0012_008b;

/// Service object DACL: Windows' default service DACL plus `RP`
/// (`SERVICE_START`) for interactive users, so the unelevated app can start
/// the demand-start updater. Stop/configure stay admin-only.
pub const SERVICE_SDDL: &str = "D:(A;;CCLCSWRPWPDTLOCRRC;;;SY)(A;;CCDCLCSWRPWPDTLOCRSDRCWDWO;;;BA)(A;;CCLCSWRPLOCRRC;;;IU)(A;;CCLCSWLOCRRC;;;SU)";

/// An owned self-relative security descriptor parsed from SDDL.
pub struct SecurityDescriptor(PSECURITY_DESCRIPTOR);

// SAFETY: the descriptor is an immutable heap block owned by this value.
unsafe impl Send for SecurityDescriptor {}
unsafe impl Sync for SecurityDescriptor {}

impl SecurityDescriptor {
    pub fn from_sddl(sddl: &str) -> Result<Self> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: valid NUL-terminated input; on success the out pointer
        // owns a LocalAlloc'd buffer that Drop frees.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(sddl),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .with_context(|| format!("parsing SDDL {sddl}"))?;
        Ok(Self(descriptor))
    }

    pub fn as_ptr(&self) -> PSECURITY_DESCRIPTOR {
        self.0
    }

    /// Non-inheritable security attributes pointing at this descriptor.
    /// Valid while `self` lives.
    pub fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptor…W.
        unsafe {
            LocalFree(Some(HLOCAL(self.0.0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_descriptors_parse() {
        SecurityDescriptor::from_sddl(PIPE_SDDL).unwrap();
        SecurityDescriptor::from_sddl(SERVICE_SDDL).unwrap();
    }

    #[test]
    fn malformed_sddl_is_an_error() {
        assert!(SecurityDescriptor::from_sddl("D:(nonsense").is_err());
    }
}
