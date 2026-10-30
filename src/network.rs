//! Network share addresses, as people type them and as the OS reports
//! mounts: `\\server\share\dir` (UNC), `smb://user@server/share/dir`,
//! `nfs://server/export/dir`, and the mount-table spellings `//server/share`
//! (cifs/smbfs) and `server:/export` (NFS).
//!
//! Everything here is pure string work — no lookups, no I/O — so parsing
//! what's typed in the location field never touches the network on the UI
//! thread. Mapping an address to a folder is done against a mount-table
//! snapshot the background drive refresh already holds
//! ([`ShareAddress::locate`]). See docs/design-network.md.

use std::path::{Path, PathBuf};

/// The file-sharing protocol a share is reached over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// SMB/CIFS: Windows shares, Samba, macOS File Sharing, most NASes.
    Smb,
    /// NFS: Unix exports.
    Nfs,
}

/// A remote location: a protocol, a host, and the path segments below it.
/// For SMB the first segment is the share name; for NFS the export path
/// and the folder below it aren't told apart (`/srv/data/x` could be
/// export `/srv` or `/srv/data`), so matching a mount is by segment prefix
/// rather than by a fixed share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareAddress {
    pub protocol: Protocol,
    /// Host name or IP, as written (case kept for display).
    pub host: String,
    /// User name given in a URL (`smb://me@nas/…`), kept for the future
    /// connect flow; never part of matching.
    pub user: Option<String>,
    /// Decoded path segments below the host, never empty strings.
    pub segments: Vec<String>,
}

