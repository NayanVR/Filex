//! "Compress to ZIP File": pack files and folders into a new `.zip` beside
//! them, the way Explorer's "Compress to › ZIP File" and Finder's
//! "Compress" do.
//!
//! Cross-platform (the `zip` crate, Deflate via the `flate2` backend the
//! catalog already uses). Blocking — run on the background executor. It
//! reports byte progress and honours cancellation through the same
//! [`OpProgress`] the copy jobs use, and never leaves a partial archive.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipWriter};

use crate::ops::{OpCanceled, OpProgress, next_free_name};

/// Where compressing `sources` should write: `<first item's name>.zip` in
/// the first item's folder (Explorer names a multi-item archive after the
/// item that was right-clicked), or the next free `name 2.zip` variant.
/// Folders and extension-less files keep their whole name; a file drops
/// its extension (`report.pdf` → `report.zip`). Touches the disk only to
/// find a free name.
pub fn archive_path(sources: &[PathBuf]) -> Result<PathBuf> {
    let first = sources.first().context("nothing to compress")?;
    let parent = first.parent().context("item has no parent folder")?;
    let stem = if first.is_dir() {
        first.file_name()
    } else {
        first.file_stem()
    }
    .context("item has no name")?;
    let mut name = stem.to_os_string();
    name.push(".zip");
    let dest = parent.join(name);
    if fs::symlink_metadata(&dest).is_ok() {
        return next_free_name(&dest);
    }
    Ok(dest)
}

/// Write `sources` (files and whole folder trees) into a new ZIP archive
/// at `dest`, each stored under its own name at the archive root. Symlinked
/// folders are skipped rather than followed (a link cycle would never end);
/// symlinked files are stored as the file they point at. Refuses to
/// overwrite `dest`. On failure or cancellation the partial archive is
/// removed, so the caller only ever sees a complete file.
pub fn compress(sources: &[PathBuf], dest: &Path, progress: &OpProgress) -> Result<()> {
    if fs::symlink_metadata(dest).is_ok() {
        bail!("{} already exists", dest.display());
    }
    let mut entries = Vec::new();
    for source in sources {
        let name = source
            .file_name()
            .with_context(|| format!("{} has no name", source.display()))?;
        collect(source, &PathBuf::from(name), &mut entries)?;
    }
    progress.set_total(entries.iter().map(|e| e.size).sum());
    let result = write_archive(&entries, dest, progress);
    if result.is_err() {
        let _ = fs::remove_file(dest);
    }
    result
}

/// One file or folder to store: `name` is its path inside the archive.
struct Entry {
    path: PathBuf,
    name: PathBuf,
    is_dir: bool,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

fn collect(path: &Path, name: &Path, out: &mut Vec<Entry>) -> Result<()> {
    let link = fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if link.file_type().is_symlink() && fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        return Ok(());
    }
    let meta = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    out.push(Entry {
        path: path.to_path_buf(),
        name: name.to_path_buf(),
        is_dir: meta.is_dir(),
        size: if meta.is_dir() { 0 } else { meta.len() },
        modified: meta.modified().ok(),
    });
    if meta.is_dir() {
        let mut children: Vec<_> = fs::read_dir(path)
            .with_context(|| format!("listing {}", path.display()))?
            .collect::<io::Result<_>>()?;
        children.sort_by_key(|child| child.file_name());
        for child in children {
            collect(&child.path(), &name.join(child.file_name()), out)?;
        }
    }
    Ok(())
}

/// Chunk size for streaming file data: cancellation is checked between
/// chunks, so it lands promptly even inside a large file.
const CHUNK: usize = 256 * 1024;

