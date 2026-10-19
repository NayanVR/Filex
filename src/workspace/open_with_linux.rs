//! Linux "Open With": the in-app app list and its launcher, built from the
//! freedesktop.org specs every desktop environment shares rather than from
//! GIO or KService, so no GLib/Qt dependency is pulled in:
//!
//! * the file's MIME type comes from `xdg-mime` (xdg-utils, installed
//!   wherever `xdg-open` is) and its parent types from shared-mime-info's
//!   `mime/subclasses`;
//! * handlers come from the `mimeapps.list` files (MIME Applications spec)
//!   and the `MimeType=` keys of installed `.desktop` entries;
//! * a chosen app is launched by expanding its `Exec=` line (Desktop Entry
//!   spec), without a shell.
//!
//! Everything here reads files or runs `xdg-mime`: call it off the UI
//! thread.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::platform::{AppChoice, sort_choices};

/// Apps registered for `path`'s MIME type (or one of its parent types),
/// the default first — or `None` when the type can't be resolved (no
/// `xdg-mime`), so the caller falls back to the chooser.
pub(super) fn apps_for(path: &Path) -> Option<Vec<AppChoice>> {
    let mime = query_mime_type(path)?;
    Some(apps_for_mime(&XdgDirs::from_env(), &mime))
}

/// Open `file` with the app whose `.desktop` file is `desktop_file` (an
/// [`AppChoice::id`] from [`apps_for`]). Detached — the app owns its own
/// lifetime.
pub(super) fn launch(file: &Path, desktop_file: &Path) -> std::io::Result<()> {
    let text = std::fs::read_to_string(desktop_file)?;
    let entry = DesktopEntry::parse(&text).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a launchable desktop entry",
        )
    })?;
    let args = exec_args(&entry, desktop_file, file).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the app's Exec line can't be parsed",
        )
    })?;
    let Some((program, rest)) = args.split_first() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the app's Exec line is empty",
        ));
    };
    let mut command = std::process::Command::new(program);
    command.args(rest);
    if let Some(dir) = entry.working_dir.as_deref().filter(|d| d.is_dir()) {
        command.current_dir(dir);
    }
    command.spawn().map(drop)
}

