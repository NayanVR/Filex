//! Windows live watcher built on `ReadDirectoryChangesW` (RDCW).
//!
//! RDCW watches a whole subtree with one handle and — unlike the USN
//! Journal — requires **no elevation**: it works for any directory the
//! user can read, on any filesystem (NTFS, FAT, network shares). It is
//! therefore the universal/unprivileged watcher. The admin-only USN
//! Journal watcher is future work, paired with the FSCTL_ENUM_USN_DATA
//! bootstrap, because USN records identify files by FRN and need the
//! FRN-keyed index that bootstrap builds (docs/indexing-architecture.md §1–2).
//!
//! Assumptions about the OS:
//! - `FILE_NOTIFY_INFORMATION` records are a packed chain (NextEntryOffset
//!   links, 0 terminates) of UTF-16 names *relative to the watched root*,
//!   with `\` separators.
//! - A zero-byte completion means the OS buffer overflowed and changes
//!   were dropped — only a rescan restores truth.
//! - Renames arrive as OLD_NAME/NEW_NAME pairs; they are treated as
//!   independent Remove + Upsert, which idempotent delta application
//!   absorbs (same policy as the macOS and Linux watchers).
//!
//! The buffer parser and delta mapping are pure and compiled on every OS
//! so fixture tests run in any CI; only the RDCW loop is Windows-gated.

use std::path::{Path, PathBuf};

use super::watcher::FsDelta;

// Action codes from winnt.h (stable ABI).
pub const FILE_ACTION_ADDED: u32 = 1;
pub const FILE_ACTION_REMOVED: u32 = 2;
pub const FILE_ACTION_MODIFIED: u32 = 3;
pub const FILE_ACTION_RENAMED_OLD_NAME: u32 = 4;
pub const FILE_ACTION_RENAMED_NEW_NAME: u32 = 5;

/// One decoded `FILE_NOTIFY_INFORMATION` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNotifyEvent {
    pub action: u32,
    /// Path relative to the watched root, already split on `\`.
    pub relative_path: PathBuf,
}

/// Parse a `FILE_NOTIFY_INFORMATION` chain. Records with non-UTF-16 names
/// or malformed offsets terminate parsing of the chain (kernel output is
/// well-formed; anything else is a broken fixture).
pub fn parse_notify_buffer(buf: &[u8]) -> Vec<FileNotifyEvent> {
    let mut events = Vec::new();
    let mut offset = 0usize;
    loop {
        if offset + 12 > buf.len() {
            break;
        }
        let rec = &buf[offset..];
        let next = u32::from_ne_bytes(rec[0..4].try_into().expect("4 bytes")) as usize;
        let action = u32::from_ne_bytes(rec[4..8].try_into().expect("4 bytes"));
        let name_bytes = u32::from_ne_bytes(rec[8..12].try_into().expect("4 bytes")) as usize;

        if offset + 12 + name_bytes > buf.len() || name_bytes % 2 != 0 {
            break;
        }
        let name_utf16: Vec<u16> = rec[12..12 + name_bytes]
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
            .collect();
        if let Ok(name) = String::from_utf16(&name_utf16) {
            // Backslash-separated, relative to the watch root. Build the
            // path component-wise so fixtures behave identically on
            // non-Windows hosts (where `\` is not a separator).
            let relative_path: PathBuf = name.split('\\').collect();
            events.push(FileNotifyEvent { action, relative_path });
        }

        if next == 0 {
            break;
        }
        offset += next;
    }
    events
}

/// Translate one RDCW record into a normalized delta. `presence` is the
/// current filesystem truth for the absolute path — `Some(is_dir)` if it
/// exists, `None` if not — because RDCW actions don't carry a file/dir
/// bit. Pure — fixture-tested on every OS.
pub fn event_to_delta(
    root: &Path,
    event: &FileNotifyEvent,
    presence: Option<bool>,
) -> Option<FsDelta> {
    let path = root.join(&event.relative_path);
    match event.action {
        FILE_ACTION_ADDED | FILE_ACTION_RENAMED_NEW_NAME => match presence {
            Some(is_dir) => Some(FsDelta::Upsert { path, is_dir }),
            // Already gone again (create+delete inside one poll window).
            None => Some(FsDelta::Remove { path }),
        },
        FILE_ACTION_REMOVED | FILE_ACTION_RENAMED_OLD_NAME => Some(FsDelta::Remove { path }),
        _ => None, // MODIFIED and unknown actions: names unaffected
    }
}