fn write_archive(entries: &[Entry], dest: &Path, progress: &OpProgress) -> Result<()> {
    let file = File::create_new(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let mut buf = vec![0u8; CHUNK];
    for entry in entries {
        if progress.canceled() {
            return Err(OpCanceled.into());
        }
        let name = archive_name(&entry.name);
        let mut options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .large_file(entry.size >= u32::MAX as u64);
        if let Some(time) = entry.modified.and_then(zip_time) {
            options = options.last_modified_time(time);
        }
        if entry.is_dir {
            zip.add_directory(name, options)?;
            continue;
        }
        zip.start_file(name, options)?;
        let mut input =
            File::open(&entry.path).with_context(|| format!("reading {}", entry.path.display()))?;
        loop {
            if progress.canceled() {
                return Err(OpCanceled.into());
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            zip.write_all(&buf[..n])?;
            progress.add_done(n as u64);
        }
    }
    zip.finish()?.flush()?;
    Ok(())
}

/// The archive-internal name: `/`-separated on every OS, as the ZIP format
/// requires (Windows paths would otherwise store `\`).
fn archive_name(name: &Path) -> String {
    name.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// A file time as a ZIP (MS-DOS) timestamp, in UTC — std has no local
/// time zone, and an offset of a few hours is harmless for an archive.
/// `None` outside DOS's 1980–2107 range.
fn zip_time(time: std::time::SystemTime) -> Option<DateTime> {
    let secs = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    DateTime::from_date_and_time(
        u16::try_from(year).ok()?,
        month,
        day,
        (rem / 3600) as u8,
        (rem % 3600 / 60) as u8,
        (rem % 60) as u8,
    )
    .ok()
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("report.pdf"), b"pdf bytes").unwrap();
        fs::create_dir_all(dir.path().join("photos/empty")).unwrap();
        fs::write(dir.path().join("photos/a.jpg"), vec![7u8; 300_000]).unwrap();
        dir
    }

    fn names(zip: &Path) -> Vec<String> {
        let archive = zip::ZipArchive::new(File::open(zip).unwrap()).unwrap();
        let mut names: Vec<String> = archive
            .file_names()
            .map(|n| n.unwrap().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn archive_is_named_after_the_first_item() {
        let dir = tree();
        let file = dir.path().join("report.pdf");
        let folder = dir.path().join("photos");
        assert_eq!(
            archive_path(std::slice::from_ref(&file)).unwrap(),
            dir.path().join("report.zip")
        );
        assert_eq!(
            archive_path(&[folder.clone(), file]).unwrap(),
            dir.path().join("photos.zip")
        );
        fs::write(dir.path().join("photos.zip"), b"").unwrap();
        assert_eq!(
            archive_path(&[folder]).unwrap(),
            dir.path().join("photos 2.zip")
        );
    }

    #[test]
    fn compresses_files_and_folder_trees_with_forward_slashes() {
        let dir = tree();
        let dest = dir.path().join("out.zip");
        let sources = [dir.path().join("report.pdf"), dir.path().join("photos")];
        let progress = OpProgress::default();
        compress(&sources, &dest, &progress).unwrap();
        assert_eq!(
            names(&dest),
            ["photos/", "photos/a.jpg", "photos/empty/", "report.pdf"]
        );
        assert_eq!(progress.fraction(), Some(1.0));
        let mut archive = zip::ZipArchive::new(File::open(&dest).unwrap()).unwrap();
        let mut body = Vec::new();
        archive
            .by_name("photos/a.jpg")
            .unwrap()
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, vec![7u8; 300_000]);
    }

    #[test]
    fn refuses_to_overwrite() {
        let dir = tree();
        let dest = dir.path().join("taken.zip");
        fs::write(&dest, b"keep me").unwrap();
        let err = compress(
            &[dir.path().join("report.pdf")],
            &dest,
            &OpProgress::default(),
        );
        assert!(err.is_err());
        assert_eq!(fs::read(&dest).unwrap(), b"keep me");
    }

    #[test]
    fn canceled_compress_reports_cancel_and_leaves_no_partial() {
        let dir = tree();
        let dest = dir.path().join("out.zip");
        let progress = OpProgress::default();
        progress.request_cancel();
        let err = compress(&[dir.path().join("photos")], &dest, &progress).unwrap_err();
        assert!(err.is::<OpCanceled>());
        assert!(!dest.exists());
    }

    #[test]
    fn civil_dates_round_trip_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(20_745), (2026, 10, 19));
        let time = std::time::UNIX_EPOCH + std::time::Duration::from_secs(20_745 * 86_400 + 3_725);
        let zip = zip_time(time).unwrap();
        assert_eq!(
            (zip.year(), zip.month(), zip.day(), zip.hour(), zip.minute()),
            (2026, 10, 19, 1, 2)
        );
        assert_eq!(zip_time(std::time::UNIX_EPOCH), None, "before 1980");
    }
}
