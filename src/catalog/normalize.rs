use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

pub const VERSION: u32 = 1;

/// Normalization version 1, pinned by the FXSEG002 segment format.
/// Full, non-Turkic folding with canonical composition before and after folding.
pub fn nfc_fold(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }
    name.nfc().case_fold().nfc().collect()
}

pub fn nfkc_fold(name: &str) -> String {
    if name.is_ascii() {
        return name.to_ascii_lowercase();
    }
    name.nfkc().case_fold().nfkc().collect()
}

/// Byte positions at filename/component starts, including camel-case and acronym
/// transitions. Map each component separately when normalization changes length.
pub fn boundaries(name: &str) -> Vec<usize> {
    let chars: Vec<_> = name.char_indices().collect();
    chars
        .iter()
        .enumerate()
        .filter_map(|(i, &(offset, c))| {
            let previous = i.checked_sub(1).map(|j| chars[j].1);
            let next = chars.get(i + 1).map(|(_, c)| *c);
            (i == 0
                || (c.is_alphanumeric()
                    && previous.is_some_and(|p| {
                        !p.is_alphanumeric()
                            || (p.is_lowercase() && c.is_uppercase())
                            || (p.is_uppercase()
                                && c.is_uppercase()
                                && next.is_some_and(char::is_lowercase))
                    })))
            .then_some(offset)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_casefold_and_boundaries() {
        assert_eq!(nfc_fold("Straße"), nfc_fold("STRASSE"));
        assert_eq!(nfc_fold("École"), nfc_fold("E\u{301}cole"));
        assert_eq!(nfc_fold("Σςσ"), "σσσ");
        assert_eq!(nfc_fold("İ"), "i\u{307}");
        assert_ne!(nfc_fold("①"), nfc_fold("1"));
        assert_eq!(nfkc_fold("①"), "1");
        assert_eq!(boundaries("XMLHttpRequest-test.rs"), vec![0, 3, 7, 15, 20]);
    }
}