/// Assumes `xdg-mime query filetype` prints the type on one line; some
/// backends (`file -i`) append `; charset=…`, which is stripped.
fn query_mime_type(path: &Path) -> Option<String> {
    let out = std::process::Command::new("xdg-mime")
        .args(["query", "filetype"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_mime_output(&String::from_utf8_lossy(&out.stdout))
}

fn parse_mime_output(stdout: &str) -> Option<String> {
    let mime = stdout.lines().next()?.split(';').next()?.trim();
    mime.contains('/').then(|| mime.to_string())
}

/// The XDG base directories, most important first. Read once from the
/// environment; tests point them at fixture trees instead.
struct XdgDirs {
    /// `$XDG_CONFIG_HOME`, then `$XDG_CONFIG_DIRS`.
    config: Vec<PathBuf>,
    /// `$XDG_DATA_HOME`, then `$XDG_DATA_DIRS`.
    data: Vec<PathBuf>,
    /// `$XDG_CURRENT_DESKTOP`, lowercased, for `<desktop>-mimeapps.list`.
    desktops: Vec<String>,
}

impl XdgDirs {
    /// Spec defaults where a variable is unset; relative paths are
    /// ignored, as the Base Directory spec requires.
    fn from_env() -> Self {
        let home = dirs::home_dir();
        let one = |var: &str, fallback: Option<PathBuf>| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .or(fallback)
        };
        let many = |var: &str, fallback: &[&str]| -> Vec<PathBuf> {
            let listed: Vec<PathBuf> = std::env::var_os(var)
                .map(|v| std::env::split_paths(&v).collect())
                .unwrap_or_default();
            let listed: Vec<PathBuf> = listed.into_iter().filter(|p| p.is_absolute()).collect();
            if listed.is_empty() {
                fallback.iter().map(PathBuf::from).collect()
            } else {
                listed
            }
        };
        let config_home = one("XDG_CONFIG_HOME", home.as_ref().map(|h| h.join(".config")));
        let data_home = one(
            "XDG_DATA_HOME",
            home.as_ref().map(|h| h.join(".local/share")),
        );
        let config = config_home
            .into_iter()
            .chain(many("XDG_CONFIG_DIRS", &["/etc/xdg"]))
            .collect();
        let data = data_home
            .into_iter()
            .chain(many("XDG_DATA_DIRS", &["/usr/local/share", "/usr/share"]))
            .collect();
        let desktops = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .split(':')
            .filter(|d| !d.is_empty())
            .map(str::to_lowercase)
            .collect();
        Self {
            config,
            data,
            desktops,
        }
    }

    /// `mimeapps.list` locations in the MIME Applications spec's order:
    /// config dirs, then the (deprecated) `applications/` data dirs, each
    /// desktop-specific file before the generic one.
    fn mimeapps_lists(&self) -> Vec<PathBuf> {
        let config = self.config.iter().cloned();
        let data = self.data.iter().map(|d| d.join("applications"));
        config
            .chain(data)
            .flat_map(|dir| {
                self.desktops
                    .iter()
                    .map(|d| format!("{d}-mimeapps.list"))
                    .chain(Some("mimeapps.list".to_string()))
                    .map(move |name| dir.join(name))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
}

/// Every app registered for `mime` or a parent type, default first, then
/// by name.
fn apps_for_mime(dirs: &XdgDirs, mime: &str) -> Vec<AppChoice> {
    let installed = installed_apps(dirs);
    let lists: Vec<MimeApps> = dirs
        .mimeapps_lists()
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|text| MimeApps::parse(&text))
        .collect();
    let lineage = mime_lineage(mime, &read_subclasses(dirs));
    let (ids, default) = handler_ids(&lineage, &lists, &installed);
    let mut apps: Vec<AppChoice> = ids
        .iter()
        .filter_map(|id| installed.get(id))
        .map(|app| AppChoice {
            name: app.entry.name.clone(),
            id: app.path.clone(),
        })
        .collect();
    let default = default.and_then(|id| installed.get(&id)).map(|a| &a.path);
    sort_choices(&mut apps, default.map(PathBuf::as_path));
    apps
}

/// Desktop ids (`org.gnome.TextEditor.desktop`) handling `lineage`, plus
/// the default's id. Per type, most specific first: `[Added Associations]`
/// in list order, then installed entries' `MimeType=`; a list's `[Removed
/// Associations]` hides an app from itself and every later list and from
/// `MimeType=`. The default is the first installed `[Default
/// Applications]` entry, else the first association.
fn handler_ids(
    lineage: &[String],
    lists: &[MimeApps],
    installed: &HashMap<String, InstalledApp>,
) -> (Vec<String>, Option<String>) {
    let mut ids: Vec<String> = Vec::new();
    let mut explicit_default = None;
    for mime in lineage {
        let mut removed: HashSet<&str> = HashSet::new();
        for list in lists {
            removed.extend(list.removed_for(mime));
            if explicit_default.is_none() {
                explicit_default = list
                    .defaults_for(mime)
                    .find(|id| installed.contains_key(*id))
                    .map(str::to_string);
            }
            for id in list.added_for(mime) {
                if installed.contains_key(id) && !removed.contains(id) {
                    ids.push(id.to_string());
                }
            }
        }
        let mut by_mime_type: Vec<&String> = installed
            .iter()
            .filter(|(id, app)| {
                !removed.contains(id.as_str()) && app.entry.mime_types.iter().any(|m| m == mime)
            })
            .map(|(id, _)| id)
            .collect();
        by_mime_type.sort();
        ids.extend(by_mime_type.into_iter().cloned());
    }
    let default = explicit_default.or_else(|| ids.first().cloned());
    if let Some(id) = &default {
        ids.push(id.clone());
    }
    let mut seen = HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    (ids, default)
}

/// `mime` then its ancestors, nearest first, from shared-mime-info's
/// `subclasses` table. Every `text/*` type is also a `text/plain`.
fn mime_lineage(mime: &str, subclasses: &HashMap<String, Vec<String>>) -> Vec<String> {
    let mut lineage = vec![mime.to_string()];
    let mut next = 0;
    while let Some(current) = lineage.get(next).cloned() {
        for parent in subclasses.get(&current).into_iter().flatten() {
            if !lineage.contains(parent) {
                lineage.push(parent.clone());
            }
        }
        next += 1;
    }
    if mime.starts_with("text/") && !lineage.iter().any(|m| m == "text/plain") {
        lineage.push("text/plain".to_string());
    }
    lineage
}

/// Assumes shared-mime-info's compiled `<data dir>/mime/subclasses`: one
/// `child parent` pair per line.
fn read_subclasses(dirs: &XdgDirs) -> HashMap<String, Vec<String>> {
    let mut table: HashMap<String, Vec<String>> = HashMap::new();
    for dir in &dirs.data {
        let Ok(text) = std::fs::read_to_string(dir.join("mime/subclasses")) else {
            continue;
        };
        for (child, parent) in text.lines().filter_map(|l| l.split_once(' ')) {
            let parents = table.entry(child.to_string()).or_default();
            let parent = parent.trim().to_string();
            if !parents.contains(&parent) {
                parents.push(parent);
            }
        }
    }
    table
}

/// An installed, launchable `.desktop` entry.
struct InstalledApp {
    path: PathBuf,
    entry: DesktopEntry,
}

/// Desktop id → entry for every launchable app under the data dirs'
/// `applications/`. Assumes the Desktop Entry spec's id rule (the path
/// below `applications/`, `/` replaced by `-`) and that an earlier data dir
/// shadows a later one with the same id, even when the earlier entry is
/// `Hidden` (that's how a user deletes a system app).
fn installed_apps(dirs: &XdgDirs) -> HashMap<String, InstalledApp> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut apps = HashMap::new();
    for dir in &dirs.data {
        let root = dir.join("applications");
        for path in desktop_files(&root) {
            let Some(id) = desktop_id(&root, &path) else {
                continue;
            };
            if !seen.insert(id.clone()) {
                continue;
            }
            let entry = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| DesktopEntry::parse(&text))
                .filter(DesktopEntry::is_installed);
            if let Some(entry) = entry {
                apps.insert(id, InstalledApp { path, entry });
            }
        }
    }
    apps
}

/// `.desktop` files under `root`, a few levels deep (`kde4/…`).
fn desktop_files(root: &Path) -> Vec<PathBuf> {
    const MAX_DEPTH: usize = 3;
    let mut files = Vec::new();
    let mut pending = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "desktop") {
                files.push(path);
            } else if depth < MAX_DEPTH && path.is_dir() {
                pending.push((path, depth + 1));
            }
        }
    }
    files.sort();
    files
}

