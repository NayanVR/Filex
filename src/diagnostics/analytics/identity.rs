//! The anonymous install ID: a random UUIDv4 in a file under the user's
//! local data dir. It is the PostHog `distinct_id`, so funnels and
//! retention can count installs. It is never derived from the machine,
//! the user name or any hardware ID, and it is deleted when the user turns
//! sharing off, so opting back in starts a new, unlinked identity.

use std::path::{Path, PathBuf};

use super::wire;

/// `<data_local_dir>/filex/analytics-id`.
pub fn default_path() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("filex").join("analytics-id"))
}

/// Outcome of [`load_or_create`]: the ID and whether this run minted it
/// (a first launch with sharing on, which `app_launched` reports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallId {
    pub id: String,
    pub created: bool,
}

/// Read the install ID at `path`, creating it if missing or malformed.
/// A write failure still returns a fresh in-memory ID (that run just
/// counts as a new install).
pub fn load_or_create(path: &Path) -> InstallId {
    if let Ok(text) = std::fs::read_to_string(path) {
        let id = text.trim();
        if is_uuid(id) {
            return InstallId {
                id: id.to_string(),
                created: false,
            };
        }
    }
    let id = wire::uuid_v4(wire::random_bytes());
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, &id);
    InstallId { id, created: true }
}

/// Forget the install ID (sharing turned off).
pub fn forget(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Accept only a canonical UUID, so a hand-edited file can't smuggle
/// arbitrary text (say, an email) into every event as the distinct ID.
fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_then_reuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("filex").join("analytics-id");
        let first = load_or_create(&path);
        assert!(first.created);
        assert!(is_uuid(&first.id));
        let second = load_or_create(&path);
        assert!(!second.created);
        assert_eq!(first.id, second.id);
    }

    #[test]
    fn forget_starts_a_new_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analytics-id");
        let first = load_or_create(&path);
        forget(&path);
        let second = load_or_create(&path);
        assert!(second.created);
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn non_uuid_contents_are_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("analytics-id");
        std::fs::write(&path, "bob@example.com").unwrap();
        let id = load_or_create(&path);
        assert!(id.created);
        assert!(is_uuid(&id.id));
    }
}
