//! First-run import of the folders a user already pinned in their OS file
//! manager: Finder sidebar favorites (macOS), Quick Access (Windows), GTK
//! bookmarks (Linux). Onboarding offers them as Filex favorites.
//!
//! The parsers are pure and portable so they unit-test on every CI runner;
//! only [`discover`] touches the OS.

use std::path::{Path, PathBuf};

/// The folders the OS file manager has pinned, in its order, keeping only
/// directories that exist right now. Best effort: an unreadable or missing
/// source yields an empty list, never an error, since onboarding simply
/// skips the import step then. Blocking; run off the UI thread.
pub fn discover() -> Vec<PathBuf> {
    let mut found = platform::pinned_folders();
    found.retain(|path| path.is_dir());
    found
}

/// `found` minus duplicates and anything already in `known` (the sidebar's
/// built-in places and existing favorites), order preserved.
pub fn new_folders(found: Vec<PathBuf>, known: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for path in found {
        if !known.contains(&path) && !out.contains(&path) {
            out.push(path);
        }
    }
    out
}

/// Parse a GTK bookmarks file (`~/.config/gtk-3.0/bookmarks`): one
/// `file:///percent/encoded/path Optional Label` per line. Non-`file:`
/// URIs (sftp, smb, …) are skipped: Filex only browses local paths.
pub fn parse_gtk_bookmarks(text: &str) -> Vec<PathBuf> {
    text.lines()
        .filter_map(|line| line.split(' ').next()?.strip_prefix("file://"))
        .filter_map(percent_decode)
        .map(PathBuf::from)
        .collect()
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Parse Finder's sidebar favorites list
/// (`com.apple.LSSharedFileList.FavoriteItems.sfl3`/`.sfl2`).
///
/// Assumes the macOS 11–15 layout: an `NSKeyedArchiver` property list whose
/// `$objects` table holds each item's bookmark as a data blob starting with
/// `book`. Rather than walk the archiver's object graph, every such blob is
/// decoded in table order. Special entries (AirDrop, Recents) have no
/// bookmark or point inside Finder.app, and are dropped.
pub fn parse_finder_favorites(bytes: &[u8]) -> Vec<PathBuf> {
    let Ok(root) = plist::Value::from_reader(std::io::Cursor::new(bytes)) else {
        return Vec::new();
    };
    let Some(objects) = root
        .as_dictionary()
        .and_then(|d| d.get("$objects"))
        .and_then(|o| o.as_array())
    else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|object| object.as_data())
        .filter_map(bookmark_path)
        .filter(|path| !path.starts_with("/System"))
        .collect()
}

/// The target path stored in a macOS bookmark blob (`NSURL` bookmark data).
///
/// Assumes Apple's undocumented but long-stable "book" format (as decoded by
/// e.g. `mac_alias`): little-endian; a header whose fourth u32 is the data
/// section's offset; the data section starts with the first table of
/// contents' offset; a TOC is `size, 0xfffffffe, id, next, count` followed
/// by `count` × `(key, offset, reserved)` entries. Key `0x1004` is the path,
/// an array (type `0x0601`) of offsets to UTF-8 strings (type `0x0101`).
/// Item offsets are relative to the data section. Only the first TOC is read.
fn bookmark_path(blob: &[u8]) -> Option<PathBuf> {
    const PATH_KEY: u32 = 0x1004;
    const STRING: u32 = 0x0101;
    const ARRAY: u32 = 0x0601;
    if blob.get(..4)? != b"book" {
        return None;
    }
    let base = u32_at(blob, 12)? as usize;
    let toc = base + u32_at(blob, base)? as usize;
    if u32_at(blob, toc + 4)? != 0xffff_fffe {
        return None;
    }
    let count = u32_at(blob, toc + 16)? as usize;
    let paths = (0..count).find_map(|n| {
        let entry = toc + 20 + 12 * n;
        if u32_at(blob, entry)? == PATH_KEY {
            u32_at(blob, entry + 4)
        } else {
            None
        }
    })?;
    // An item is `length, type, payload`.
    let item = |offset: u32| -> Option<(u32, &[u8])> {
        let at = base + offset as usize;
        let len = u32_at(blob, at)? as usize;
        Some((u32_at(blob, at + 4)?, blob.get(at + 8..at + 8 + len)?))
    };
    let (kind, array) = item(paths)?;
    if kind != ARRAY {
        return None;
    }
    let mut path = PathBuf::from("/");
    for offset in array.as_chunks::<4>().0 {
        let (kind, name) = item(u32::from_le_bytes(*offset))?;
        if kind != STRING {
            return None;
        }
        path.push(std::str::from_utf8(name).ok()?);
    }
    (path != Path::new("/")).then_some(path)
}