fn desktop_id(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?.to_str()?;
    Some(relative.replace('/', "-"))
}

/// The `[Desktop Entry]` keys "Open With" needs.
#[derive(Debug, Default, PartialEq, Eq)]
struct DesktopEntry {
    name: String,
    exec: String,
    icon: Option<String>,
    mime_types: Vec<String>,
    try_exec: Option<String>,
    working_dir: Option<PathBuf>,
    hidden: bool,
    terminal: bool,
}

impl DesktopEntry {
    /// `None` unless it's a `Type=Application` with `Name` and `Exec`.
    /// Localised keys (`Name[de]`) are skipped; values are unescaped.
    fn parse(text: &str) -> Option<Self> {
        let mut entry = Self::default();
        let mut in_main_group = false;
        let mut is_application = false;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                in_main_group = line == "[Desktop Entry]";
                continue;
            }
            if !in_main_group || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unescape_value(value.trim());
            match key.trim() {
                "Type" => is_application = value == "Application",
                "Name" => entry.name = value,
                "Exec" => entry.exec = value,
                "Icon" => entry.icon = Some(value).filter(|v| !v.is_empty()),
                "MimeType" => {
                    entry.mime_types = value
                        .split(';')
                        .filter(|m| !m.is_empty())
                        .map(str::to_string)
                        .collect()
                }
                "TryExec" => entry.try_exec = Some(value).filter(|v| !v.is_empty()),
                "Path" => entry.working_dir = Some(PathBuf::from(value)),
                "Hidden" => entry.hidden = value == "true",
                "Terminal" => entry.terminal = value == "true",
                _ => {}
            }
        }
        (is_application && !entry.name.is_empty() && !entry.exec.is_empty()).then_some(entry)
    }

    /// Worth listing: not deleted (`Hidden`), its `TryExec` program
    /// present, and not a terminal app — Filex doesn't pick a terminal
    /// emulator to host one, so it would open nothing visible.
    fn is_installed(&self) -> bool {
        !self.hidden && !self.terminal && self.try_exec.as_deref().is_none_or(program_exists)
    }
}

