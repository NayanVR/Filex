//! Windows: per-user registry entries under `HKCU\Software\Classes`.
//!
//! Assumes the documented HKCR merge: a key that exists under
//! `HKCU\Software\Classes` shadows the same key under
//! `HKLM\Software\Classes` for unelevated processes, so nothing here needs
//! admin rights, and deleting our HKCU keys restores the machine defaults
//! exactly. Elevated processes read HKLM only, so they keep Explorer and
//! the system dialog — by design.

use super::Status;
use anyhow::{Context, Result, bail};
use std::path::Path;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, WIN32_ERROR};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteTreeW, RegDeleteValueW, RegGetValueW,
    RegOpenKeyExW, RegQueryInfoKeyW, RegSetValueExW,
};
use windows::core::HSTRING;

/// Our verb name under `Directory\shell` and `Drive\shell`.
const VERB: &str = "filex";
/// The "File Explorer" launcher object (ShellExecute of
/// `shell:::{52205fd8-…}`). The Win+E hotkey does not go through it on
/// Windows 11 (checked 2026-10-27), so Win+E still opens Explorer.
const EXPLORER_LAUNCHER: &str = r"CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";
pub const CLSID_FILE_OPEN_DIALOG: &str = "{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}";
pub const CLSID_FILE_SAVE_DIALOG: &str = "{C0B4E2F3-BA21-4773-8DBA-335EC946EB8B}";
/// Where the previous default verbs are kept so unregistering restores
/// them, relative to the hive's backup base.
const BACKUP_KEY: &str = "ShellBackup";
/// Stored for a default verb that did not exist before we set ours. Not a
/// valid verb name (`<` is illegal in key names), and free of NULs, which
/// REG_SZ would truncate at.
const ABSENT: &str = "<absent>";
/// Backup value listing the parent keys registration had to create
/// (`|`-separated, relative to the classes root). Unregistering deletes
/// only these, and only while empty: keys the user already had — even
/// empty ones — are left exactly as they were.
const CREATED_KEYS: &str = "CreatedKeys";

/// A registry subtree we read and write: `classes` plays the role of
/// `HKCU\Software\Classes`, `app` of `HKCU\Software\Filex`. Tests point
/// both at a scratch key so they never touch the real associations.
pub struct Hive {
    root: HKEY,
    classes: String,
    app: String,
}

impl Hive {
    pub fn user_classes() -> Self {
        Self {
            root: HKEY_CURRENT_USER,
            classes: r"Software\Classes".into(),
            app: r"Software\Filex".into(),
        }
    }

    fn class_key(&self, key: &str) -> String {
        format!(r"{}\{key}", self.classes)
    }

    fn app_key(&self, key: &str) -> String {
        format!(r"{}\{key}", self.app)
    }
}

/// One string value to write. `name: None` is the key's default value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Write {
    pub key: String,
    pub name: Option<&'static str>,
    pub data: String,
}

fn write(key: impl Into<String>, name: Option<&'static str>, data: impl Into<String>) -> Write {
    Write {
        key: key.into(),
        name,
        data: data.into(),
    }
}