impl ShareAddress {
    /// Parse an address someone typed: a UNC path (`\\server\share`) or an
    /// `smb://` / `nfs://` URL. URL segments are percent-decoded. `None` for
    /// anything else, including bare `//server/share` — on Unix that's an
    /// ordinary absolute path, so it's left to the filesystem.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Some(rest) = text.strip_prefix(r"\\") {
            // `\\?\` and `\\.\` are Windows device/verbatim prefixes, not
            // shares.
            if rest.starts_with("?\\") || rest.starts_with(".\\") {
                return None;
            }
            return Self::from_parts(Protocol::Smb, None, rest.split(['\\', '/']), false);
        }
        let (scheme, rest) = text.split_once("://")?;
        let protocol = match scheme.to_ascii_lowercase().as_str() {
            "smb" | "cifs" => Protocol::Smb,
            "nfs" => Protocol::Nfs,
            _ => return None,
        };
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        let (user, host) = match authority.rsplit_once('@') {
            Some((user, host)) => (Some(percent_decode(user)?), host),
            None => (None, authority),
        };
        let mut parts = vec![host];
        parts.extend(path.split('/'));
        let mut address = Self::from_parts(protocol, user, parts.into_iter(), true)?;
        address.user = address.user.filter(|u| !u.is_empty());
        Some(address)
    }

    /// Parse a mount table's source field for a network filesystem type:
    /// `//[user@]server/share[/dir]` for cifs/smb3/smbfs, `server:/export`
    /// for nfs/nfs4. `None` for local filesystems and anything unrecognized. `source`
    /// is taken already unescaped from the mount table's own quoting
    /// (Linux's octal `\040`); macOS percent-encodes `smbfs` sources, so
    /// those are decoded here.
    pub fn from_mount_source(source: &str, fstype: &str) -> Option<Self> {
        match fstype {
            "cifs" | "smb3" | "smbfs" => {
                let rest = source.strip_prefix("//")?;
                let (user, rest) = match rest.split_once('@') {
                    // macOS writes `//DOMAIN;user@server/share`.
                    Some((user, rest)) if !user.contains('/') => {
                        let user = user.rsplit(';').next().unwrap_or(user);
                        (Some(user.to_owned()), rest)
                    }
                    _ => (None, rest),
                };
                Self::from_parts(Protocol::Smb, user, rest.split('/'), fstype == "smbfs")
            }
            "nfs" | "nfs4" => {
                let (host, export) = source.rsplit_once(":/")?;
                let host = host.trim_start_matches('[').trim_end_matches(']');
                let parts = std::iter::once(host).chain(export.split('/'));
                Self::from_parts(Protocol::Nfs, None, parts, false)
            }
            _ => None,
        }
    }

    /// Build from a host followed by path pieces, dropping empty pieces
    /// (doubled or trailing separators). Needs a host and, for SMB, a share.
    fn from_parts<'a>(
        protocol: Protocol,
        user: Option<String>,
        mut parts: impl Iterator<Item = &'a str>,
        decode: bool,
    ) -> Option<Self> {
        let host = parts.next().filter(|h| !h.is_empty())?;
        let decode_part = |p: &str| {
            if decode {
                percent_decode(p)
            } else {
                Some(p.to_owned())
            }
        };
        let host = decode_part(host)?;
        let segments = parts
            .filter(|p| !p.is_empty())
            .map(decode_part)
            .collect::<Option<Vec<_>>>()?;
        if protocol == Protocol::Smb && segments.is_empty() {
            return None; // `\\server` alone lists shares — not a folder.
        }
        Some(Self {
            protocol,
            host,
            user,
            segments,
        })
    }

    /// The SMB share name (first segment); `None` for NFS.
    pub fn share(&self) -> Option<&str> {
        match self.protocol {
            Protocol::Smb => self.segments.first().map(String::as_str),
            Protocol::Nfs => None,
        }
    }

    /// The UNC spelling Windows opens directly (`\\server\share\dir`).
    /// SMB only: Windows' NFS client is an optional feature that mounts to
    /// a drive letter instead.
    pub fn to_unc(&self) -> Option<String> {
        if self.protocol != Protocol::Smb {
            return None;
        }
        let mut unc = format!(r"\\{}", self.host);
        for segment in &self.segments {
            unc.push('\\');
            unc.push_str(segment);
        }
        Some(unc)
    }

    /// The URL spelling (`smb://user@server/share/dir`, `nfs://server/export`)
    /// that GVfs and NetFS mount from, segments percent-encoded.
    pub fn to_url(&self) -> String {
        let scheme = match self.protocol {
            Protocol::Smb => "smb",
            Protocol::Nfs => "nfs",
        };
        let mut url = format!("{scheme}://");
        if let Some(user) = &self.user {
            url.push_str(&percent_encode(user));
            url.push('@');
        }
        url.push_str(&percent_encode(&self.host));
        for segment in &self.segments {
            url.push('/');
            url.push_str(&percent_encode(segment));
        }
        url
    }

    /// The address of the share itself, without any folder below it: the
    /// first segment for SMB. For NFS the export and the folders below it
    /// can't be told apart, so the whole path is kept.
    pub fn share_root(&self) -> Self {
        let mut root = self.clone();
        if self.protocol == Protocol::Smb {
            root.segments.truncate(1);
        }
        root
    }

    /// A short name for a mounted share, Explorer-style: `share (server)`
    /// for SMB, `export (server)` from the export's last segment for NFS.
    pub fn display_name(&self) -> String {
        let leaf = match self.protocol {
            Protocol::Smb => self.share(),
            Protocol::Nfs => self.segments.last().map(String::as_str),
        };
        match leaf {
            Some(leaf) => format!("{leaf} ({})", self.host),
            None => self.host.clone(),
        }
    }

    /// Whether `mount` (a mounted share's source) covers this address:
    /// same protocol and host, and its segments are a prefix of ours.
    /// SMB host and share names are case-insensitive; NFS paths are not.
    /// Returns how many of our segments the mount consumes.
    fn covered_by(&self, mount: &ShareAddress) -> Option<usize> {
        if self.protocol != mount.protocol
            || !self.host.eq_ignore_ascii_case(&mount.host)
            || mount.segments.len() > self.segments.len()
        {
            return None;
        }
        let same = |a: &String, b: &String| match self.protocol {
            Protocol::Smb => a.to_lowercase() == b.to_lowercase(),
            Protocol::Nfs => a == b,
        };
        mount
            .segments
            .iter()
            .zip(&self.segments)
            .all(|(m, s)| same(m, s))
            .then_some(mount.segments.len())
    }

    /// The local folder for this address, given the mounted shares as
    /// `(source, mount point)` pairs: the deepest mount that covers it,
    /// joined with the rest of the path. `None` when no mount covers it
    /// (the share isn't connected).
    pub fn locate<'a>(
        &self,
        mounts: impl IntoIterator<Item = (&'a ShareAddress, &'a Path)>,
    ) -> Option<PathBuf> {
        let (used, mount_point) = mounts
            .into_iter()
            .filter_map(|(source, point)| Some((self.covered_by(source)?, point)))
            .max_by_key(|(used, _)| *used)?;
        let mut path = mount_point.to_path_buf();
        path.extend(&self.segments[used..]);
        Some(path)
    }
}