/// `program` is an existing absolute path, or found on `$PATH`.
fn program_exists(program: &str) -> bool {
    let program = Path::new(program);
    if program.is_absolute() {
        return program.is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(program).is_file()))
}

/// Desktop Entry value escapes: `\s` `\n` `\t` `\r` `\\`. Any other
/// backslash is kept for the `Exec` quoting rules to read.
fn unescape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Split an `Exec` value into arguments: whitespace separates, double
/// quotes group, and `\"` `` \` `` `\$` `\\` escape inside them (and any
/// character outside). `None` for an unterminated quote.
fn split_exec(exec: &str) -> Option<Vec<String>> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => match chars.next() {
                Some(e @ ('"' | '`' | '$' | '\\')) => current.push(e),
                Some(other) => {
                    current.push('\\');
                    current.push(other);
                }
                None => return None,
            },
            '\\' => {
                current.push(chars.next()?);
                in_arg = true;
            }
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if in_arg {
        args.push(current);
    }
    Some(args)
}

/// `entry`'s command line for opening `file`: the `Exec` arguments with
/// field codes expanded — `%f`/`%F` to the path, `%u`/`%U` to a `file://`
/// URI, `%i` to `--icon <Icon>`, `%c` to the name, `%k` to the desktop
/// file, `%%` to `%`; deprecated codes dropped. An `Exec` with no file code
/// gets the path appended, as GIO does. Arguments are passed to the program
/// directly, never through a shell, so paths with spaces stay one argument.
fn exec_args(entry: &DesktopEntry, desktop_file: &Path, file: &Path) -> Option<Vec<OsString>> {
    let mut args = Vec::new();
    let mut took_file = false;
    for token in split_exec(&entry.exec)? {
        match token.as_str() {
            "%f" | "%F" => {
                args.push(file.as_os_str().to_owned());
                took_file = true;
            }
            "%u" | "%U" => {
                args.push(file_uri(file).into());
                took_file = true;
            }
            "%i" => {
                if let Some(icon) = &entry.icon {
                    args.push("--icon".into());
                    args.push(icon.into());
                }
            }
            "%d" | "%D" | "%n" | "%N" | "%v" | "%m" => {}
            _ => args.push(expand_inline(
                &token,
                entry,
                desktop_file,
                file,
                &mut took_file,
            )),
        }
    }
    if !took_file {
        args.push(file.as_os_str().to_owned());
    }
    Some(args)
}

/// Field codes embedded in a larger argument (`--file=%f`).
fn expand_inline(
    token: &str,
    entry: &DesktopEntry,
    desktop_file: &Path,
    file: &Path,
    took_file: &mut bool,
) -> OsString {
    let mut out = OsString::new();
    let mut chars = token.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c.encode_utf8(&mut [0; 4]));
            continue;
        }
        match chars.next() {
            Some('%') => out.push("%"),
            Some('f' | 'F') => {
                out.push(file.as_os_str());
                *took_file = true;
            }
            Some('u' | 'U') => {
                out.push(file_uri(file));
                *took_file = true;
            }
            Some('c') => out.push(&entry.name),
            Some('k') => out.push(desktop_file.as_os_str()),
            Some('i') => out.push(entry.icon.as_deref().unwrap_or_default()),
            _ => {}
        }
    }
    out
}

/// `file://` URI for an absolute path, percent-encoding every byte outside
/// RFC 3986's unreserved set (and `/`), so non-UTF-8 names survive.
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// One parsed `mimeapps.list`: MIME type → desktop ids, per group.
#[derive(Debug, Default)]
struct MimeApps {
    defaults: HashMap<String, Vec<String>>,
    added: HashMap<String, Vec<String>>,
    removed: HashMap<String, Vec<String>>,
}

