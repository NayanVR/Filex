//! Per-OS "hidden" metadata bits for directory listings.

/// Whether the OS marks this entry hidden beyond the dotfile convention.
/// Windows: `FILE_ATTRIBUTE_HIDDEN`. macOS: the Finder `UF_HIDDEN` flag
/// in `st_flags`. Linux has no such bit, only dotfiles.
#[cfg(target_os = "windows")]
pub(super) fn has_hidden_flag(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    meta.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
}

/// See the Windows variant.
#[cfg(target_os = "macos")]
pub(super) fn has_hidden_flag(meta: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt as _;
    const UF_HIDDEN: u32 = 0x8000;
    meta.st_flags() & UF_HIDDEN != 0
}

/// See the Windows variant.
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(super) fn has_hidden_flag(_meta: &std::fs::Metadata) -> bool {
    false
}