#[cfg(target_os = "windows")]
pub use imp::{
    DirChangesWatcher, UsnBootstrap, UsnJournalInfo, UsnJournalWatcher, connect_index_pipe,
    query_usn_journal, run_pipe_server, usn_bootstrap, volume_root_drive,
};

#[cfg(target_os = "windows")]
mod imp {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc::Sender;
    use std::thread::JoinHandle;

    use anyhow::{Context as _, Result, bail};
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_HANDLE_EOF, ERROR_NOTIFY_ENUM_DIR, GENERIC_READ, HANDLE,
    };
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};
    use windows::Win32::System::Threading::CreateEventW;
    use windows::Win32::System::Ioctl::{
        FSCTL_ENUM_USN_DATA, FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0,
        READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
    };
    use windows::core::HSTRING;

    use crate::index::usn::{JOURNAL_REASON_MASK, MftEntry, journal_record_to_delta, parse_usn_output};
    use crate::index::VolumeIndex;

    /// Sendable wrapper: HANDLEs are agile; this one is only used for
    /// ReadDirectoryChangesW on the reader thread and CancelIoEx/CloseHandle
    /// on drop.
    struct DirHandle(HANDLE);
    unsafe impl Send for DirHandle {}
    unsafe impl Sync for DirHandle {}

    /// Watches a directory subtree via an overlapped RDCW loop on a
    /// dedicated thread. RDCW only tracks changes from the moment a read
    /// is *pending*, so `spawn` hands the thread an "armed" channel and
    /// waits for the first read to be queued before returning — callers
    /// are guaranteed coverage from the instant `spawn` completes (the
    /// watcher-before-bootstrap ordering depends on this). Dropping
    /// cancels the pending read and closes the handle.
    pub struct DirChangesWatcher {
        handle: std::sync::Arc<DirHandle>,
        thread: Option<JoinHandle<()>>,
    }

    impl DirChangesWatcher {
        /// Start watching `root` (must be canonicalized). Works without
        /// elevation for any directory the user can read.
        pub fn spawn(root: &Path, deltas: Sender<Vec<FsDelta>>) -> Result<Self> {
            // SAFETY: directory-handle open for change notification;
            // OVERLAPPED so reads are async and cancellable.
            let raw = unsafe {
                CreateFileW(
                    &HSTRING::from(root.as_os_str()),
                    FILE_LIST_DIRECTORY.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    None,
                )
            }
            .with_context(|| format!("opening {} for change notification", root.display()))?;

            let handle = std::sync::Arc::new(DirHandle(raw));
            let (armed_tx, armed_rx) = std::sync::mpsc::channel();
            let thread = std::thread::Builder::new().name("filex-rdcw".into()).spawn({
                let handle = handle.clone();
                let root = root.to_path_buf();
                move || reader_loop(&handle, &root, &deltas, &armed_tx)
            })?;

            armed_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .context("change-notification reader failed to arm (see log)")?;
            Ok(Self { handle, thread: Some(thread) })
        }
    }

    impl Drop for DirChangesWatcher {
        fn drop(&mut self) {
            // SAFETY: cancelling/closing our own handle; the reader thread's
            // pending RDCW completes with an error and the loop exits.
            unsafe {
                CancelIoEx(self.handle.0, None).ok();
                CloseHandle(self.handle.0).ok();
            }
            if let Some(thread) = self.thread.take() {
                thread.join().ok();
            }
        }
    }

    fn reader_loop(
        handle: &DirHandle,
        root: &Path,
        deltas: &Sender<Vec<FsDelta>>,
        armed: &Sender<()>,
    ) {
        use windows::Win32::Foundation::ERROR_OPERATION_ABORTED;

        // SAFETY: plain event creation (auto-reset, unnamed, unsignaled).
        let event = match unsafe { CreateEventW(None, false, false, None) } {
            Ok(event) => VolumeHandle(event),
            Err(err) => {
                tracing::warn!("CreateEventW for change notification failed: {err}");
                return;
            }
        };
        // u64-backed: ReadDirectoryChangesW fails with ERROR_NOACCESS if
        // the buffer is not DWORD-aligned; Vec<u8> alignment is luck.
        let mut buf = vec![0u64; 8 * 1024]; // 64 KiB
        let buf_len_bytes = (buf.len() * size_of::<u64>()) as u32;
        let mut first_read = true;

        loop {
            let mut overlapped = OVERLAPPED::default();
            overlapped.hEvent = event.0;
            // SAFETY: buf and overlapped outlive the pending I/O — both
            // live until GetOverlappedResult(bWait=true) completes below.
            let issued = unsafe {
                ReadDirectoryChangesW(
                    handle.0,
                    buf.as_mut_ptr() as *mut _,
                    buf_len_bytes,
                    true, // recursive
                    FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME,
                    None, // bytes come from GetOverlappedResult
                    Some(&mut overlapped),
                    None,
                )
            };
            if let Err(err) = issued {
                tracing::warn!(
                    "ReadDirectoryChangesW on {} failed to start: {err}",
                    root.display()
                );
                break;
            }
            if first_read {
                first_read = false;
                // From this point the OS is recording changes; unblock
                // spawn(). Receiver gone means spawn timed out — keep
                // running anyway, the watcher itself is healthy.
                armed.send(()).ok();
            }

            let mut bytes_returned = 0u32;
            // SAFETY: waits for the exact I/O issued above.
            let result = unsafe {
                GetOverlappedResult(handle.0, &overlapped, &mut bytes_returned, true)
            };
            let batch = match result {
                Err(err) if err.code() == ERROR_NOTIFY_ENUM_DIR.to_hresult() => {
                    // Too many changes for the OS to enumerate: rescan.
                    vec![FsDelta::Rescan { path: root.to_path_buf() }]
                }
                Err(err) => {
                    // Cancellation (drop) is expected; anything else must
                    // be visible — a silent break means silent staleness.
                    if err.code() != ERROR_OPERATION_ABORTED.to_hresult() {
                        tracing::warn!(
                            "change notification on {} failed: {err}",
                            root.display()
                        );
                    }
                    break;
                }
                Ok(()) if bytes_returned == 0 => {
                    // Our buffer overflowed; changes were dropped.
                    vec![FsDelta::Rescan { path: root.to_path_buf() }]
                }
                Ok(()) => {
                    // SAFETY: the kernel wrote bytes_returned bytes.
                    let bytes = unsafe {
                        std::slice::from_raw_parts(
                            buf.as_ptr() as *const u8,
                            bytes_returned as usize,
                        )
                    };
                    parse_notify_buffer(bytes)
                        .iter()
                        .filter_map(|event| {
                            let presence =
                                std::fs::symlink_metadata(root.join(&event.relative_path))
                                    .ok()
                                    .map(|m| m.is_dir());
                            event_to_delta(root, event, presence)
                        })
                        .collect()
                }
            };

            if !batch.is_empty() && deltas.send(batch).is_err() {
                break; // receiver gone: shutting down
            }
        }
    }

    // ---- named-pipe transport for the index service (ipc module) ----

    /// Serve [`crate::index::ipc`] clients over a named pipe, one pipe
    /// instance (and thread) per connection, until `accept_limit`
    /// connections have been served (None = forever). The pipe's default
    /// security descriptor already restricts writing to the creating user
    /// and administrators, which is the intended audience.
    pub fn run_pipe_server(
        pipe_name: &str,
        host: std::sync::Arc<dyn crate::index::ipc::IndexHost>,
        accept_limit: Option<usize>,
    ) -> Result<()> {
        use std::os::windows::io::FromRawHandle as _;
        use windows::Win32::Foundation::ERROR_PIPE_CONNECTED;
        use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
        use windows::Win32::System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
            PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        };

        let mut served = 0usize;
        while accept_limit.is_none_or(|limit| served < limit) {
            // SAFETY: standard named-pipe creation; name outlives the call.
            let handle = unsafe {
                CreateNamedPipeW(
                    &HSTRING::from(pipe_name),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    64 * 1024,
                    64 * 1024,
                    0,
                    None,
                )
            };
            if handle.is_invalid() {
                return Err(windows::core::Error::from_thread())
                    .with_context(|| format!("creating named pipe {pipe_name}"));
            }
            // SAFETY: freshly created, unconnected pipe instance.
            let connected = unsafe { ConnectNamedPipe(handle, None) };
            if let Err(err) = connected
                && err.code() != ERROR_PIPE_CONNECTED.to_hresult()
            {
                // SAFETY: closing the instance we just created.
                unsafe { CloseHandle(handle).ok() };
                return Err(err).context("waiting for a pipe client");
            }
            served += 1;

            // SAFETY: ownership of the connected instance moves into the
            // File, whose Drop closes it when the connection ends.
            let stream = unsafe { std::fs::File::from_raw_handle(handle.0 as _) };
            let host = host.clone();
            std::thread::Builder::new().name("filex-ipc".into()).spawn(move || {
                let reader = match stream.try_clone() {
                    Ok(reader) => reader,
                    Err(err) => {
                        tracing::warn!("pipe clone failed: {err}");
                        return;
                    }
                };
                if let Err(err) = crate::index::ipc::serve_connection(reader, stream, &*host) {
                    tracing::warn!("ipc connection ended with error: {err:#}");
                }
            })?;
        }
        Ok(())
    }

    /// Connect to the index service's pipe. `attempts` > 1 retries with a
    /// short sleep (for busy pipe instances or a just-starting service);
    /// use 1 for a fast liveness probe. Returns a duplex stream for
    /// [`crate::index::ipc::RemoteIndex::connect`].
    pub fn connect_index_pipe(pipe_name: &str, attempts: u32) -> Result<std::fs::File> {
        let mut last_err = None;
        for attempt in 0..attempts.max(1) {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            match std::fs::File::options().read(true).write(true).open(pipe_name) {
                Ok(file) => return Ok(file),
                Err(err) => last_err = Some(err),
            }
        }
        Err(last_err.expect("at least one attempt"))
            .with_context(|| format!("connecting to the index service at {pipe_name}"))
    }

    // ---- USN fast path (requires Administrator; volume roots only) ----

    /// If `path` is a volume root (`C:\`, including the `\\?\C:\` verbatim
    /// form canonicalize produces), return its drive letter. The USN fast
    /// path is only offered for volume roots: with the whole volume
    /// indexed, every journal record's FRN and parent FRN resolve, so
    /// renames/moves never leave dangling subtrees.
    pub fn volume_root_drive(path: &Path) -> Option<u8> {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        let drive = match components.next()? {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(d) | Prefix::VerbatimDisk(d) => d,
                _ => return None,
            },
            _ => return None,
        };
        if !matches!(components.next(), Some(Component::RootDir)) {
            return None;
        }
        components.next().is_none().then_some(drive)
    }

    /// Owned volume handle, closed on drop.
    struct VolumeHandle(HANDLE);
    unsafe impl Send for VolumeHandle {}
    unsafe impl Sync for VolumeHandle {}

    impl Drop for VolumeHandle {
        fn drop(&mut self) {
            // SAFETY: closing a handle we own.
            unsafe { CloseHandle(self.0).ok() };
        }
    }

    /// Open `\\.\X:` for USN ioctls. Requires Administrator (or Backup
    /// Operators); without elevation this fails with ACCESS_DENIED and the
    /// caller falls back to the unprivileged path.
    fn open_volume(drive: u8) -> Result<VolumeHandle> {
        let device = format!(r"\\.\{}:", drive as char);
        // SAFETY: standard volume-device open.
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(device.as_str()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .with_context(|| {
            format!("opening volume {device} for USN access (requires Administrator)")
        })?;
        Ok(VolumeHandle(handle))
    }

    /// The FRN of a directory, read from its open handle. Assumes NTFS
    /// 64-bit file ids (the volume-root gate guarantees a local NTFS-style
    /// drive; ReFS 128-bit ids fail the USN record parse instead).
    fn frn_of_directory(path: &Path) -> Result<u64> {
        // SAFETY: standard directory-handle open + info query.
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(path.as_os_str()),
                0, // attributes only
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        }
        .with_context(|| format!("opening {} to read its file id", path.display()))?;
        let guard = VolumeHandle(handle);
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: valid handle and out-pointer.
        unsafe { GetFileInformationByHandle(guard.0, &mut info) }
            .with_context(|| format!("querying file id of {}", path.display()))?;
        Ok((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
    }

    /// Current journal identity/position — consulted for checkpoint replay
    /// validation and captured before bootstrap enumeration.
    #[derive(Debug, Clone, Copy)]
    pub struct UsnJournalInfo {
        pub journal_id: u64,
        pub first_usn: i64,
        pub next_usn: i64,
    }

    pub fn query_usn_journal(root: &Path) -> Result<UsnJournalInfo> {
        let drive = volume_root_drive(root)
            .ok_or_else(|| anyhow::anyhow!("{} is not a volume root", root.display()))?;
        let volume = open_volume(drive)?;
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut bytes = 0u32;
        // SAFETY: valid handle; out-buffer sized for USN_JOURNAL_DATA_V0.
        unsafe {
            DeviceIoControl(
                volume.0,
                FSCTL_QUERY_USN_JOURNAL,
                None,
                0,
                Some(&mut data as *mut _ as *mut _),
                size_of::<USN_JOURNAL_DATA_V0>() as u32,
                Some(&mut bytes),
                None,
            )
        }
        .context("querying the USN journal (is the volume NTFS with journaling enabled?)")?;
        Ok(UsnJournalInfo {
            journal_id: data.UsnJournalID,
            first_usn: data.FirstUsn,
            next_usn: data.NextUsn,
        })
    }

    pub struct UsnBootstrap {
        pub index: VolumeIndex,
        pub journal: UsnJournalInfo,
    }

    /// Build the index by enumerating the volume's MFT via
    /// `FSCTL_ENUM_USN_DATA` — the "Everything" bootstrap. Seconds for
    /// millions of files, versus minutes of directory walking. The journal
    /// position is captured *before* enumeration so events raced by the
    /// enumeration replay through the watcher (idempotently) afterwards.
    pub fn usn_bootstrap(root: &Path) -> Result<UsnBootstrap> {
        let Some(drive) = volume_root_drive(root) else {
            bail!("USN bootstrap requires a volume root, got {}", root.display());
        };
        let journal = query_usn_journal(root)?;
        let root_frn = frn_of_directory(root)?;
        let volume = open_volume(drive)?;

        let mut records: Vec<MftEntry> = Vec::new();
        let mut input = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: journal.next_usn,
        };
        // u64-backed: USN output buffers must be DWORD64-aligned.
        let mut buf = vec![0u64; (1 << 20) / size_of::<u64>()];
        let buf_len_bytes = (buf.len() * size_of::<u64>()) as u32;
        loop {
            let mut bytes = 0u32;
            // SAFETY: valid handle; in/out buffers live across the call.
            let result = unsafe {
                DeviceIoControl(
                    volume.0,
                    FSCTL_ENUM_USN_DATA,
                    Some(&input as *const _ as *const _),
                    size_of::<MFT_ENUM_DATA_V0>() as u32,
                    Some(buf.as_mut_ptr() as *mut _),
                    buf_len_bytes,
                    Some(&mut bytes),
                    None,
                )
            };
            match result {
                Ok(()) => {
                    // SAFETY: the kernel wrote `bytes` bytes.
                    let out = unsafe {
                        std::slice::from_raw_parts(buf.as_ptr() as *const u8, bytes as usize)
                    };
                    let (next_frn, batch) = parse_usn_output(out);
                    if batch.is_empty() {
                        break; // defensive: no forward progress
                    }
                    records.extend(batch.iter().map(MftEntry::from));
                    input.StartFileReferenceNumber = next_frn;
                }
                Err(err) if err.code() == ERROR_HANDLE_EOF.to_hresult() => break,
                Err(err) => {
                    return Err(err).context("enumerating the MFT (FSCTL_ENUM_USN_DATA)");
                }
            }
        }

        let (index, orphans) = crate::index::usn::build_index_from_mft(root, root_frn, &records);
        if orphans > 0 {
            tracing::warn!("MFT enumeration produced {orphans} unreachable records (skipped)");
        }
        Ok(UsnBootstrap { index, journal })
    }

    /// Tails the USN journal via bounded `FSCTL_READ_USN_JOURNAL` reads on
    /// a dedicated thread, translating records into native-keyed deltas.
    /// Journal truncation/deletion degrades to a root rescan.
    ///
    /// Reads use the ioctl's own Timeout (1s) rather than blocking
    /// indefinitely: a synchronous journal read parked in the kernel is
    /// not reliably cancellable with CancelIoEx, so shutdown works by
    /// flagging and letting the loop wake — Drop then joins within ~1s.
    pub struct UsnJournalWatcher {
        handle: Arc<VolumeHandle>,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<JoinHandle<()>>,
        journal_id: u64,
        next_usn: Arc<AtomicU64>,
    }

    impl UsnJournalWatcher {
        pub fn spawn(
            root: &Path,
            journal_id: u64,
            start_usn: i64,
            deltas: Sender<Vec<FsDelta>>,
        ) -> Result<Self> {
            let Some(drive) = volume_root_drive(root) else {
                bail!("USN journal watcher requires a volume root");
            };
            let handle = Arc::new(open_volume(drive)?);
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let next_usn = Arc::new(AtomicU64::new(start_usn as u64));
            let thread = std::thread::Builder::new().name("filex-usn-journal".into()).spawn({
                let handle = handle.clone();
                let shutdown = shutdown.clone();
                let next_usn = next_usn.clone();
                let root = root.to_path_buf();
                move || journal_loop(&handle, &root, journal_id, &next_usn, &deltas, &shutdown)
            })?;
            Ok(Self { handle, shutdown, thread: Some(thread), journal_id, next_usn })
        }

        pub fn journal_id(&self) -> u64 {
            self.journal_id
        }

        /// Shared USN position; read after the watcher is dropped and the
        /// writer joined, it is the exact checkpoint through which every
        /// record has been applied.
        pub fn next_usn_handle(&self) -> Arc<AtomicU64> {
            self.next_usn.clone()
        }
    }

    impl Drop for UsnJournalWatcher {
        fn drop(&mut self) {
            self.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
            // Best-effort nudge; the loop exits on its own within one
            // read timeout even if the cancel is ignored.
            // SAFETY: cancelling I/O on our own handle.
            unsafe { CancelIoEx(self.handle.0, None).ok() };
            if let Some(thread) = self.thread.take() {
                thread.join().ok();
            }
        }
    }

    fn journal_loop(
        handle: &VolumeHandle,
        root: &Path,
        journal_id: u64,
        next_usn: &AtomicU64,
        deltas: &Sender<Vec<FsDelta>>,
        shutdown: &std::sync::atomic::AtomicBool,
    ) {
        // u64-backed: USN output buffers must be DWORD64-aligned.
        let mut buf = vec![0u64; (256 * 1024) / size_of::<u64>()];
        let buf_len_bytes = (buf.len() * size_of::<u64>()) as u32;
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            let input = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: next_usn.load(Ordering::Relaxed) as i64,
                ReasonMask: JOURNAL_REASON_MASK,
                ReturnOnlyOnClose: 0,
                Timeout: 1,        // seconds; bounds each read so the
                BytesToWaitFor: 1, // shutdown flag is honored promptly
                UsnJournalID: journal_id,
            };
            let mut bytes = 0u32;
            // SAFETY: valid handle; buffers live across the blocking call.
            let result = unsafe {
                DeviceIoControl(
                    handle.0,
                    FSCTL_READ_USN_JOURNAL,
                    Some(&input as *const _ as *const _),
                    size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
                    Some(buf.as_mut_ptr() as *mut _),
                    buf_len_bytes,
                    Some(&mut bytes),
                    None,
                )
            };
            match result {
                Ok(()) if bytes as usize >= 8 => {
                    // SAFETY: the kernel wrote `bytes` bytes.
                    let out = unsafe {
                        std::slice::from_raw_parts(buf.as_ptr() as *const u8, bytes as usize)
                    };
                    let (next, records) = parse_usn_output(out);
                    next_usn.store(next, Ordering::Relaxed);
                    let batch: Vec<FsDelta> =
                        records.iter().filter_map(journal_record_to_delta).collect();
                    if !batch.is_empty() && deltas.send(batch).is_err() {
                        break; // receiver gone: shutting down
                    }
                }
                Ok(()) => break, // defensive: no continuation returned
                Err(err) => {
                    // Cancellation (drop) is silent; anything else —
                    // journal deleted, truncated past our USN, resized —
                    // means records were lost: reconcile with a rescan.
                    use windows::Win32::Foundation::ERROR_OPERATION_ABORTED;
                    if err.code() != ERROR_OPERATION_ABORTED.to_hresult() {
                        tracing::warn!("USN journal read failed ({err}); rescanning");
                        deltas
                            .send(vec![FsDelta::Rescan { path: root.to_path_buf() }])
                            .ok();
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a FILE_NOTIFY_INFORMATION chain from (action, name) pairs.
    fn encode_chain(records: &[(u32, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        for (i, (action, name)) in records.iter().enumerate() {
            let name_utf16: Vec<u16> = name.encode_utf16().collect();
            let name_bytes = name_utf16.len() * 2;
            // Records are DWORD-aligned; the kernel pads NextEntryOffset.
            let record_len = 12 + name_bytes;
            let padded = (record_len + 3) & !3;
            let next = if i + 1 == records.len() { 0 } else { padded as u32 };

            buf.extend_from_slice(&next.to_ne_bytes());
            buf.extend_from_slice(&action.to_ne_bytes());
            buf.extend_from_slice(&(name_bytes as u32).to_ne_bytes());
            for unit in &name_utf16 {
                buf.extend_from_slice(&unit.to_ne_bytes());
            }
            buf.resize(buf.len() + (padded - record_len), 0);
        }
        buf
    }

    #[test]
    fn parses_chained_records_with_utf16_names() {
        let buf = encode_chain(&[
            (FILE_ACTION_ADDED, r"docs\new file.txt"),
            (FILE_ACTION_REMOVED, "übersicht.md"),
        ]);
        let events = parse_notify_buffer(&buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].action, FILE_ACTION_ADDED);
        assert_eq!(
            events[0].relative_path,
            PathBuf::from("docs").join("new file.txt")
        );
        assert_eq!(events[1].relative_path, PathBuf::from("übersicht.md"));
    }

    #[test]
    fn parses_single_record_with_zero_next_offset() {
        let buf = encode_chain(&[(FILE_ACTION_MODIFIED, "x.txt")]);
        assert_eq!(parse_notify_buffer(&buf).len(), 1);
    }

    #[test]
    fn parser_stops_on_malformed_length() {
        let mut buf = encode_chain(&[(FILE_ACTION_ADDED, "ok.txt")]);
        // Claim a name longer than the buffer.
        buf[8..12].copy_from_slice(&1000u32.to_ne_bytes());
        assert!(parse_notify_buffer(&buf).is_empty());
    }

    #[test]
    fn maps_added_and_renamed_new_using_presence() {
        let root = Path::new("/root");
        let added = FileNotifyEvent {
            action: FILE_ACTION_ADDED,
            relative_path: PathBuf::from("a.txt"),
        };
        assert_eq!(
            event_to_delta(root, &added, Some(false)),
            Some(FsDelta::Upsert { path: root.join("a.txt"), is_dir: false })
        );

        let renamed = FileNotifyEvent {
            action: FILE_ACTION_RENAMED_NEW_NAME,
            relative_path: PathBuf::from("dir"),
        };
        assert_eq!(
            event_to_delta(root, &renamed, Some(true)),
            Some(FsDelta::Upsert { path: root.join("dir"), is_dir: true })
        );
        // Vanished before we could stat it: safe to treat as removal.
        assert_eq!(
            event_to_delta(root, &added, None),
            Some(FsDelta::Remove { path: root.join("a.txt") })
        );
    }

    #[test]
    fn maps_removed_and_renamed_old_as_removes() {
        let root = Path::new("/root");
        for action in [FILE_ACTION_REMOVED, FILE_ACTION_RENAMED_OLD_NAME] {
            let event = FileNotifyEvent { action, relative_path: PathBuf::from("gone.txt") };
            assert_eq!(
                event_to_delta(root, &event, None),
                Some(FsDelta::Remove { path: root.join("gone.txt") })
            );
        }
    }

    #[test]
    fn modified_and_unknown_actions_map_to_nothing() {
        let root = Path::new("/root");
        for action in [FILE_ACTION_MODIFIED, 99] {
            let event = FileNotifyEvent { action, relative_path: PathBuf::from("x") };
            assert_eq!(event_to_delta(root, &event, Some(false)), None);
        }
    }

    #[test]
    fn nested_relative_paths_join_with_native_separators() {
        let root = Path::new("/root");
        let event = FileNotifyEvent {
            action: FILE_ACTION_ADDED,
            relative_path: PathBuf::from("a").join("b").join("c.txt"),
        };
        let Some(FsDelta::Upsert { path, .. }) = event_to_delta(root, &event, Some(false)) else {
            panic!("expected upsert");
        };
        assert_eq!(path, root.join("a").join("b").join("c.txt"));
    }

    /// Live smoke tests against the real Windows APIs — these are what
    /// CI's Windows runner executes; fixture tests above cover the logic
    /// on every OS. The USN tests exercise the elevated fast path (CI
    /// runners are Administrator) and skip gracefully when unelevated.
    #[cfg(target_os = "windows")]
    mod live {
        use super::super::*;
        use std::fs;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        fn wait_for(
            rx: &mpsc::Receiver<Vec<FsDelta>>,
            timeout: Duration,
            mut pred: impl FnMut(&FsDelta) -> bool,
        ) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if let Ok(batch) = rx.recv_timeout(Duration::from_millis(200)) {
                    if batch.iter().any(&mut pred) {
                        return true;
                    }
                }
            }
            false
        }

        #[test]
        fn volume_root_detection_handles_verbatim_and_plain_forms() {
            assert_eq!(volume_root_drive(Path::new(r"C:\")), Some(b'C'));
            assert_eq!(volume_root_drive(Path::new(r"\\?\C:\")), Some(b'C'));
            assert_eq!(volume_root_drive(Path::new(r"C:\Users")), None);
            assert_eq!(volume_root_drive(Path::new(r"\\server\share")), None);
            // The form canonicalize actually produces:
            let canonical = Path::new(r"C:\").canonicalize().unwrap();
            assert_eq!(volume_root_drive(&canonical), Some(b'C'));
        }

        #[test]
        fn rdcw_reports_real_file_creation_and_removal() {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let (tx, rx) = mpsc::channel();
            let _watcher = DirChangesWatcher::spawn(&root, tx).unwrap();

            let target = root.join("rdcw-smoke.txt");
            fs::write(&target, b"x").unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Upsert { path, is_dir: false } if path == &target)
                }),
                "no Upsert for created file"
            );

            fs::remove_file(&target).unwrap();
            assert!(
                wait_for(&rx, Duration::from_secs(10), |d| {
                    matches!(d, FsDelta::Remove { path } if path == &target)
                }),
                "no Remove for deleted file"
            );
        }

        /// Requires elevation: tails the real USN journal on whichever
        /// volume hosts %TEMP% (not necessarily C: — CI runners map temp
        /// to D:) and verifies a created file arrives as a native delta.
        #[test]
        fn usn_journal_reports_real_file_creation() {
            let temp = std::env::temp_dir().canonicalize().unwrap();
            // Prefix + RootDir components = the volume root of %TEMP%.
            let root: PathBuf = temp.components().take(2).collect();
            assert!(
                volume_root_drive(&root).is_some(),
                "could not derive a volume root from {}",
                temp.display()
            );
            let info = match query_usn_journal(&root) {
                Ok(info) => info,
                Err(err) => {
                    eprintln!("skipping USN live test (needs Administrator): {err:#}");
                    return;
                }
            };

            let (tx, rx) = mpsc::channel();
            let _watcher =
                UsnJournalWatcher::spawn(&root, info.journal_id, info.next_usn, tx).unwrap();

            let name = format!("usn-smoke-{}.txt", std::process::id());
            let target = temp.join(&name);
            fs::write(&target, b"x").unwrap();

            let seen = wait_for(&rx, Duration::from_secs(20), |d| {
                matches!(d, FsDelta::NativeUpsert { name: n, is_dir: false, .. } if *n == name)
            });
            fs::remove_file(&target).ok();
            assert!(seen, "no NativeUpsert from the USN journal for {name}");
        }

        /// Real named-pipe roundtrip: server thread + client in-process,
        /// exercising the exact transport the elevated service uses.
        #[test]
        fn index_service_pipe_roundtrip() {
            use crate::index::ipc::{IndexHost, MultiRootHost, RemoteIndex};
            use crate::index::{ROOT, VolumeIndex};
            use std::sync::{Arc, RwLock};

            let mut index = VolumeIndex::new(r"C:\vol");
            index.insert(ROOT, "pipe-hit.txt", false).unwrap();
            let host: Arc<dyn IndexHost> = Arc::new(MultiRootHost {
                roots: vec![(PathBuf::from(r"C:\vol"), Arc::new(RwLock::new(index)))],
            });

            let pipe_name = format!(r"\\.\pipe\filex-test-{}", std::process::id());
            let server = std::thread::spawn({
                let pipe_name = pipe_name.clone();
                move || run_pipe_server(&pipe_name, host, Some(1))
            });

            let stream = connect_index_pipe(&pipe_name, 20).unwrap();
            let reader = stream.try_clone().unwrap();
            let mut client = RemoteIndex::connect(reader, stream).unwrap();

            let hits = client.search("pipe-hit", 10).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].name, "pipe-hit.txt");
            assert_eq!(client.status().unwrap().roots[0].files, 1);

            drop(client);
            server.join().unwrap().unwrap();
        }

        /// Requires elevation and enumerates the whole C: MFT — minutes of
        /// wall time on large volumes, so opt-in: run with
        /// `cargo test -- --ignored` on Windows to validate tier 2.
        #[test]
        #[ignore = "full-volume MFT enumeration; run explicitly on Windows"]
        fn usn_bootstrap_enumerates_the_volume() {
            let root = Path::new(r"C:\").canonicalize().unwrap();
            let boot = usn_bootstrap(&root).expect("usn_bootstrap (needs Administrator)");
            assert!(boot.index.len() > 1000, "suspiciously small volume index");
            // Windows itself must be findable on any Windows install.
            assert!(!boot.index.search("notepad", 100).is_empty());
        }
    }
}
