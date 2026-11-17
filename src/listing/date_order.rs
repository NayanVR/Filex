//! Whether the user's locale writes the day before the month ("1 Nov
//! 2026") or after it ("Nov 1, 2026"), so Filex's dates read the way the
//! OS's own panels (Finder, Quick Look, Explorer) write them.

use std::sync::OnceLock;

/// Day before month in the user's locale. Asked once, then cached.
pub fn day_first() -> bool {
    static DAY_FIRST: OnceLock<bool> = OnceLock::new();
    *DAY_FIRST.get_or_init(platform::day_first)
}

/// Whether a date pattern such as `"d MMM"` / `"MMM d"` puts the day
/// first. Patterns without both fields read as month-first.
// Foundation's answer on macOS; the others are tested but unused there.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn pattern_day_first(pattern: &str) -> bool {
    matches!((pattern.find('d'), pattern.find('M')), (Some(d), Some(m)) if d < m)
}

/// Whether a POSIX locale name (`en_IN.UTF-8`, `de_DE@euro`) belongs to a
/// region that writes month before day. Only a handful do; no region
/// (`C`, `POSIX`, empty) reads as month-first, the old default.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn locale_day_first(locale: &str) -> bool {
    let region = locale
        .split(['.', '@'])
        .next()
        .and_then(|name| name.split_once('_'))
        .map(|(_, region)| region);
    match region {
        None | Some("") => false,
        Some(region) => !matches!(region, "US" | "PH" | "CA" | "FM" | "MH" | "PW" | "BZ"),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    /// Assumes Foundation's `dateFormatFromTemplate` reorders the `MMMd`
    /// skeleton for the current locale, the source Finder and Quick Look
    /// format from. In-memory: no file or process I/O.
    pub fn day_first() -> bool {
        use objc2_foundation::{NSDateFormatter, NSLocale, NSString};
        let locale = NSLocale::currentLocale();
        NSDateFormatter::dateFormatFromTemplate_options_locale(
            &NSString::from_str("MMMd"),
            0,
            Some(&locale),
        )
        .is_some_and(|pattern| super::pattern_day_first(&pattern.to_string()))
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    /// Linux: the POSIX locale variables, most specific first.
    // ponytail: Windows sets none of these, so it stays month-first; read
    // GetLocaleInfoEx(LOCALE_SSHORTDATE) there when a Windows user asks.
    pub fn day_first() -> bool {
        ["LC_ALL", "LC_TIME", "LANG"]
            .iter()
            .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
            .is_some_and(|locale| super::locale_day_first(&locale))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_and_locales_give_the_order() {
        assert!(pattern_day_first("d MMM"));
        assert!(!pattern_day_first("MMM d"));
        assert!(!pattern_day_first("yyyy"));
        assert!(locale_day_first("en_IN.UTF-8"));
        assert!(locale_day_first("de_DE@euro"));
        assert!(!locale_day_first("en_US.UTF-8"));
        assert!(!locale_day_first("C"));
        assert!(!locale_day_first(""));
    }
}
