//! Command-line parsing for the UI binary.
//!
//! Once Filex is the default file explorer, the OS (and every app that
//! used to run `explorer.exe <args>`) hands Filex Explorer's own argument
//! syntax, so this accepts it alongside plain paths: `/select,<path>`,
//! `/e,`, `/root,<path>`, `/n`, `shell:<name>` and `::{CLSID}`. Anything
//! Filex cannot show is reported as [`Browse::Foreign`] so the caller can
//! pass it through to the system file manager unchanged.
//!
//! Pure logic: no GPUI, no I/O beyond `is_dir` checks done by the caller.

use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What one launch of `filex` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Run (or reuse) the browsing UI.
    Browse(Browse),
    /// Run a one-shot picker window; the request arrives on stdin and the
    /// answer leaves on stdout (see [`crate::picker`]).
    Pick,
    /// Change the OS integration and exit without a window.
    Shell(ShellCommand),
}

/// Where a browsing launch should land. Serializable because it is also
/// the message forwarded to an already-running instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Browse {
    /// No target: start normally, or just bring the running window up.
    Default,
    /// Open this folder.
    Open(PathBuf),
    /// Open the folder containing this item, with the item selected.
    Select(PathBuf),
    /// The drives overview ("This PC").
    ThisPc,
    /// Something only the system file manager can show (Control Panel,
    /// Recycle Bin, an unknown `shell:` name). Holds the original args.
    Foreign(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellCommand {
    RegisterDefault,
    UnregisterDefault,
    RegisterDialogs,
    UnregisterDialogs,
    /// Undo everything (the uninstaller's hook).
    UnregisterAll,
}

impl Browse {
    /// `Open` of something that is not a folder becomes `Select` (Explorer
    /// opens a file's folder with it selected). Touches the file system, so
    /// it runs before the UI exists or on a background thread — never on
    /// the UI thread.
    pub fn normalize(self) -> Browse {
        match self {
            Browse::Open(path) if path.exists() && !path.is_dir() => Browse::Select(path),
            other => other,
        }
    }
}

/// This PC's shell CLSID, used by Win+E and `explorer.exe ::{…}`.
const THIS_PC_CLSID: &str = "{20d04fe0-3aea-1069-a2d8-08002b30309d}";

/// Parse the arguments after the program name. `cwd` resolves relative
/// paths; `known_folder` maps `shell:` names like `Downloads` to paths.
pub fn parse(
    args: &[OsString],
    cwd: &Path,
    known_folder: impl Fn(&str) -> Option<PathBuf>,
) -> Launch {
    let strings: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Some(command) = strings.first().and_then(|a| own_flag(a)) {
        return command;
    }
    Launch::Browse(parse_browse(&strings, cwd, &known_folder))
}

/// Filex's own `--` flags (only valid as the first argument).
fn own_flag(arg: &str) -> Option<Launch> {
    let shell = |c| Some(Launch::Shell(c));
    match arg {
        "--pick" => Some(Launch::Pick),
        "--register-default" => shell(ShellCommand::RegisterDefault),
        "--unregister-default" => shell(ShellCommand::UnregisterDefault),
        "--register-dialogs" => shell(ShellCommand::RegisterDialogs),
        "--unregister-dialogs" => shell(ShellCommand::UnregisterDialogs),
        "--unregister-shell" => shell(ShellCommand::UnregisterAll),
        _ => None,
    }
}

fn parse_browse(
    args: &[String],
    cwd: &Path,
    known_folder: &impl Fn(&str) -> Option<PathBuf>,
) -> Browse {
    let mut select = false;
    let mut target: Option<String> = None;
    for arg in args {
        let (switches, rest) = explorer_switches(arg);
        select |= switches.select;
        if let Some(rest) = rest.filter(|r| !r.is_empty()) {
            target = Some(rest);
        }
    }
    let Some(target) = target else {
        return Browse::Default;
    };
    match shell_target(&target, known_folder) {
        Some(ShellTarget::ThisPc) => Browse::ThisPc,
        Some(ShellTarget::Path(path)) => Browse::Open(path),
        Some(ShellTarget::Unknown) => Browse::Foreign(args.to_vec()),
        None => {
            let path = resolve(&target, cwd);
            if select {
                Browse::Select(path)
            } else {
                Browse::Open(path)
            }
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Switches {
    select: bool,
}

/// Split one argument into Explorer switches and the trailing path.
/// Explorer joins switches with commas (`/e,/select,C:\a,b.txt`): every
/// leading `/switch` token is consumed, and whatever follows the last one
/// — commas included, since paths may contain them — is the target. An
/// argument that does not start with `/` is a plain path.
fn explorer_switches(arg: &str) -> (Switches, Option<String>) {
    let mut switches = Switches::default();
    let trimmed = arg.trim();
    if !trimmed.starts_with('/') || looks_like_unix_path(trimmed) {
        return (switches, Some(unquote(trimmed)));
    }
    let mut rest = trimmed;
    while let Some(after_slash) = rest.strip_prefix('/') {
        let (name, tail) = match after_slash.split_once(',') {
            Some((name, tail)) => (name, Some(tail)),
            None => (after_slash, None),
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "select" => switches.select = true,
            "e" | "n" | "root" | "separate" | "idlist" => {}
            // Not a switch we know: treat the whole remainder as the path.
            _ => return (switches, Some(unquote(rest))),
        }
        match tail {
            Some(tail) => rest = tail.trim_start(),
            None => return (switches, None),
        }
    }
    (switches, Some(unquote(rest)))
}

/// `/home/me` is a path on Unix, not an Explorer switch. Explorer
/// switches are single words followed by a comma or the end.
fn looks_like_unix_path(arg: &str) -> bool {
    cfg!(unix) && arg[1..].contains('/')
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s);
    // The shell runs `filex.exe "%1"`, so a drive arrives as `"D:\"`, and
    // Windows' argument rules read `\"` as an escaped quote: the program
    // sees `D:"`. No Windows path can contain `"`, so a trailing one is
    // always that lost backslash.
    match s.strip_suffix('"') {
        Some(head) if cfg!(windows) => format!("{head}\\"),
        _ => s.to_string(),
    }
}

enum ShellTarget {
    ThisPc,
    Path(PathBuf),
    Unknown,
}

/// Recognize `shell:Name`, `shell:::{CLSID}` and `::{CLSID}` targets.
/// `None` means the target is an ordinary path.
fn shell_target(
    target: &str,
    known_folder: &impl Fn(&str) -> Option<PathBuf>,
) -> Option<ShellTarget> {
    let lower = target.to_ascii_lowercase();
    let clsid = lower
        .strip_prefix("shell:::")
        .or_else(|| lower.strip_prefix("::"));
    if let Some(clsid) = clsid {
        return Some(if clsid == THIS_PC_CLSID {
            ShellTarget::ThisPc
        } else {
            ShellTarget::Unknown
        });
    }
    let name = lower.strip_prefix("shell:")?;
    if matches!(name, "mycomputerfolder" | "this pc" | "drivefolder") {
        return Some(ShellTarget::ThisPc);
    }
    Some(known_folder(name).map_or(ShellTarget::Unknown, ShellTarget::Path))
}

fn resolve(target: &str, cwd: &Path) -> PathBuf {
    let path = PathBuf::from(target);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

/// The user folders `shell:` names map to, from the OS's own records.
/// `name` is lowercase.
pub fn known_folder(name: &str) -> Option<PathBuf> {
    match name {
        "desktop" => dirs::desktop_dir(),
        "downloads" => dirs::download_dir(),
        "personal" | "documents" => dirs::document_dir(),
        "my pictures" | "pictures" => dirs::picture_dir(),
        "my music" | "music" => dirs::audio_dir(),
        "my video" | "videos" => dirs::video_dir(),
        "profile" | "userprofile" => dirs::home_dir(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\work")
        } else {
            PathBuf::from("/work")
        }
    }

    fn abs(p: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{p}"))
        } else {
            PathBuf::from(format!("/{}", p.replace('\\', "/")))
        }
    }

    fn browse(args: &[&str]) -> Browse {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let folders = |name: &str| (name == "downloads").then(|| abs("Downloads"));
        match parse(&args, &cwd(), folders) {
            Launch::Browse(b) => b,
            other => panic!("expected browse, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_is_a_plain_start() {
        assert_eq!(browse(&[]), Browse::Default);
    }

    #[test]
    fn a_plain_absolute_path_opens_it() {
        let p = abs("Users");
        assert_eq!(browse(&[p.to_str().unwrap()]), Browse::Open(p.clone()));
    }

    #[test]
    fn relative_paths_resolve_against_cwd() {
        assert_eq!(browse(&["sub dir"]), Browse::Open(cwd().join("sub dir")));
    }

    #[cfg(windows)]
    #[test]
    fn explorer_select_with_comma_in_the_file_name() {
        assert_eq!(
            browse(&[r"/select,C:\a\b, c.txt"]),
            Browse::Select(PathBuf::from(r"C:\a\b, c.txt"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn explorer_switches_split_across_arguments() {
        // `explorer.exe /select, "C:\a b\c.txt"` arrives as two args.
        assert_eq!(
            browse(&["/select,", r"C:\a b\c.txt"]),
            Browse::Select(PathBuf::from(r"C:\a b\c.txt"))
        );
    }

    /// Regression: opening `D:\` from the shell (`filex.exe "%1"`) arrived
    /// as `D:"` and failed with "volume label syntax is incorrect".
    #[cfg(windows)]
    #[test]
    fn drive_roots_survive_the_escaped_quote() {
        assert_eq!(browse(&[r#"D:""#]), Browse::Open(PathBuf::from(r"D:\")));
        assert_eq!(
            browse(&[r#"C:\Program Files\""#]),
            Browse::Open(PathBuf::from(r"C:\Program Files\"))
        );
        assert_eq!(
            browse(&[r#"/select,D:""#]),
            Browse::Select(PathBuf::from(r"D:\"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn stacked_switches_and_quoted_root() {
        assert_eq!(
            browse(&[r#"/e,/root,"C:\x y""#]),
            Browse::Open(PathBuf::from(r"C:\x y"))
        );
        assert_eq!(browse(&["/n"]), Browse::Default);
    }

    #[test]
    fn this_pc_by_clsid_and_by_name() {
        assert_eq!(
            browse(&["::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"]),
            Browse::ThisPc
        );
        assert_eq!(
            browse(&["shell:::{20D04FE0-3AEA-1069-A2D8-08002B30309D}"]),
            Browse::ThisPc
        );
        assert_eq!(browse(&["shell:MyComputerFolder"]), Browse::ThisPc);
    }

    #[test]
    fn known_shell_folders_resolve() {
        assert_eq!(browse(&["shell:Downloads"]), Browse::Open(abs("Downloads")));
    }

    #[test]
    fn unknown_shell_targets_pass_through_untouched() {
        let control_panel = "::{26EE0668-A00A-44D7-9371-BEB064C98683}";
        assert_eq!(
            browse(&[control_panel]),
            Browse::Foreign(vec![control_panel.to_string()])
        );
        assert_eq!(
            browse(&["shell:RecycleBinFolder"]),
            Browse::Foreign(vec!["shell:RecycleBinFolder".to_string()])
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_absolute_paths_are_not_switches() {
        assert_eq!(
            browse(&["/home/me"]),
            Browse::Open(PathBuf::from("/home/me"))
        );
        assert_eq!(browse(&["/tmp"]), Browse::Open(PathBuf::from("/tmp")));
    }

    #[test]
    fn opening_a_file_selects_it_in_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(Browse::Open(file.clone()).normalize(), Browse::Select(file));
        let folder = Browse::Open(dir.path().to_path_buf());
        assert_eq!(folder.clone().normalize(), folder);
        // Missing paths stay as they are; the window reports the error.
        let missing = Browse::Open(dir.path().join("gone"));
        assert_eq!(missing.clone().normalize(), missing);
    }

    #[test]
    fn own_flags_are_recognized_only_first() {
        let parse_one = |a: &str| parse(&[OsString::from(a)], &cwd(), |_| None);
        assert_eq!(parse_one("--pick"), Launch::Pick);
        assert_eq!(
            parse_one("--unregister-shell"),
            Launch::Shell(ShellCommand::UnregisterAll)
        );
        assert_eq!(
            parse_one("--register-default"),
            Launch::Shell(ShellCommand::RegisterDefault)
        );
    }
}