/// A share GVfs mounted (`gio mount`, or a file manager's "Connect to
/// Server"), from its directory name under `$XDG_RUNTIME_DIR/gvfs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GvfsMount {
    /// The name to show for it.
    pub label: String,
    /// Where it points, for the protocols [`ShareAddress`] covers (SMB,
    /// NFS); `None` for SFTP, WebDAV, FTP and AFP.
    pub share: Option<ShareAddress>,
}

impl GvfsMount {
    /// Parse a GVfs FUSE directory name: `type:key=value,…` with values
    /// percent-encoded, keys in any order — `smb-share:server=nas,share=media`,
    /// `nfs:host=fs1,prefix=%2Fsrv%2Fdata`, `sftp:host=example.com,user=me`.
    /// `None` for anything that isn't a network share: local devices GVfs
    /// also mounts (`mtp:`, `gphoto2:`), cloud accounts, and browse-only
    /// locations like `smb-server:` (a server's share list, not a folder).
    pub fn parse(name: &str) -> Option<Self> {
        let (scheme, fields) = name.split_once(':')?;
        let fields = fields
            .split(',')
            .map(|field| {
                let (key, value) = field.split_once('=')?;
                Some((key, percent_decode(value)?))
            })
            .collect::<Option<Vec<_>>>()?;
        let field = |key: &str| {
            fields
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_str())
                .filter(|v| !v.is_empty())
        };
        let host = field("host").or(field("server"))?;
        let share = match scheme {
            "smb-share" => ShareAddress::from_parts(
                Protocol::Smb,
                field("user").map(str::to_owned),
                [host, field("share")?].into_iter(),
                false,
            ),
            "nfs" => {
                let prefix = field("prefix").unwrap_or("");
                ShareAddress::from_parts(
                    Protocol::Nfs,
                    None,
                    std::iter::once(host).chain(prefix.split('/')),
                    false,
                )
            }
            _ => None,
        };
        let label = match (scheme, &share) {
            (_, Some(share)) => share.display_name(),
            ("afp-volume", None) => format!("{} ({host})", field("volume")?),
            ("sftp", None) => format!("{host} (SFTP)"),
            ("dav" | "davs", None) => format!("{host} (WebDAV)"),
            ("ftp" | "ftps", None) => format!("{host} (FTP)"),
            _ => return None,
        };
        Some(Self { label, share })
    }
}

