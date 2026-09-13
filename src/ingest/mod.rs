//! Read-only filesystem inspection. Notification adapters never mutate catalogs.
use crate::catalog::segment::{Identity, Record, Root, raw_name};
use anyhow::Result;
use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub fn identity(path: &Path, metadata: &std::fs::Metadata) -> Identity {
    let birth = metadata
        .created()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos() as u64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        Identity {
            device: metadata.dev(),
            key: metadata.ino(),
            birth,
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
            },
        };
        if let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0 | FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
        {
            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            if unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
                .is_ok()
            {
                return Identity {
                    device: info.dwVolumeSerialNumber as u64,
                    key: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
                    birth: ((info.ftCreationTime.dwHighDateTime as u64) << 32)
                        | info.ftCreationTime.dwLowDateTime as u64,
                };
            }
        }
        Identity {
            device: 0,
            key: 0,
            birth,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Identity {
            device: 0,
            key: 0,
            birth,
        }
    }
}
pub fn inspect(path: &Path, id: u64, parent: u64, root: u32) -> Result<Record> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(Record {
        id,
        parent,
        root,
        name: raw_name(path.file_name().unwrap_or(path.as_os_str())),
        flags: if meta.is_dir() { Record::DIRECTORY } else { 0 }
            | if meta.file_type().is_symlink() {
                Record::SYMLINK
            } else {
                0
            },
        identity: identity(path, &meta),
        size: Some(meta.len()),
        mtime: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64),
    })
}
/// Iterative walk: bounded directory stack, no full descendant path arena and no
/// symlink traversal. Permission loss is reported rather than silently claiming
/// a complete generation.
pub fn walk(
    root: &Root,
    excluded: &Path,
    mut allocate: impl FnMut(&Path, &Identity) -> u64,
    mut emit: impl FnMut(Record) -> Result<()>,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<usize> {
    let mut skipped = 0;
    let include_system = include_system_files();
    let meta = std::fs::symlink_metadata(&root.path)?;
    let root_identity = identity(&root.path, &meta);
    let root_id = allocate(&root.path, &root_identity);
    emit(inspect(&root.path, root_id, 0, root.id)?)?;
    let mut stack = vec![(root.path.clone(), root_id)];
    while let Some((path, parent)) = stack.pop() {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            anyhow::bail!("enumeration cancelled");
        }
        let entries = match std::fs::read_dir(path) {
            Ok(e) => e,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let path = entry.path();
            if path.starts_with(excluded) || (!include_system && excluded_system(&root.path, &path))
            {
                continue;
            }
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let key = identity(&path, &meta);
            let id = allocate(&path, &key);
            let record = match inspect(&path, id, parent, root.id) {
                Ok(r) => r,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let dir = record.is_dir();
            emit(record)?;
            if dir {
                stack.push((path, id));
            }
        }
        std::thread::yield_now();
    }
    Ok(skipped)
}
pub fn default_roots_file() -> Option<PathBuf> {
    Some(dirs::data_local_dir()?.join("filex/roots.list"))
}
pub fn load_roots(path: &Path) -> Vec<PathBuf> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}
pub fn validate_new_root(existing: &[PathBuf], path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize()?;
    anyhow::ensure!(canonical.is_dir(), "root is not a directory");
    anyhow::ensure!(
        !existing
            .iter()
            .any(|p| canonical.starts_with(p) || p.starts_with(&canonical)),
        "root overlaps an existing root"
    );
    Ok(canonical)
}
#[cfg(target_os = "macos")]
pub fn has_full_disk_access() -> bool {
    dirs::home_dir().is_some_and(|home| std::fs::read_dir(home.join("Library/Mail")).is_ok())
}
#[cfg(target_os = "macos")]
pub fn open_full_disk_access_settings() {
    let _ = std::process::Command::new("open")
        .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles")
        .spawn();
}
/// Normalize existing ancestors without following the final symlink itself.
pub fn canonical_event_path(path: &Path) -> PathBuf {
    if let Some(parent) = path.parent()
        && let (Ok(parent), Some(name)) = (parent.canonicalize(), path.file_name())
    {
        return parent.join(name);
    }
    path.to_path_buf()
}

#[cfg(windows)]
const SYSTEM_DIRS: &[&str] = &[
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$Recycle.Bin",
    "System Volume Information",
    "$WinREAgent",
    "Recovery",
    "PerfLogs",
];
#[cfg(target_os = "macos")]
const SYSTEM_DIRS: &[&str] = &[
    "System", "Library", "private", "usr", "bin", "sbin", "cores", "opt", "dev",
];
#[cfg(not(any(windows, target_os = "macos")))]
const SYSTEM_DIRS: &[&str] = &[
    "proc", "sys", "dev", "run", "boot", "usr", "bin", "sbin", "lib", "lib64", "etc", "var", "opt",
    "srv",
];
pub fn include_system_files() -> bool {
    crate::settings::default_settings_file()
        .and_then(|p| crate::settings::Settings::load(&p, None).ok())
        .is_some_and(|s| s.index_system_files)
}
pub fn excluded_system(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .and_then(|p| p.components().next())
        .is_some_and(|part| {
            SYSTEM_DIRS
                .iter()
                .any(|s| part.as_os_str().to_string_lossy().eq_ignore_ascii_case(s))
        })
}