impl MimeApps {
    fn parse(text: &str) -> Self {
        enum Group {
            Defaults,
            Added,
            Removed,
        }
        let mut list = Self::default();
        let mut group = None;
        for line in text.lines().map(str::trim) {
            if line.starts_with('[') {
                group = match line {
                    "[Default Applications]" => Some(Group::Defaults),
                    "[Added Associations]" => Some(Group::Added),
                    "[Removed Associations]" => Some(Group::Removed),
                    _ => None,
                };
                continue;
            }
            if line.starts_with('#') {
                continue;
            }
            let Some((mime, ids)) = line.split_once('=') else {
                continue;
            };
            let target = match group {
                Some(Group::Defaults) => &mut list.defaults,
                Some(Group::Added) => &mut list.added,
                Some(Group::Removed) => &mut list.removed,
                None => continue,
            };
            let ids = ids.split(';').map(str::trim).filter(|id| !id.is_empty());
            target
                .entry(mime.trim().to_string())
                .or_default()
                .extend(ids.map(str::to_string));
        }
        list
    }

    fn defaults_for<'a>(&'a self, mime: &str) -> impl Iterator<Item = &'a str> {
        Self::ids(&self.defaults, mime)
    }

    fn added_for<'a>(&'a self, mime: &str) -> impl Iterator<Item = &'a str> {
        Self::ids(&self.added, mime)
    }

    fn removed_for<'a>(&'a self, mime: &str) -> impl Iterator<Item = &'a str> {
        Self::ids(&self.removed, mime)
    }

    fn ids<'a>(
        group: &'a HashMap<String, Vec<String>>,
        mime: &str,
    ) -> impl Iterator<Item = &'a str> {
        group.get(mime).into_iter().flatten().map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixture XDG tree: `home` shadows `system`, like
    /// `~/.local/share` over `/usr/share`.
    struct Fixture {
        _root: tempfile::TempDir,
        config: PathBuf,
        home: PathBuf,
        system: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let [config, home, system] = ["config", "home", "system"].map(|d| root.path().join(d));
            for dir in [&config, &home, &system] {
                std::fs::create_dir_all(dir.join("applications")).unwrap();
            }
            Self {
                _root: root,
                config,
                home,
                system,
            }
        }

        fn dirs(&self) -> XdgDirs {
            XdgDirs {
                config: vec![self.config.clone()],
                data: vec![self.home.clone(), self.system.clone()],
                desktops: vec!["gnome".into()],
            }
        }

        fn write(&self, path: PathBuf, text: &str) -> PathBuf {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            path
        }

        fn app(&self, data_dir: &Path, id: &str, name: &str, mime: &str) -> PathBuf {
            self.write(
                data_dir.join("applications").join(id),
                &format!(
                    "[Desktop Entry]\nType=Application\nName={name}\nExec={name} %F\nMimeType={mime}\n"
                ),
            )
        }
    }

    fn names(apps: &[AppChoice]) -> Vec<&str> {
        apps.iter().map(|a| a.name.as_str()).collect()
    }

    #[test]
    fn mime_output_drops_charset_and_rejects_garbage() {
        assert_eq!(
            parse_mime_output("text/plain; charset=us-ascii\n").as_deref(),
            Some("text/plain")
        );
        assert_eq!(
            parse_mime_output("image/png\n").as_deref(),
            Some("image/png")
        );
        assert_eq!(parse_mime_output(""), None);
        assert_eq!(parse_mime_output("cannot open file\n"), None);
    }

    #[test]
    fn desktop_entry_reads_main_group_only_and_unescapes() {
        let entry = DesktopEntry::parse(
            "# comment\n\
             [Desktop Entry]\n\
             Type=Application\n\
             Name=Text Editor\n\
             Name[de]=Texteditor\n\
             Exec=gnome-text-editor\\s--new %U\n\
             Icon=org.gnome.TextEditor\n\
             MimeType=text/plain;text/markdown;\n\
             Terminal=false\n\
             [Desktop Action new-window]\n\
             Name=New Window\n\
             Exec=gnome-text-editor --new-window\n",
        )
        .unwrap();
        assert_eq!(entry.name, "Text Editor");
        assert_eq!(entry.exec, "gnome-text-editor --new %U");
        assert_eq!(entry.icon.as_deref(), Some("org.gnome.TextEditor"));
        assert_eq!(entry.mime_types, ["text/plain", "text/markdown"]);
        assert!(entry.is_installed());
    }

    #[test]
    fn non_apps_and_entries_without_exec_are_rejected() {
        assert!(DesktopEntry::parse("[Desktop Entry]\nType=Link\nName=x\nURL=y\n").is_none());
        assert!(DesktopEntry::parse("[Desktop Entry]\nType=Application\nName=x\n").is_none());
    }

    #[test]
    fn hidden_terminal_and_missing_try_exec_apps_are_not_listed() {
        let base = "[Desktop Entry]\nType=Application\nName=x\nExec=x\n";
        let installed = |extra: &str| {
            DesktopEntry::parse(&format!("{base}{extra}"))
                .unwrap()
                .is_installed()
        };
        assert!(!installed("Hidden=true\n"));
        assert!(!installed("Terminal=true\n"));
        assert!(!installed("TryExec=/nonexistent/filex-test-binary\n"));
        assert!(installed("TryExec=/bin/sh\n"));
    }

    #[test]
    fn exec_quoting_and_escapes() {
        assert_eq!(
            split_exec(r#"app --title "two words" "a \"q\" \$x \\ \`" plain\ arg"#).unwrap(),
            [
                "app",
                "--title",
                "two words",
                r#"a "q" $x \ `"#,
                "plain arg"
            ]
        );
        assert_eq!(split_exec("  app   %f  ").unwrap(), ["app", "%f"]);
        assert_eq!(split_exec(r#"app "unterminated"#), None);
    }

    fn entry(exec: &str) -> DesktopEntry {
        DesktopEntry {
            name: "My App".into(),
            exec: exec.into(),
            icon: Some("my-icon".into()),
            ..Default::default()
        }
    }

    fn expand(exec: &str, file: &str) -> Vec<String> {
        exec_args(&entry(exec), Path::new("/apps/my.desktop"), Path::new(file))
            .unwrap()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    /// A path with spaces stays a single argument: nothing goes through
    /// a shell, so no quoting is added either.
    #[test]
    fn file_codes_expand_to_one_argument_for_paths_with_spaces() {
        let file = "/home/me/My Docs/2'x3' Banner.png";
        assert_eq!(expand("viewer %f", file), ["viewer", file]);
        assert_eq!(
            expand("viewer --files %F", file),
            ["viewer", "--files", file]
        );
        assert_eq!(
            expand("viewer %U", file),
            ["viewer", "file:///home/me/My%20Docs/2%27x3%27%20Banner.png"]
        );
        assert_eq!(expand("viewer %u", "/tmp/a"), ["viewer", "file:///tmp/a"]);
    }

    #[test]
    fn other_field_codes() {
        assert_eq!(
            expand("app %i --name=%c --from %k 100%% %d %f", "/f"),
            [
                "app",
                "--icon",
                "my-icon",
                "--name=My App",
                "--from",
                "/apps/my.desktop",
                "100%",
                "/f"
            ]
        );
        assert_eq!(expand("app --open=%f", "/a b"), ["app", "--open=/a b"]);
    }

    #[test]
    fn exec_without_file_code_gets_the_path_appended() {
        assert_eq!(
            expand("app --new-window", "/a b"),
            ["app", "--new-window", "/a b"]
        );
    }

    #[test]
    fn non_utf8_names_are_percent_encoded_in_uris() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9"));
        assert_eq!(file_uri(path), "file:///tmp/caf%E9");
    }

    #[test]
    fn mimeapps_groups_and_comments() {
        let list = MimeApps::parse(
            "[Default Applications]\n\
             text/plain=editor.desktop;other.desktop\n\
             # text/plain=commented.desktop\n\
             [Added Associations]\n\
             text/plain=added.desktop;\n\
             [Unknown Group]\n\
             text/plain=ignored.desktop\n\
             [Removed Associations]\n\
             text/plain = removed.desktop ;\n",
        );
        assert_eq!(
            list.defaults_for("text/plain").collect::<Vec<_>>(),
            ["editor.desktop", "other.desktop"]
        );
        assert_eq!(
            list.added_for("text/plain").collect::<Vec<_>>(),
            ["added.desktop"]
        );
        assert_eq!(
            list.removed_for("text/plain").collect::<Vec<_>>(),
            ["removed.desktop"]
        );
        assert_eq!(list.added_for("image/png").count(), 0);
    }

    #[test]
    fn lineage_walks_subclasses_and_text_falls_back_to_plain() {
        let subclasses = HashMap::from([
            ("text/x-csrc".to_string(), vec!["text/x-c".to_string()]),
            ("text/x-c".to_string(), vec!["text/plain".to_string()]),
            (
                "image/svg+xml".to_string(),
                vec!["application/xml".to_string()],
            ),
        ]);
        assert_eq!(
            mime_lineage("text/x-csrc", &subclasses),
            ["text/x-csrc", "text/x-c", "text/plain"]
        );
        assert_eq!(
            mime_lineage("text/rust", &subclasses),
            ["text/rust", "text/plain"]
        );
        assert_eq!(
            mime_lineage("image/svg+xml", &subclasses),
            ["image/svg+xml", "application/xml"]
        );
    }

    #[test]
    fn default_app_first_then_associations_by_name() {
        let fx = Fixture::new();
        fx.app(&fx.system, "zed.desktop", "Zed", "text/plain;");
        fx.app(&fx.system, "gedit.desktop", "Gedit", "text/plain;");
        fx.app(&fx.system, "viewer.desktop", "Viewer", "image/png;");
        let kate = fx.app(&fx.system, "kde/kate.desktop", "Kate", "text/markdown;");
        fx.write(
            fx.config.join("mimeapps.list"),
            "[Default Applications]\ntext/plain=missing.desktop;zed.desktop\n",
        );
        fx.write(
            fx.system.join("mime/subclasses"),
            "text/markdown text/plain\n",
        );
        let apps = apps_for_mime(&fx.dirs(), "text/markdown");
        assert_eq!(names(&apps), ["Zed", "Gedit", "Kate"]);
        assert_eq!(apps[2].id, kate);
    }

    #[test]
    fn without_explicit_default_the_first_association_leads() {
        let fx = Fixture::new();
        fx.app(&fx.system, "a.desktop", "Alpha", "image/png;");
        fx.app(&fx.system, "z.desktop", "Zulu", "image/png;");
        fx.write(
            fx.config.join("gnome-mimeapps.list"),
            "[Added Associations]\nimage/png=z.desktop;\n",
        );
        assert_eq!(
            names(&apps_for_mime(&fx.dirs(), "image/png")),
            ["Zulu", "Alpha"]
        );
    }

    #[test]
    fn user_entries_shadow_system_ones_and_hidden_deletes() {
        let fx = Fixture::new();
        fx.app(&fx.system, "editor.desktop", "System Editor", "text/plain;");
        let user = fx.app(&fx.home, "editor.desktop", "My Editor", "text/plain;");
        fx.app(&fx.system, "gone.desktop", "Gone", "text/plain;");
        fx.write(
            fx.home.join("applications/gone.desktop"),
            "[Desktop Entry]\nType=Application\nName=Gone\nExec=gone\nHidden=true\n",
        );
        let apps = apps_for_mime(&fx.dirs(), "text/plain");
        assert_eq!(names(&apps), ["My Editor"]);
        assert_eq!(apps[0].id, user);
    }

    #[test]
    fn removed_associations_hide_mime_type_matches() {
        let fx = Fixture::new();
        fx.app(&fx.system, "keep.desktop", "Keep", "image/png;");
        fx.app(&fx.system, "drop.desktop", "Drop", "image/png;");
        fx.write(
            fx.config.join("mimeapps.list"),
            "[Removed Associations]\nimage/png=drop.desktop;\n",
        );
        assert_eq!(names(&apps_for_mime(&fx.dirs(), "image/png")), ["Keep"]);
    }

    #[test]
    fn unknown_type_lists_nothing() {
        let fx = Fixture::new();
        fx.app(&fx.system, "viewer.desktop", "Viewer", "image/png;");
        assert!(apps_for_mime(&fx.dirs(), "application/x-filex-test").is_empty());
    }

    #[test]
    fn mimeapps_lists_follow_spec_order() {
        let dirs = XdgDirs {
            config: vec!["/c".into()],
            data: vec!["/d".into()],
            desktops: vec!["kde".into()],
        };
        assert_eq!(
            dirs.mimeapps_lists(),
            [
                PathBuf::from("/c/kde-mimeapps.list"),
                "/c/mimeapps.list".into(),
                "/d/applications/kde-mimeapps.list".into(),
                "/d/applications/mimeapps.list".into(),
            ]
        );
    }

    #[test]
    fn launch_rejects_a_broken_desktop_file() {
        let fx = Fixture::new();
        let bad = fx.write(
            fx.system.join("applications/bad.desktop"),
            "[Desktop Entry]\nType=Application\nName=Bad\nExec=app \"oops\n",
        );
        let err = launch(Path::new("/tmp/x"), &bad).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