/// Percent-encode everything but RFC 3986 unreserved characters.
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Decode `%XX` escapes as UTF-8. `None` for a malformed escape or bytes
/// that aren't UTF-8, so a garbled URL is rejected rather than guessed at.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smb(host: &str, segments: &[&str]) -> ShareAddress {
        ShareAddress {
            protocol: Protocol::Smb,
            host: host.into(),
            user: None,
            segments: segments.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn unc_paths_parse_with_either_slash() {
        assert_eq!(
            ShareAddress::parse(r"\\nas\Media\Movies"),
            Some(smb("nas", &["Media", "Movies"]))
        );
        assert_eq!(
            ShareAddress::parse(r"\\nas/Media\\Movies\"),
            Some(smb("nas", &["Media", "Movies"]))
        );
    }

    #[test]
    fn a_server_without_a_share_or_a_device_path_is_not_a_share() {
        assert_eq!(ShareAddress::parse(r"\\nas"), None);
        assert_eq!(ShareAddress::parse(r"\\nas\"), None);
        assert_eq!(ShareAddress::parse(r"\\?\C:\Users"), None);
        assert_eq!(ShareAddress::parse(r"\\.\PhysicalDrive0"), None);
        assert_eq!(ShareAddress::parse("smb://nas"), None);
    }

    #[test]
    fn plain_paths_and_other_schemes_are_left_alone() {
        assert_eq!(ShareAddress::parse("/home/me"), None);
        assert_eq!(ShareAddress::parse("//nas/share"), None);
        assert_eq!(ShareAddress::parse(r"C:\Users"), None);
        assert_eq!(ShareAddress::parse("https://nas/share"), None);
        assert_eq!(ShareAddress::parse("ftp://nas/share"), None);
    }

    #[test]
    fn smb_urls_decode_user_and_segments() {
        let address = ShareAddress::parse("SMB://me%40corp@nas.local/My%20Share/a%2Fb/").unwrap();
        assert_eq!(address.protocol, Protocol::Smb);
        assert_eq!(address.host, "nas.local");
        assert_eq!(address.user.as_deref(), Some("me@corp"));
        assert_eq!(address.segments, ["My Share", "a/b"]);
        assert_eq!(address.share(), Some("My Share"));
        assert_eq!(
            ShareAddress::parse("smb://@nas/s").unwrap().user,
            None,
            "an empty user is no user"
        );
    }

    #[test]
    fn malformed_escapes_are_rejected() {
        assert_eq!(ShareAddress::parse("smb://nas/bad%2"), None);
        assert_eq!(ShareAddress::parse("smb://nas/bad%zz"), None);
        assert_eq!(ShareAddress::parse("smb://nas/%ff"), None, "not UTF-8");
    }

    #[test]
    fn nfs_urls_keep_the_whole_path() {
        let address = ShareAddress::parse("nfs://fs1/srv/data/projects").unwrap();
        assert_eq!(address.protocol, Protocol::Nfs);
        assert_eq!(address.segments, ["srv", "data", "projects"]);
        assert_eq!(address.share(), None);
        assert_eq!(address.to_unc(), None);
        // An NFS server root is a valid export.
        assert!(
            ShareAddress::parse("nfs://fs1/")
                .unwrap()
                .segments
                .is_empty()
        );
    }

    #[test]
    fn unc_spelling_round_trips() {
        let address = ShareAddress::parse("smb://nas/Media/Movies").unwrap();
        assert_eq!(address.to_unc().unwrap(), r"\\nas\Media\Movies");
        assert_eq!(
            ShareAddress::parse(&address.to_unc().unwrap()).unwrap(),
            address
        );
    }

    #[test]
    fn linux_cifs_and_macos_smbfs_sources_parse() {
        assert_eq!(
            ShareAddress::from_mount_source("//nas/My Share", "cifs"),
            Some(smb("nas", &["My Share"]))
        );
        let mac =
            ShareAddress::from_mount_source("//WORKGROUP;me@nas._smb._tcp.local/Media", "smbfs")
                .unwrap();
        assert_eq!(mac.user.as_deref(), Some("me"));
        assert_eq!(mac.host, "nas._smb._tcp.local");
        assert_eq!(mac.segments, ["Media"]);
        assert_eq!(
            ShareAddress::from_mount_source("//nas/Photo%20Library", "smbfs")
                .unwrap()
                .segments,
            ["Photo Library"]
        );
    }

    #[test]
    fn nfs_sources_parse_including_ipv6_hosts() {
        let source = ShareAddress::from_mount_source("fs1:/srv/data", "nfs4").unwrap();
        assert_eq!(source.host, "fs1");
        assert_eq!(source.segments, ["srv", "data"]);
        let v6 = ShareAddress::from_mount_source("[fd00::1]:/export", "nfs").unwrap();
        assert_eq!(v6.host, "fd00::1");
        assert_eq!(v6.segments, ["export"]);
    }

    #[test]
    fn local_filesystems_are_not_shares() {
        assert_eq!(ShareAddress::from_mount_source("/dev/sda1", "ext4"), None);
        assert_eq!(ShareAddress::from_mount_source("tmpfs", "tmpfs"), None);
        assert_eq!(ShareAddress::from_mount_source("nas/share", "cifs"), None);
    }

    #[test]
    fn display_names_read_like_explorer() {
        assert_eq!(smb("nas", &["Media"]).display_name(), "Media (nas)");
        let nfs = ShareAddress::from_mount_source("fs1:/srv/data", "nfs").unwrap();
        assert_eq!(nfs.display_name(), "data (fs1)");
        let root = ShareAddress::from_mount_source("fs1:/", "nfs").unwrap();
        assert_eq!(root.display_name(), "fs1");
    }

    #[test]
    fn locate_maps_into_the_deepest_covering_mount() {
        let whole = smb("NAS", &["media"]);
        let movies = smb("nas", &["Media", "Movies"]);
        let other = smb("nas", &["Backup"]);
        let mounts = [
            (&whole, Path::new("/mnt/media")),
            (&movies, Path::new("/mnt/movies")),
            (&other, Path::new("/mnt/backup")),
        ];
        let typed = ShareAddress::parse(r"\\nas\Media\Movies\2024").unwrap();
        assert_eq!(
            typed.locate(mounts),
            Some(PathBuf::from("/mnt/movies/2024"))
        );
        let typed = ShareAddress::parse("smb://nas/media/Music").unwrap();
        assert_eq!(
            typed.locate(mounts),
            Some(PathBuf::from("/mnt/media/Music"))
        );
    }

    #[test]
    fn locate_needs_a_mount_for_the_same_share_and_protocol() {
        let media = smb("nas", &["Media"]);
        let mounts = [(&media, Path::new("/mnt/media"))];
        assert_eq!(
            ShareAddress::parse("smb://nas/Photos")
                .unwrap()
                .locate(mounts),
            None
        );
        assert_eq!(
            ShareAddress::parse("smb://other/Media")
                .unwrap()
                .locate(mounts),
            None
        );
        assert_eq!(
            ShareAddress::parse("nfs://nas/Media")
                .unwrap()
                .locate(mounts),
            None
        );
    }

    #[test]
    fn nfs_matching_is_case_sensitive() {
        let export = ShareAddress::from_mount_source("fs1:/srv/Data", "nfs").unwrap();
        let mounts = [(&export, Path::new("/mnt/data"))];
        assert_eq!(
            ShareAddress::parse("nfs://fs1/srv/Data/x")
                .unwrap()
                .locate(mounts),
            Some(PathBuf::from("/mnt/data/x"))
        );
        assert_eq!(
            ShareAddress::parse("nfs://fs1/srv/data/x")
                .unwrap()
                .locate(mounts),
            None
        );
    }

    #[test]
    fn gvfs_smb_shares_parse_with_fields_in_any_order() {
        let mount =
            GvfsMount::parse("smb-share:domain=WORK,server=nas,share=my%20media,user=me").unwrap();
        assert_eq!(mount.label, "my media (nas)");
        let mut expected = smb("nas", &["my media"]);
        expected.user = Some("me".into());
        assert_eq!(mount.share, Some(expected));
        assert_eq!(
            GvfsMount::parse("smb-share:share=Media,server=nas")
                .unwrap()
                .share,
            Some(smb("nas", &["Media"]))
        );
    }

    #[test]
    fn gvfs_nfs_mounts_carry_their_export_path() {
        let mount = GvfsMount::parse("nfs:host=fs1,prefix=%2Fsrv%2Fdata").unwrap();
        assert_eq!(mount.label, "data (fs1)");
        assert_eq!(
            mount.share,
            Some(ShareAddress {
                protocol: Protocol::Nfs,
                host: "fs1".into(),
                user: None,
                segments: vec!["srv".into(), "data".into()],
            })
        );
    }

    #[test]
    fn other_gvfs_network_mounts_are_named_but_not_addressable() {
        for (name, label) in [
            ("sftp:host=example.com,user=me", "example.com (SFTP)"),
            (
                "davs:host=cloud.example.com,ssl=true",
                "cloud.example.com (WebDAV)",
            ),
            ("ftp:host=ftp.example.com", "ftp.example.com (FTP)"),
            ("afp-volume:host=mac,volume=Shared", "Shared (mac)"),
        ] {
            let mount = GvfsMount::parse(name).unwrap();
            assert_eq!(mount.label, label);
            assert_eq!(mount.share, None, "{name}");
        }
    }

    #[test]
    fn gvfs_devices_browse_points_and_garbage_are_not_shares() {
        for name in [
            "mtp:host=Google_Pixel_123",
            "gphoto2:host=Canon",
            "smb-server:server=nas",
            "google-drive:host=gmail.com,user=me",
            "smb-share:server=nas",
            "smb-share:server=nas,share=%zz",
            "not-a-gvfs-name",
            "sftp:user=me",
        ] {
            assert_eq!(GvfsMount::parse(name), None, "{name}");
        }
    }

    #[test]
    fn addresses_round_trip_through_their_url() {
        for text in [
            "smb://me@nas/My%20Media/Films%20%26%20TV",
            "nfs://fs1/srv/data",
            "smb://192.168.1.5/public",
        ] {
            let address = ShareAddress::parse(text).unwrap();
            assert_eq!(address.to_url(), text);
            assert_eq!(ShareAddress::parse(&address.to_url()), Some(address));
        }
        assert_eq!(
            ShareAddress::parse(r"\\nas\Media\Movies").unwrap().to_url(),
            "smb://nas/Media/Movies"
        );
    }

    #[test]
    fn the_share_root_drops_folders_for_smb_only() {
        let smb = ShareAddress::parse("smb://nas/Media/Movies/2026").unwrap();
        assert_eq!(smb.share_root().to_url(), "smb://nas/Media");
        let nfs = ShareAddress::parse("nfs://fs1/srv/data").unwrap();
        assert_eq!(nfs.share_root(), nfs);
    }
}
