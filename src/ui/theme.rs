//! Shared color palette (dark). Placeholder until a real theme system
//! lands — every component styles itself from these constants so a
//! future theme swap touches exactly one file.

/// Window background.
pub const BG: u32 = 0x1e2227;
/// Panel background (top bar, sidebar, status bar).
pub const BG_PANEL: u32 = 0x23272e;
/// Hover background for interactive rows/buttons.
pub const BG_HOVER: u32 = 0x2f343c;
/// Background of the selected list row.
pub const BG_SELECTED: u32 = 0x37404d;
/// Panel / control borders.
pub const BORDER: u32 = 0x363c45;
/// Primary text.
pub const TEXT: u32 = 0xd7dae0;
/// Secondary text (paths, sizes, placeholders, section headers).
pub const TEXT_DIM: u32 = 0x8b929e;
/// Accent (directories, ready markers, cursor, focused borders).
pub const ACCENT: u32 = 0x5ac8fa;
/// [`ACCENT`] at ~25% alpha — text selection highlight.
pub const ACCENT_SELECTION: u32 = 0x5ac8fa40;
/// Warnings (failed roots, missing permissions).
pub const WARN: u32 = 0xe5c07b;
