//! Error tracking: crash reports and logged errors as PostHog `$exception`
//! events. Pure and SDK-free.
//!
//! The frame/exception shape matches PostHog's official Rust SDK
//! (`posthog` 1.7, `error_tracking.rs`): `$exception_list` entries carry
//! `type`, `value`, `mechanism {type, handled, synthetic}` and a `raw`
//! stacktrace whose frames are `{function, filename, lineno, lang: "rust",
//! platform: "native", in_app, synthetic, client_resolved}`, ordered
//! outermost-first (the crashing frame is last).
//!
//! Privacy: every string here passes [`scrub_text`], which is stricter
//! than [`telemetry::scrub`]. It also drops quoted strings, bare file names,
//! relative paths, emails and URLs, because a logged error such as
//! `failed to rename "taxes-2025.pdf"` holds no absolute path for the base
//! scrubber to catch.

use std::hash::{Hash, Hasher};

use serde_json::{Map, Value, json};

use crate::diagnostics::telemetry::{self, CrashReport};

/// Most frames kept per exception; deep async stacks add little.
const MAX_FRAMES: usize = 64;
/// Longest exception message kept, in chars.
const MAX_VALUE_CHARS: usize = 1_000;

/// [`telemetry::scrub`] plus redaction of everything else that can name a
/// user's file or identify them. Over-redaction is the accepted trade.
pub fn scrub_text(text: &str) -> String {
    let base = telemetry::scrub(text);
    let unquoted = redact_quoted(&base);
    unquoted
        .lines()
        .map(|line| {
            line.split(' ')
                .map(redact_token)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace the contents of `"…"`, `` `…` `` and `'…'` with `…`. A `'` only
/// opens a quote at a word boundary, so "couldn't" survives.
fn redact_quoted(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let opens = match c {
            '"' | '`' => true,
            '\'' => i == 0 || !chars[i - 1].is_alphanumeric(),
            _ => false,
        };
        if opens && let Some(len) = chars[i + 1..].iter().position(|&d| d == c) {
            out.push(c);
            out.push('…');
            out.push(c);
            i += len + 2;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Redact one space-delimited token if it looks like a file name, a
/// relative path, an email or a URL. Rust symbols (`a::b`) and numbers
/// (`1.5`, `0.5.0`) are kept.
fn redact_token(token: &str) -> &str {
    let t = token.trim_matches(|c: char| "()[]{},;:.!?<>".contains(c));
    if t.is_empty() || t == "<path>" || t == "…" {
        return token;
    }
    if t.contains('@') && t.contains('.') {
        return "<email>";
    }
    if t.contains("://") {
        return "<url>";
    }
    if t.contains('/') || t.contains('\\') {
        return "<path>";
    }
    if t.contains("::") {
        return token;
    }
    if looks_like_file_name(t) {
        return "<name>";
    }
    token
}

/// `stem.ext` with a short alphanumeric extension and a stem that isn't
/// just a number (so versions and decimals survive).
fn looks_like_file_name(t: &str) -> bool {
    let Some((stem, ext)) = t.rsplit_once('.') else {
        return false;
    };
    !stem.is_empty()
        && !stem.chars().all(|c| c.is_ascii_digit() || c == '.')
        && (1..=8).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

/// One stack frame on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub function: String,
    pub filename: Option<String>,
    pub lineno: Option<u32>,
}

impl Frame {
    fn in_app(&self) -> bool {
        self.function.starts_with("filex::") || self.function.starts_with("filex_")
    }

    fn to_json(&self) -> Value {
        let mut frame = json!({
            "function": self.function,
            "lang": "rust",
            "platform": "native",
            "in_app": self.in_app(),
            "synthetic": false,
            "client_resolved": true,
        });
        if let Some(filename) = &self.filename {
            frame["filename"] = filename.clone().into();
        }
        if let Some(lineno) = self.lineno {
            frame["lineno"] = lineno.into();
        }
        frame
    }
}

/// Parse a `std::backtrace::Backtrace` rendering (already scrubbed) into
/// frames, outermost first. Panic and capture machinery at the top is
/// dropped so the crashing frame really is last.
///
/// The format is `N: symbol` followed by an optional `at file:line:col`
/// line. Scrubbed absolute paths arrive as `<path>` and are left off.
pub fn parse_backtrace(text: &str) -> Vec<Frame> {
    let mut frames: Vec<Frame> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(location) = trimmed.strip_prefix("at ") {
            if let Some(frame) = frames.last_mut() {
                let (file, line) = split_location(location);
                frame.filename = file;
                frame.lineno = line;
            }
            continue;
        }
        let Some((index, symbol)) = trimmed.split_once(": ") else {
            continue;
        };
        if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        frames.push(Frame {
            function: normalize_function(symbol),
            filename: None,
            lineno: None,
        });
    }
    // Rendering is innermost-first; drop the machinery that captured it.
    let skip = frames
        .iter()
        .position(|f| !is_machinery(&f.function))
        .unwrap_or(frames.len());
    let mut frames: Vec<Frame> = frames.into_iter().skip(skip).collect();
    frames.truncate(MAX_FRAMES);
    frames.reverse();
    frames
}

/// The panic/backtrace plumbing above the real crash site.
fn is_machinery(function: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "std::backtrace",
        "std::panicking",
        "std::panic",
        "core::panicking",
        "core::panic",
        "rust_begin_unwind",
        "__rustc",
        "<alloc::boxed::Box<F,A> as core::ops::function::Fn",
        "filex::diagnostics::telemetry",
    ];
    PREFIXES.iter().any(|p| function.starts_with(p))
}

/// Strip the legacy-mangling hash suffix (`::h0123456789abcdef`) so
/// grouping is stable across builds.
fn normalize_function(symbol: &str) -> String {
    match symbol.rsplit_once("::") {
        Some((prefix, hash))
            if hash.len() == 17
                && hash.starts_with('h')
                && hash[1..].chars().all(|c| c.is_ascii_hexdigit()) =>
        {
            prefix.to_string()
        }
        _ => symbol.to_string(),
    }
}

/// `file:line:col` → (file, line). A scrubbed `<path>` keeps no file.
fn split_location(location: &str) -> (Option<String>, Option<u32>) {
    let mut parts = location.rsplitn(3, ':');
    let (col, line, file) = (parts.next(), parts.next(), parts.next());
    let (file, line) = match (file, line, col) {
        (Some(file), Some(line), Some(_)) => (file, line.parse().ok()),
        _ => (location, None),
    };
    let file = (!file.contains("<path>") && !file.is_empty()).then(|| short_source_path(file));
    (file, line)
}

/// Keep a source path from `src/` on (`./src/ops.rs` → `src/ops.rs`),
/// which is all grouping needs and holds no machine detail.
fn short_source_path(file: &str) -> String {
    let file = file.replace('\\', "/");
    match file.rfind("src/") {
        Some(i) => file[i..].to_string(),
        None => file.rsplit('/').next().unwrap_or_default().to_string(),
    }
}

/// `$exception` properties for a queued crash. Fatal, unhandled, and
/// stamped with the crashing build's version, not the uploader's.
pub fn crash_props(report: &CrashReport) -> Map<String, Value> {
    let frames = parse_backtrace(&report.backtrace);
    let value = truncate(&scrub_text(&report.message));
    let mut props = exception_props(
        "Panic",
        &value,
        "panic",
        false,
        "fatal",
        &frames,
        fingerprint(&[
            "panic",
            report.location.as_deref().unwrap_or(""),
            &top_in_app(&frames),
        ]),
    );
    props.insert("$app_version".into(), report.version.clone().into());
    props.insert("process".into(), report.app.clone().into());
    props.insert("thread".into(), scrub_text(&report.thread).into());
    if let Some(location) = &report.location {
        props.insert("panic_location".into(), scrub_text(location).into());
    }
    props
}

/// `$exception` properties for an `error!` log event. `target` is the
/// module path and `file`/`line` the callsite, all compile-time constants,
/// which is what groups the issue: the message varies per occurrence.
pub fn log_error_props(
    target: &str,
    file: Option<&str>,
    line: Option<u32>,
    message: &str,
) -> Map<String, Value> {
    let frames = vec![Frame {
        function: target.to_string(),
        filename: file.map(short_source_path),
        lineno: line,
    }];
    let callsite = format!("{}:{}", file.unwrap_or(target), line.unwrap_or(0));
    let mut props = exception_props(
        "LoggedError",
        &truncate(&scrub_text(message)),
        "log",
        true,
        "error",
        &frames,
        fingerprint(&["log", target, &callsite]),
    );
    props.insert("log_target".into(), target.to_string().into());
    props
}

fn exception_props(
    ty: &str,
    value: &str,
    mechanism: &str,
    handled: bool,
    level: &str,
    frames: &[Frame],
    fingerprint: String,
) -> Map<String, Value> {
    let mut item = json!({
        "type": ty,
        "value": value,
        "mechanism": { "type": mechanism, "handled": handled, "synthetic": false },
    });
    if !frames.is_empty() {
        item["stacktrace"] = json!({
            "type": "raw",
            "frames": frames.iter().map(Frame::to_json).collect::<Vec<_>>(),
        });
    }
    let mut props = Map::new();
    props.insert("$exception_list".into(), json!([item]));
    props.insert("$exception_level".into(), level.into());
    props.insert("$exception_fingerprint".into(), fingerprint.into());
    props
}

/// The innermost in-app function, the best single grouping signal for a
/// panic whose message embeds runtime values.
fn top_in_app(frames: &[Frame]) -> String {
    frames
        .iter()
        .rev()
        .find(|f| f.in_app())
        .map(|f| f.function.clone())
        .unwrap_or_default()
}

/// A stable hex fingerprint over compile-time facts (never the message).
/// FNV-1a rather than `DefaultHasher`, whose output may change between
/// Rust releases and would split every issue on a toolchain bump.
fn fingerprint(parts: &[&str]) -> String {
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }
        fn write(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.0 ^= u64::from(*b);
                self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    let mut hasher = Fnv(0xcbf2_9ce4_8422_2325);
    parts.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn truncate(text: &str) -> String {
    match text.char_indices().nth(MAX_VALUE_CHARS) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_text_drops_names_quotes_emails_and_urls() {
        assert_eq!(
            scrub_text(r#"failed to rename "taxes 2025.pdf": permission denied"#),
            r#"failed to rename "…": permission denied"#
        );
        assert_eq!(
            scrub_text("could not open report.docx"),
            "could not open <name>"
        );
        assert_eq!(scrub_text("open Documents/plan.txt"), "open <path>");
        assert_eq!(scrub_text("mail bob@example.com now"), "mail <email> now");
        assert_eq!(scrub_text("GET https://x.y/z failed"), "GET <url> failed");
        assert_eq!(scrub_text("'secret' was locked"), "'…' was locked");
        assert_eq!(scrub_text("/home/bob/a/b missing"), "<path> missing");
    }

    #[test]
    fn scrub_text_keeps_diagnostic_words_symbols_and_numbers() {
        let msg = "couldn't read index: filex::catalog::Wal epoch 12 v0.5.0 took 1.5s (os error 2)";
        assert_eq!(scrub_text(msg), msg);
    }

    const BACKTRACE: &str = "   0: std::backtrace::Backtrace::force_capture
             at <path>
   1: filex::diagnostics::telemetry::capture_panic
   2: std::panicking::rust_panic_with_hook
   3: filex::ops::apply::h0123456789abcdef
             at ./src/ops.rs:120:9
   4: filex::workspace::paste
             at D:\\a\\Filex\\src\\workspace\\file_ops.rs:40:5
   5: main";

    #[test]
    fn backtrace_parses_outermost_first_without_machinery() {
        let frames = parse_backtrace(BACKTRACE);
        let names: Vec<&str> = frames.iter().map(|f| f.function.as_str()).collect();
        assert_eq!(
            names,
            ["main", "filex::workspace::paste", "filex::ops::apply"]
        );
        let crash = frames.last().unwrap();
        assert_eq!(crash.filename.as_deref(), Some("src/ops.rs"));
        assert_eq!(crash.lineno, Some(120));
        assert_eq!(
            frames[1].filename.as_deref(),
            Some("src/workspace/file_ops.rs")
        );
    }

    fn report() -> CrashReport {
        CrashReport {
            schema: telemetry::SCHEMA,
            app: "filex".into(),
            version: "0.4.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            unix_millis: 0,
            thread: "main".into(),
            message: "index out of bounds: the len is 3 but the index is 7".into(),
            location: Some("src/ops.rs:120".into()),
            backtrace: BACKTRACE.into(),
        }
    }

    #[test]
    fn crash_props_follow_posthog_exception_shape() {
        let props = crash_props(&report());
        assert_eq!(props["$exception_level"], "fatal");
        assert_eq!(props["$app_version"], "0.4.0");
        let item = &props["$exception_list"][0];
        assert_eq!(item["type"], "Panic");
        assert_eq!(item["mechanism"]["type"], "panic");
        assert_eq!(item["mechanism"]["handled"], false);
        assert_eq!(item["stacktrace"]["type"], "raw");
        let last = item["stacktrace"]["frames"]
            .as_array()
            .unwrap()
            .last()
            .unwrap();
        assert_eq!(last["function"], "filex::ops::apply");
        assert_eq!(last["lang"], "rust");
        assert_eq!(last["platform"], "native");
        assert_eq!(last["in_app"], true);
        assert_eq!(last["lineno"], 120);
    }

    #[test]
    fn crash_fingerprint_ignores_the_message() {
        let a = crash_props(&report());
        let mut other = report();
        other.message = "index out of bounds: the len is 9 but the index is 11".into();
        let b = crash_props(&other);
        assert_eq!(a["$exception_fingerprint"], b["$exception_fingerprint"]);
    }

    #[test]
    fn log_error_props_group_by_callsite_and_scrub_the_message() {
        let a = log_error_props(
            "filex::ops",
            Some("src/ops.rs"),
            Some(77),
            "move failed for \"holiday.jpg\"",
        );
        let b = log_error_props(
            "filex::ops",
            Some("src/ops.rs"),
            Some(77),
            "move failed for x",
        );
        assert_eq!(a["$exception_fingerprint"], b["$exception_fingerprint"]);
        assert_eq!(a["$exception_level"], "error");
        let item = &a["$exception_list"][0];
        assert_eq!(item["value"], "move failed for \"…\"");
        assert_eq!(item["mechanism"]["handled"], true);
        assert_eq!(item["stacktrace"]["frames"][0]["filename"], "src/ops.rs");
    }

    #[test]
    fn long_messages_are_truncated() {
        let long = "x".repeat(MAX_VALUE_CHARS + 50);
        let props = log_error_props("t", None, None, &long);
        let value = props["$exception_list"][0]["value"].as_str().unwrap();
        assert_eq!(value.chars().count(), MAX_VALUE_CHARS + 1);
    }
}