fn u32_at(blob: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(blob.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(target_os = "macos")]
mod platform {
    use std::path::PathBuf;

    /// Finder's favorites live under `~/Library/Application Support`, which
    /// TCC guards: without Full Disk Access the read fails and this returns
    /// nothing. `.sfl3` is macOS 14+, `.sfl2` before it.
    pub fn pinned_folders() -> Vec<PathBuf> {
        let Some(dir) = dirs::data_dir().map(|d| d.join("com.apple.sharedfilelist")) else {
            return Vec::new();
        };
        ["sfl3", "sfl2"]
            .iter()
            .find_map(|ext| {
                std::fs::read(dir.join(format!("com.apple.LSSharedFileList.FavoriteItems.{ext}")))
                    .ok()
            })
            .map(|bytes| super::parse_finder_favorites(&bytes))
            .unwrap_or_default()
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
mod platform {
    use std::path::PathBuf;

    /// GTK file managers (Nautilus, Nemo, Thunar…) share
    /// `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`; KDE's Dolphin keeps its own
    /// `user-places.xbel`, not read yet.
    pub fn pinned_folders() -> Vec<PathBuf> {
        dirs::config_dir()
            .and_then(|dir| std::fs::read_to_string(dir.join("gtk-3.0/bookmarks")).ok())
            .map(|text| super::parse_gtk_bookmarks(&text))
            .unwrap_or_default()
    }
}

#[cfg(all(windows, feature = "app"))]
mod platform {
    use std::path::PathBuf;

    use windows::Win32::System::Com::{
        COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    };
    use windows::core::w;

    /// Explorer's Quick Access folders (pinned plus frequent), enumerated
    /// through the shell namespace. Runs on its own thread so its COM
    /// apartment can't collide with whatever the calling executor thread
    /// already initialised. Built-in namespace only: no third-party shell
    /// extension is loaded to enumerate it.
    pub fn pinned_folders() -> Vec<PathBuf> {
        std::thread::spawn(|| unsafe {
            if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
                return Vec::new();
            }
            let found = quick_access().unwrap_or_default();
            CoUninitialize();
            found
        })
        .join()
        .unwrap_or_default()
    }

    unsafe fn quick_access() -> windows::core::Result<Vec<PathBuf>> {
        unsafe {
            let folder: IShellItem = SHCreateItemFromParsingName(
                w!("shell:::{679f85cb-0220-4080-b29b-5540cc05aab6}"),
                None,
            )?;
            let items: IEnumShellItems = folder.BindToHandler(None, &BHID_EnumItems)?;
            let mut found = Vec::new();
            loop {
                let mut item = [None];
                if items.Next(&mut item, None).is_err() {
                    break;
                }
                let Some(item) = item[0].take() else { break };
                if let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) {
                    if let Ok(path) = name.to_string() {
                        found.push(PathBuf::from(path));
                    }
                    CoTaskMemFree(Some(name.0 as *const _));
                }
            }
            Ok(found)
        }
    }
}

/// The library builds for Windows without the `app` feature (the daemon),
/// which lacks the shell bindings; nothing there needs the import.
#[cfg(all(windows, not(feature = "app")))]
mod platform {
    pub fn pinned_folders() -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal bookmark blob whose path is `components`, laid out as
    /// [`bookmark_path`] documents (48-byte header, one TOC).
    fn bookmark(components: &[&str]) -> Vec<u8> {
        fn push(out: &mut Vec<u8>, value: u32) {
            out.extend_from_slice(&value.to_le_bytes());
        }
        let mut data = vec![0; 4];
        let mut offsets = Vec::new();
        for name in components {
            offsets.push(data.len() as u32);
            push(&mut data, name.len() as u32);
            push(&mut data, 0x0101);
            data.extend_from_slice(name.as_bytes());
            data.resize(data.len().next_multiple_of(4), 0);
        }
        let array = data.len() as u32;
        push(&mut data, 4 * offsets.len() as u32);
        push(&mut data, 0x0601);
        offsets.iter().for_each(|&o| push(&mut data, o));
        let toc = data.len() as u32;
        data[..4].copy_from_slice(&toc.to_le_bytes());
        for value in [12, 0xffff_fffe, 1, 0, 1, 0x1004, array, 0] {
            push(&mut data, value);
        }
        let mut blob = b"book".to_vec();
        for value in [48 + data.len() as u32, 0x1004_0000, 48] {
            push(&mut blob, value);
        }
        blob.resize(48, 0);
        blob.extend(data);
        blob
    }

    #[test]
    fn bookmark_path_reads_the_path_components() {
        let blob = bookmark(&["Users", "me", "My Projects"]);
        assert_eq!(
            bookmark_path(&blob),
            Some(PathBuf::from("/Users/me/My Projects"))
        );
    }

    #[test]
    fn bookmark_path_rejects_garbage_without_panicking() {
        assert_eq!(bookmark_path(b"nope"), None);
        let blob = bookmark(&["Users", "me"]);
        // The last 4 bytes are the TOC entry's unused `reserved` field.
        for len in 0..blob.len() - 4 {
            assert_eq!(bookmark_path(&blob[..len]), None, "truncated at {len}");
        }
    }

    #[test]
    fn finder_favorites_keep_bookmarks_in_order_and_drop_system_items() {
        let objects = vec![
            plist::Value::String("$null".into()),
            plist::Value::Data(bookmark(&["Users", "me", "Work"])),
            plist::Value::Data(b"not a bookmark".to_vec()),
            plist::Value::Data(bookmark(&["System", "Library", "CoreServices"])),
            plist::Value::Data(bookmark(&["Applications"])),
        ];
        let mut root = plist::Dictionary::new();
        root.insert("$objects".into(), plist::Value::Array(objects));
        let mut bytes = Vec::new();
        plist::Value::Dictionary(root)
            .to_writer_binary(&mut bytes)
            .unwrap();
        assert_eq!(
            parse_finder_favorites(&bytes),
            vec![
                PathBuf::from("/Users/me/Work"),
                PathBuf::from("/Applications")
            ]
        );
        assert!(parse_finder_favorites(b"garbage").is_empty());
    }

    #[test]
    fn gtk_bookmarks_decode_paths_and_skip_remote_uris() {
        let text = "file:///home/me/My%20Stuff Stuff\n\
                    sftp://server/srv\n\
                    file:///home/me/code\n\
                    \n\
                    file:///bad%zz\n";
        assert_eq!(
            parse_gtk_bookmarks(text),
            vec![
                PathBuf::from("/home/me/My Stuff"),
                PathBuf::from("/home/me/code")
            ]
        );
    }

    #[test]
    fn new_folders_drops_known_and_duplicate_paths() {
        let p = |s: &str| PathBuf::from(s);
        let found = vec![p("/a"), p("/home"), p("/b"), p("/a")];
        assert_eq!(new_folders(found, &[p("/home")]), vec![p("/a"), p("/b")]);
    }
}