/// The values that make folders, drives and the launcher open `exe`. The default
/// verbs of `Directory\shell` and `Drive\shell` are handled separately
/// (they need backing up first).
pub fn default_explorer_writes(exe: &Path) -> Vec<Write> {
    let exe = exe.display();
    let open_path = format!(r#""{exe}" "%1""#);
    let mut writes = Vec::new();
    for class in ["Directory", "Drive"] {
        let verb = format!(r"{class}\shell\{VERB}");
        writes.push(write(&verb, None, "Open in Filex"));
        writes.push(write(format!(r"{verb}\command"), None, &open_path));
    }
    let launcher = format!(r"{EXPLORER_LAUNCHER}\shell\opennewwindow\command");
    writes.push(write(&launcher, None, format!(r#""{exe}""#)));
    // Blank out the machine key's DelegateExecute so the shell runs our
    // command line instead of Explorer's COM launcher.
    writes.push(write(&launcher, Some("DelegateExecute"), ""));
    writes
}

/// The values that load `shim` in place of the system file dialogs.
pub fn dialog_writes(shim: &Path) -> Vec<Write> {
    let mut writes = Vec::new();
    for clsid in [CLSID_FILE_OPEN_DIALOG, CLSID_FILE_SAVE_DIALOG] {
        let server = format!(r"CLSID\{clsid}\InprocServer32");
        writes.push(write(&server, None, shim.display().to_string()));
        // Matches comdlg32's own registration.
        writes.push(write(&server, Some("ThreadingModel"), "Apartment"));
    }
    writes
}

/// Parent keys registration may have to create, outermost first.
fn scaffolding_keys() -> Vec<String> {
    let mut keys = Vec::new();
    for class in VERB_CLASSES {
        keys.push(class.to_string());
        keys.push(format!(r"{class}\shell"));
    }
    keys.push("CLSID".to_string());
    keys.push(EXPLORER_LAUNCHER.to_string());
    keys.push(format!(r"{EXPLORER_LAUNCHER}\shell"));
    keys
}

/// Classes whose default verb we take over.
const VERB_CLASSES: [&str; 2] = ["Directory", "Drive"];

/// Register Filex as the default explorer.
pub fn register_default(hive: &Hive, exe: &Path) -> Result<()> {
    if !exe.is_file() {
        bail!("{} does not exist", exe.display());
    }
    let backup_key = hive.app_key(BACKUP_KEY);
    if get_string(hive.root, &backup_key, Some(CREATED_KEYS))?.is_none() {
        let mut created = Vec::new();
        for key in scaffolding_keys() {
            if !key_exists(hive.root, &hive.class_key(&key))? {
                created.push(key);
            }
        }
        set_string(
            hive.root,
            &backup_key,
            Some(CREATED_KEYS),
            &created.join("|"),
        )?;
    }
    for w in default_explorer_writes(exe) {
        set_string(hive.root, &hive.class_key(&w.key), w.name, &w.data)?;
    }
    for class in VERB_CLASSES {
        let shell = hive.class_key(&format!(r"{class}\shell"));
        let current = get_string(hive.root, &shell, None)?;
        if current.as_deref() != Some(VERB) {
            let backup = current.unwrap_or_else(|| ABSENT.to_string());
            set_string(
                hive.root,
                &hive.app_key(BACKUP_KEY),
                Some(class_static(class)),
                &backup,
            )?;
        }
        set_string(hive.root, &shell, None, VERB)?;
    }
    Ok(())
}

/// Remove exactly what [`register_default`] added and restore the previous
/// default verbs. Safe to run when nothing is registered.
pub fn unregister_default(hive: &Hive) -> Result<()> {
    for class in VERB_CLASSES {
        let shell = hive.class_key(&format!(r"{class}\shell"));
        if get_string(hive.root, &shell, None)?.as_deref() == Some(VERB) {
            let backup_key = hive.app_key(BACKUP_KEY);
            match get_string(hive.root, &backup_key, Some(class_static(class)))?.as_deref() {
                Some(previous) if previous != ABSENT => {
                    set_string(hive.root, &shell, None, previous)?;
                }
                _ => delete_value(hive.root, &shell, None)?,
            }
            delete_value(hive.root, &backup_key, Some(class_static(class)))?;
        }
        delete_tree(
            hive.root,
            &hive.class_key(&format!(r"{class}\shell\{VERB}")),
        )?;
    }
    delete_tree(
        hive.root,
        &hive.class_key(&format!(r"{EXPLORER_LAUNCHER}\shell\opennewwindow")),
    )?;
    let backup_key = hive.app_key(BACKUP_KEY);
    if let Some(created) = get_string(hive.root, &backup_key, Some(CREATED_KEYS))? {
        let mut created: Vec<&str> = created.split('|').filter(|k| !k.is_empty()).collect();
        // Deepest first, so a parent is empty by the time it is checked.
        created.sort_by_key(|k| std::cmp::Reverse(k.matches('\\').count()));
        for key in created {
            delete_if_empty(hive.root, &hive.class_key(key))?;
        }
        delete_value(hive.root, &backup_key, Some(CREATED_KEYS))?;
    }
    Ok(())
}

pub fn register_dialogs(hive: &Hive, shim: &Path) -> Result<()> {
    if !shim.is_file() {
        bail!(
            "the dialog shim is missing ({}); this build does not include it",
            shim.display()
        );
    }
    for w in dialog_writes(shim) {
        set_string(hive.root, &hive.class_key(&w.key), w.name, &w.data)?;
    }
    Ok(())
}

pub fn unregister_dialogs(hive: &Hive) -> Result<()> {
    for clsid in [CLSID_FILE_OPEN_DIALOG, CLSID_FILE_SAVE_DIALOG] {
        delete_tree(hive.root, &hive.class_key(&format!(r"CLSID\{clsid}")))?;
    }
    Ok(())
}

pub fn status(hive: &Hive) -> Status {
    let default_explorer = VERB_CLASSES.iter().all(|class| {
        get_string(hive.root, &hive.class_key(&format!(r"{class}\shell")), None)
            .ok()
            .flatten()
            .as_deref()
            == Some(VERB)
    });
    let dialogs = [CLSID_FILE_OPEN_DIALOG, CLSID_FILE_SAVE_DIALOG]
        .iter()
        .all(|clsid| {
            get_string(
                hive.root,
                &hive.class_key(&format!(r"CLSID\{clsid}\InprocServer32")),
                None,
            )
            .ok()
            .flatten()
            .is_some_and(|dll| dll.to_ascii_lowercase().contains("filex"))
        });
    Status {
        default_explorer,
        dialogs,
    }
}

/// Tell Explorer the associations changed so open windows pick up the new
/// verbs without a sign-out.
pub fn notify_associations_changed() {
    #[cfg(feature = "app")]
    {
        use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
        // SAFETY: SHCNE_ASSOCCHANGED takes no item pointers.
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    }
}

fn class_static(class: &str) -> &'static str {
    match class {
        "Directory" => "Directory",
        _ => "Drive",
    }
}

fn check(status: WIN32_ERROR, what: &str, key: &str) -> Result<()> {
    status.ok().with_context(|| format!("{what} {key}"))
}

fn set_string(root: HKEY, key: &str, name: Option<&str>, data: &str) -> Result<()> {
    let mut hkey = HKEY::default();
    // SAFETY: `hkey` is a valid out-pointer; the key is closed below.
    let status = unsafe {
        RegCreateKeyExW(
            root,
            &HSTRING::from(key),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut hkey,
            None,
        )
    };
    check(status, "creating", key)?;
    let wide: Vec<u16> = data.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: REG_SZ data is the NUL-terminated UTF-16 buffer as bytes.
    let bytes = unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
    let name = HSTRING::from(name.unwrap_or(""));
    // SAFETY: `hkey` was opened for writing above.
    let status = unsafe { RegSetValueExW(hkey, &name, None, REG_SZ, Some(bytes)) };
    // SAFETY: closing the key opened above.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    check(status, "writing", key)
}

/// A string value, or `None` when the key or value does not exist.
fn get_string(root: HKEY, key: &str, name: Option<&str>) -> Result<Option<String>> {
    let key_w = HSTRING::from(key);
    let name_w = HSTRING::from(name.unwrap_or(""));
    let mut size = 0u32;
    // SAFETY: size query with no buffer.
    let status = unsafe {
        RegGetValueW(
            root,
            &key_w,
            &name_w,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status, "reading", key)?;
    let mut buf = vec![0u16; (size as usize).div_ceil(2)];
    // SAFETY: `buf` holds `size` bytes.
    let status = unsafe {
        RegGetValueW(
            root,
            &key_w,
            &name_w,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    check(status, "reading", key)?;
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Ok(Some(String::from_utf16_lossy(&buf[..len])))
}

fn delete_value(root: HKEY, key: &str, name: Option<&str>) -> Result<()> {
    let Some(hkey) = open(root, key, KEY_WRITE)? else {
        return Ok(());
    };
    // SAFETY: `hkey` is open for writing.
    let status = unsafe { RegDeleteValueW(hkey, &HSTRING::from(name.unwrap_or(""))) };
    // SAFETY: closing the key opened above.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "deleting a value of", key)
}

fn delete_tree(root: HKEY, key: &str) -> Result<()> {
    // SAFETY: plain registry call on a string path.
    let status = unsafe { RegDeleteTreeW(root, &HSTRING::from(key)) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "deleting", key)?;
    // RegDeleteTree leaves the (now empty) key itself.
    // SAFETY: as above.
    let status = unsafe { RegDeleteKeyW(root, &HSTRING::from(key)) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "deleting", key)
}

/// Delete `key` if it has no subkeys and no values. Missing is fine.
fn delete_if_empty(root: HKEY, key: &str) -> Result<()> {
    let Some(hkey) = open(root, key, KEY_READ)? else {
        return Ok(());
    };
    let (mut subkeys, mut values) = (0u32, 0u32);
    // SAFETY: `hkey` is open for reading; out-pointers are valid.
    let status = unsafe {
        RegQueryInfoKeyW(
            hkey,
            None,
            None,
            None,
            Some(&mut subkeys),
            None,
            None,
            Some(&mut values),
            None,
            None,
            None,
            None,
        )
    };
    // SAFETY: closing the key opened above.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    check(status, "inspecting", key)?;
    if subkeys > 0 || values > 0 {
        return Ok(());
    }
    // SAFETY: plain registry call on a string path.
    let status = unsafe { RegDeleteKeyW(root, &HSTRING::from(key)) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "deleting", key)
}

fn key_exists(root: HKEY, key: &str) -> Result<bool> {
    let Some(hkey) = open(root, key, KEY_READ)? else {
        return Ok(false);
    };
    // SAFETY: closing the key just opened.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    Ok(true)
}

fn open(
    root: HKEY,
    key: &str,
    access: windows::Win32::System::Registry::REG_SAM_FLAGS,
) -> Result<Option<HKEY>> {
    let mut hkey = HKEY::default();
    // SAFETY: `hkey` is a valid out-pointer.
    let status = unsafe { RegOpenKeyExW(root, &HSTRING::from(key), None, access, &mut hkey) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status, "opening", key)?;
    Ok(Some(hkey))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway subtree under HKCU\Software\Filex-test-<random>, deleted
    /// on drop. Registry writes here never affect real associations.
    struct Scratch {
        base: String,
    }

    impl Scratch {
        fn new() -> Self {
            let mut bytes = [0u8; 8];
            getrandom::getrandom(&mut bytes).unwrap();
            let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            Self {
                base: format!(r"Software\Filex-test-{id}"),
            }
        }

        fn hive(&self) -> Hive {
            Hive {
                root: HKEY_CURRENT_USER,
                classes: format!(r"{}\Classes", self.base),
                app: format!(r"{}\App", self.base),
            }
        }

        fn get(&self, key: &str, name: Option<&str>) -> Option<String> {
            get_string(
                HKEY_CURRENT_USER,
                &format!(r"{}\Classes\{key}", self.base),
                name,
            )
            .unwrap()
        }

        fn exists(&self, key: &str) -> bool {
            open(
                HKEY_CURRENT_USER,
                &format!(r"{}\Classes\{key}", self.base),
                KEY_READ,
            )
            .unwrap()
            .inspect(|&h| unsafe {
                let _ = RegCloseKey(h);
            })
            .is_some()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = delete_tree(HKEY_CURRENT_USER, &self.base);
        }
    }

    fn fake_exe(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn the_write_plan_quotes_paths_with_spaces() {
        let writes = default_explorer_writes(Path::new(r"C:\Program Files\Filex\filex.exe"));
        let command = writes
            .iter()
            .find(|w| w.key == r"Directory\shell\filex\command")
            .unwrap();
        assert_eq!(command.data, r#""C:\Program Files\Filex\filex.exe" "%1""#);
        assert!(
            writes
                .iter()
                .any(|w| w.name == Some("DelegateExecute") && w.data.is_empty())
        );
    }

    #[test]
    fn register_then_unregister_restores_a_previous_default_verb() {
        let scratch = Scratch::new();
        let hive = scratch.hive();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(&dir, "filex.exe");
        // Another app had already set Directory's default verb.
        set_string(
            hive.root,
            &hive.class_key(r"Directory\shell"),
            None,
            "otherapp",
        )
        .unwrap();

        register_default(&hive, &exe).unwrap();
        assert_eq!(
            scratch.get(r"Directory\shell", None).as_deref(),
            Some("filex")
        );
        assert_eq!(scratch.get(r"Drive\shell", None).as_deref(), Some("filex"));
        assert!(status(&hive).default_explorer);
        // Registering twice must not overwrite the backup with our own verb.
        register_default(&hive, &exe).unwrap();

        unregister_default(&hive).unwrap();
        assert_eq!(
            scratch.get(r"Directory\shell", None).as_deref(),
            Some("otherapp")
        );
        assert!(!scratch.exists(r"Directory\shell\filex"));
        // Drive had no default before: its scaffolding is gone entirely.
        assert!(!scratch.exists(r"Drive"));
        assert!(!scratch.exists(EXPLORER_LAUNCHER));
        assert!(!status(&hive).default_explorer);
    }

    #[test]
    fn unregister_leaves_other_apps_verbs_alone() {
        let scratch = Scratch::new();
        let hive = scratch.hive();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(&dir, "filex.exe");
        set_string(
            hive.root,
            &hive.class_key(r"Directory\shell\vscode\command"),
            None,
            "code %1",
        )
        .unwrap();
        register_default(&hive, &exe).unwrap();
        unregister_default(&hive).unwrap();
        assert_eq!(
            scratch
                .get(r"Directory\shell\vscode\command", None)
                .as_deref(),
            Some("code %1")
        );
    }

    /// Regression: on a real machine `Directory\shell`, `Drive` and
    /// `Drive\shell` already existed as empty keys, and unregistering
    /// deleted them because it pruned every empty parent.
    #[test]
    fn unregister_keeps_empty_keys_the_user_already_had() {
        let scratch = Scratch::new();
        let hive = scratch.hive();
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_exe(&dir, "filex.exe");
        for key in [r"Directory\shell", r"Drive\shell"] {
            let full = hive.class_key(key);
            set_string(hive.root, &full, Some("tmp"), "x").unwrap();
            delete_value(hive.root, &full, Some("tmp")).unwrap();
        }

        register_default(&hive, &exe).unwrap();
        unregister_default(&hive).unwrap();

        assert!(scratch.exists(r"Directory\shell"));
        assert!(scratch.exists(r"Drive\shell"));
        assert_eq!(scratch.get(r"Directory\shell", None), None);
        assert!(!scratch.exists(r"Directory\shell\filex"));
        // The launcher keys were ours alone, so they go.
        assert!(!scratch.exists(EXPLORER_LAUNCHER));
    }

    #[test]
    fn unregister_without_register_is_a_no_op() {
        let scratch = Scratch::new();
        unregister_default(&scratch.hive()).unwrap();
        unregister_dialogs(&scratch.hive()).unwrap();
    }

    #[test]
    fn missing_binaries_are_refused() {
        let scratch = Scratch::new();
        let missing = Path::new(r"C:\definitely\not\here\filex.exe");
        assert!(register_default(&scratch.hive(), missing).is_err());
        assert!(register_dialogs(&scratch.hive(), missing).is_err());
        assert!(!scratch.exists("Directory"));
    }

    #[test]
    fn dialog_registration_round_trips() {
        let scratch = Scratch::new();
        let hive = scratch.hive();
        let dir = tempfile::tempdir().unwrap();
        let shim = fake_exe(&dir, "filex_dialog_shim.dll");
        register_dialogs(&hive, &shim).unwrap();
        assert!(status(&hive).dialogs);
        assert_eq!(
            scratch
                .get(
                    &format!(r"CLSID\{CLSID_FILE_SAVE_DIALOG}\InprocServer32"),
                    Some("ThreadingModel")
                )
                .as_deref(),
            Some("Apartment")
        );
        unregister_dialogs(&hive).unwrap();
        assert!(!status(&hive).dialogs);
        assert!(!scratch.exists(&format!(r"CLSID\{CLSID_FILE_OPEN_DIALOG}")));
    }
}
