//! The proxy dialog objects.
//!
//! Each wraps the real comdlg32 dialog. Setters are forwarded *and* the
//! values Filex needs but the real dialog cannot report back (title, OK
//! label, filters, default extension) are recorded. `Show` asks Filex
//! first; if Filex picked something, the result getters answer from that
//! pick, otherwise every call stays with the real dialog.
//!
//! Assumes single-threaded use from the host's STA thread, which is what
//! the class's `Apartment` threading model promises; `RefCell` borrows are
//! never held across a call out of this object (re-entrancy safe).

use crate::launch;
use crate::protocol::{Mode, Request, Response};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use windows::Win32::Foundation::{E_FAIL, E_NOTIMPL, ERROR_CANCELLED, HWND, S_OK};
use windows::Win32::System::Com::{
    BIND_OPTS, CoTaskMemAlloc, CoTaskMemFree, CreateBindCtx, STGM_CREATE,
};
use windows::Win32::System::Ole::{IOleWindow, IOleWindow_Impl};
use windows::Win32::UI::Shell::Common::{COMDLG_FILTERSPEC, ITEMIDLIST};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyDescriptionList, IPropertyStore};
use windows::Win32::UI::Shell::{
    CDCONTROLSTATEF, FDAP, FILEOPENDIALOGOPTIONS, FOS_ALLOWMULTISELECT, FOS_OVERWRITEPROMPT,
    FOS_PICKFOLDERS, IFileDialog, IFileDialog_Impl, IFileDialog2, IFileDialog2_Impl,
    IFileDialogCustomize, IFileDialogCustomize_Impl, IFileDialogEvents, IFileOpenDialog,
    IFileOpenDialog_Impl, IFileOperationProgressSink, IFileSaveDialog, IFileSaveDialog_Impl,
    IModalWindow_Impl, IShellItem, IShellItemArray, IShellItemFilter, SHCreateItemFromParsingName,
    SHCreateShellItemArrayFromIDLists, SHGetIDListFromObject, SIGDN_FILESYSPATH,
};
use windows::core::{
    BOOL, GUID, HRESULT, IUnknownImpl as _, Interface, PCWSTR, PWSTR, Ref, Result, implement,
};

/// What a successful Filex pick left behind for the result getters.
struct Picked {
    paths: Vec<PathBuf>,
    filter_index: usize,
}

#[derive(Default)]
struct Recorded {
    title: Option<String>,
    ok_label: Option<String>,
    default_extension: Option<String>,
    filters: Vec<(String, String)>,
    /// The app added custom controls Filex cannot draw: use the real dialog.
    customized: bool,
    sinks: Vec<(u32, IFileDialogEvents)>,
    picked: Option<Picked>,
    /// The window `Show` was given, reported by `IOleWindow` after a
    /// Filex pick (the picker itself is gone by then).
    owner: Option<isize>,
}

/// State and logic shared by the Open and Save proxies.
struct Proxy {
    real: IFileDialog,
    recorded: RefCell<Recorded>,
}

impl Proxy {
    fn new(real: IFileDialog) -> Self {
        Self {
            real,
            recorded: RefCell::default(),
        }
    }

    fn picked_path(&self, index: usize) -> Option<PathBuf> {
        trace("picked_path");
        self.recorded
            .borrow()
            .picked
            .as_ref()
            .and_then(|p| p.paths.get(index).cloned())
    }

    fn picked_paths(&self) -> Option<Vec<PathBuf>> {
        trace("picked_paths");
        self.recorded
            .borrow()
            .picked
            .as_ref()
            .map(|p| p.paths.clone())
    }

    /// `Show`: Filex first, the real dialog on any doubt.
    fn show(&self, owner: HWND, save: bool, me: &IFileDialog) -> Result<()> {
        trace("show");
        {
            let mut recorded = self.recorded.borrow_mut();
            recorded.picked = None;
            recorded.owner = (!owner.is_invalid()).then_some(owner.0 as isize);
        }
        if launch::should_bypass() || self.recorded.borrow().customized {
            return unsafe { self.real.Show(Some(owner)) };
        }
        let mut request = self.request(save);
        request.owner = (!owner.is_invalid()).then(|| owner.0 as usize as u64);
        let outcome = std::panic::catch_unwind(|| launch::pick(&request, owner));
        match outcome {
            Ok(Some(Response::Cancelled)) => Err(HRESULT::from_win32(ERROR_CANCELLED.0).into()),
            Ok(Some(Response::Picked {
                paths,
                filter_index,
            })) => {
                self.recorded.borrow_mut().picked = Some(Picked {
                    paths,
                    filter_index,
                });
                if self.file_ok(me) {
                    Ok(())
                } else {
                    // The app rejected the pick in OnFileOk (it wanted to
                    // validate interactively): let the real dialog do it.
                    self.recorded.borrow_mut().picked = None;
                    unsafe { self.real.Show(Some(owner)) }
                }
            }
            _ => unsafe { self.real.Show(Some(owner)) },
        }
    }

    /// Everything Filex needs to draw the request, from the real dialog's
    /// getters where it has them and our records where it does not.
    fn request(&self, save: bool) -> Request {
        trace("request");
        // SAFETY: plain getters on the real dialog.
        let options = unsafe { self.real.GetOptions() }.unwrap_or_default();
        let folder = unsafe { self.real.GetFolder() }
            .ok()
            .and_then(|i| item_path(&i));
        let file_name = unsafe { self.real.GetFileName() }
            .ok()
            .and_then(take_co_string);
        let type_index = unsafe { self.real.GetFileTypeIndex() }.unwrap_or(1);
        let recorded = self.recorded.borrow();
        let mode = if options.contains(FOS_PICKFOLDERS) {
            Mode::Folder
        } else if save {
            Mode::Save
        } else {
            Mode::Open
        };
        Request {
            mode: Some(mode),
            multiple: options.contains(FOS_ALLOWMULTISELECT),
            title: recorded.title.clone(),
            ok_label: recorded.ok_label.clone(),
            folder,
            file_name: file_name.filter(|n| !n.is_empty()),
            filters: recorded.filters.clone(),
            filter_index: type_index.saturating_sub(1) as usize,
            default_extension: recorded.default_extension.clone(),
            confirm_overwrite: !save || options.contains(FOS_OVERWRITEPROMPT),
            owner: None,
        }
    }

    /// Run every advised sink's `OnFileOk`; see [`file_ok_accepts`] for
    /// what counts as a refusal.
    fn file_ok(&self, me: &IFileDialog) -> bool {
        trace("file_ok");
        let sinks: Vec<IFileDialogEvents> = self
            .recorded
            .borrow()
            .sinks
            .iter()
            .map(|(_, s)| s.clone())
            .collect();
        sinks.iter().all(|sink| {
            // SAFETY: raw vtable call so S_FALSE is visible (the wrapper
            // maps every success code to Ok).
            let hr = unsafe { (Interface::vtable(sink).OnFileOk)(sink.as_raw(), me.as_raw()) };
            trace(&format!("OnFileOk -> {:#010x}", hr.0));
            file_ok_accepts(hr)
        })
    }

    fn set_file_types(&self, count: u32, specs: *const COMDLG_FILTERSPEC) -> Result<()> {
        trace("set_file_types");
        if !specs.is_null() {
            // SAFETY: the caller passes `count` valid specs.
            let specs = unsafe { std::slice::from_raw_parts(specs, count as usize) };
            self.recorded.borrow_mut().filters = specs
                .iter()
                .map(|s| {
                    (
                        pcwstr(s.pszName).unwrap_or_default(),
                        pcwstr(s.pszSpec).unwrap_or_default(),
                    )
                })
                .collect();
        }
        unsafe {
            self.real
                .SetFileTypes(std::slice::from_raw_parts(specs, count as usize))
        }
    }

    fn advise(&self, sink: Ref<IFileDialogEvents>) -> Result<u32> {
        trace("advise");
        let cookie = unsafe { self.real.Advise(sink.ok()?)? };
        if let Some(sink) = sink.cloned() {
            self.recorded.borrow_mut().sinks.push((cookie, sink));
        }
        Ok(cookie)
    }

    fn unadvise(&self, cookie: u32) -> Result<()> {
        trace("unadvise");
        self.recorded
            .borrow_mut()
            .sinks
            .retain(|(c, _)| *c != cookie);
        unsafe { self.real.Unadvise(cookie) }
    }

    fn file_type_index(&self) -> Result<u32> {
        trace("file_type_index");
        if let Some(picked) = &self.recorded.borrow().picked {
            return Ok(picked.filter_index as u32 + 1);
        }
        unsafe { self.real.GetFileTypeIndex() }
    }

    fn folder(&self) -> Result<IShellItem> {
        trace("folder");
        match self.picked_path(0).as_deref().and_then(Path::parent) {
            Some(parent) => shell_item(parent),
            None => unsafe { self.real.GetFolder() },
        }
    }

    fn current_selection(&self) -> Result<IShellItem> {
        trace("current_selection");
        match self.picked_path(0) {
            Some(path) => shell_item(&path),
            None => unsafe { self.real.GetCurrentSelection() },
        }
    }

    fn file_name(&self) -> Result<PWSTR> {
        trace("file_name");
        match self.picked_path(0) {
            Some(path) => co_string(&path.file_name().unwrap_or_default().to_string_lossy()),
            None => unsafe { self.real.GetFileName() },
        }
    }

    fn result(&self) -> Result<IShellItem> {
        trace("result");
        match self.picked_path(0) {
            Some(path) => shell_item(&path),
            None => unsafe { self.real.GetResult() },
        }
    }

    fn customize(&self) -> Result<IFileDialogCustomize> {
        trace("customize");
        self.recorded.borrow_mut().customized = true;
        self.real.cast()
    }
}

/// Whether an `OnFileOk` reply lets the pick through. `S_FALSE` is the
/// documented "keep the dialog open" answer and any real failure is a
/// refusal too; `E_NOTIMPL` means the sink has no handler for this event
/// (Chrome's does exactly that) and must be treated as acceptance.
fn file_ok_accepts(hr: HRESULT) -> bool {
    hr == S_OK || hr == E_NOTIMPL
}

/// Generates `IModalWindow` + `IFileDialog` + `IFileDialog2` +
/// `IFileDialogCustomize` for a proxy type with a `proxy: Proxy` field.
macro_rules! forward_dialog {
    ($ty:ident, $iface:ty, $save:expr) => {
        impl IModalWindow_Impl for concat_idents_impl!($ty) {
            fn Show(&self, owner: HWND) -> Result<()> {
                trace("Show");
                let me: $iface = self.to_interface();
                let me: IFileDialog = me.cast()?;
                self.proxy.show(owner, $save, &me)
            }
        }

        impl IFileDialog_Impl for concat_idents_impl!($ty) {
            fn SetFileTypes(&self, count: u32, specs: *const COMDLG_FILTERSPEC) -> Result<()> {
                trace("SetFileTypes");
                self.proxy.set_file_types(count, specs)
            }
            fn SetFileTypeIndex(&self, index: u32) -> Result<()> {
                trace("SetFileTypeIndex");
                unsafe { self.proxy.real.SetFileTypeIndex(index) }
            }
            fn GetFileTypeIndex(&self) -> Result<u32> {
                trace("GetFileTypeIndex");
                self.proxy.file_type_index()
            }
            fn Advise(&self, sink: Ref<IFileDialogEvents>) -> Result<u32> {
                trace("Advise");
                self.proxy.advise(sink)
            }
            fn Unadvise(&self, cookie: u32) -> Result<()> {
                trace("Unadvise");
                self.proxy.unadvise(cookie)
            }
            fn SetOptions(&self, fos: FILEOPENDIALOGOPTIONS) -> Result<()> {
                trace("SetOptions");
                unsafe { self.proxy.real.SetOptions(fos) }
            }
            fn GetOptions(&self) -> Result<FILEOPENDIALOGOPTIONS> {
                trace("GetOptions");
                unsafe { self.proxy.real.GetOptions() }
            }
            fn SetDefaultFolder(&self, item: Ref<IShellItem>) -> Result<()> {
                trace("SetDefaultFolder");
                unsafe { self.proxy.real.SetDefaultFolder(item.ok()?) }
            }
            fn SetFolder(&self, item: Ref<IShellItem>) -> Result<()> {
                trace("SetFolder");
                unsafe { self.proxy.real.SetFolder(item.ok()?) }
            }
            fn GetFolder(&self) -> Result<IShellItem> {
                trace("GetFolder");
                self.proxy.folder()
            }
            fn GetCurrentSelection(&self) -> Result<IShellItem> {
                trace("GetCurrentSelection");
                self.proxy.current_selection()
            }
            fn SetFileName(&self, name: &PCWSTR) -> Result<()> {
                trace("SetFileName");
                unsafe { self.proxy.real.SetFileName(*name) }
            }
            fn GetFileName(&self) -> Result<PWSTR> {
                trace("GetFileName");
                self.proxy.file_name()
            }
            fn SetTitle(&self, title: &PCWSTR) -> Result<()> {
                trace("SetTitle");
                self.proxy.recorded.borrow_mut().title = pcwstr(*title);
                unsafe { self.proxy.real.SetTitle(*title) }
            }
            fn SetOkButtonLabel(&self, label: &PCWSTR) -> Result<()> {
                trace("SetOkButtonLabel");
                self.proxy.recorded.borrow_mut().ok_label = pcwstr(*label);
                unsafe { self.proxy.real.SetOkButtonLabel(*label) }
            }
            fn SetFileNameLabel(&self, label: &PCWSTR) -> Result<()> {
                trace("SetFileNameLabel");
                unsafe { self.proxy.real.SetFileNameLabel(*label) }
            }
            fn GetResult(&self) -> Result<IShellItem> {
                trace("GetResult");
                self.proxy.result()
            }
            fn AddPlace(&self, item: Ref<IShellItem>, fdap: FDAP) -> Result<()> {
                trace("AddPlace");
                unsafe { self.proxy.real.AddPlace(item.ok()?, fdap) }
            }
            fn SetDefaultExtension(&self, ext: &PCWSTR) -> Result<()> {
                trace("SetDefaultExtension");
                self.proxy.recorded.borrow_mut().default_extension = pcwstr(*ext);
                unsafe { self.proxy.real.SetDefaultExtension(*ext) }
            }
            fn Close(&self, hr: HRESULT) -> Result<()> {
                trace("Close");
                unsafe { self.proxy.real.Close(hr) }
            }
            fn SetClientGuid(&self, guid: *const GUID) -> Result<()> {
                trace("SetClientGuid");
                if guid.is_null() {
                    return Err(E_FAIL.into());
                }
                unsafe { self.proxy.real.SetClientGuid(guid) }
            }
            fn ClearClientData(&self) -> Result<()> {
                trace("ClearClientData");
                unsafe { self.proxy.real.ClearClientData() }
            }
            fn SetFilter(&self, filter: Ref<IShellItemFilter>) -> Result<()> {
                trace("SetFilter");
                unsafe { self.proxy.real.SetFilter(filter.ok()?) }
            }
        }

        // Apps parent their own prompts to the dialog through this (WPF
        // casts to it inside OnFileOk and gives up if it can't).
        impl IOleWindow_Impl for concat_idents_impl!($ty) {
            fn GetWindow(&self) -> Result<HWND> {
                trace("GetWindow");
                let recorded = self.proxy.recorded.borrow();
                match (&recorded.picked, recorded.owner) {
                    // After a Filex pick: the app's window, or none (a
                    // null parent is valid for the app's own prompts).
                    (Some(_), owner) => Ok(HWND(owner.unwrap_or(0) as *mut _)),
                    (None, _) => {
                        drop(recorded);
                        unsafe { self.proxy.real.cast::<IOleWindow>()?.GetWindow() }
                    }
                }
            }
            fn ContextSensitiveHelp(&self, enter: BOOL) -> Result<()> {
                trace("ContextSensitiveHelp");
                unsafe {
                    self.proxy
                        .real
                        .cast::<IOleWindow>()?
                        .ContextSensitiveHelp(enter.as_bool())
                }
            }
        }

        impl IFileDialog2_Impl for concat_idents_impl!($ty) {
            fn SetCancelButtonLabel(&self, label: &PCWSTR) -> Result<()> {
                trace("SetCancelButtonLabel");
                unsafe {
                    self.proxy
                        .real
                        .cast::<IFileDialog2>()?
                        .SetCancelButtonLabel(*label)
                }
            }
            fn SetNavigationRoot(&self, item: Ref<IShellItem>) -> Result<()> {
                trace("SetNavigationRoot");
                unsafe {
                    self.proxy
                        .real
                        .cast::<IFileDialog2>()?
                        .SetNavigationRoot(item.ok()?)
                }
            }
        }

        impl IFileDialogCustomize_Impl for concat_idents_impl!($ty) {
            fn EnableOpenDropDown(&self, id: u32) -> Result<()> {
                trace("EnableOpenDropDown");
                unsafe { self.proxy.customize()?.EnableOpenDropDown(id) }
            }
            fn AddMenu(&self, id: u32, label: &PCWSTR) -> Result<()> {
                trace("AddMenu");
                unsafe { self.proxy.customize()?.AddMenu(id, *label) }
            }
            fn AddPushButton(&self, id: u32, label: &PCWSTR) -> Result<()> {
                trace("AddPushButton");
                unsafe { self.proxy.customize()?.AddPushButton(id, *label) }
            }
            fn AddComboBox(&self, id: u32) -> Result<()> {
                trace("AddComboBox");
                unsafe { self.proxy.customize()?.AddComboBox(id) }
            }
            fn AddRadioButtonList(&self, id: u32) -> Result<()> {
                trace("AddRadioButtonList");
                unsafe { self.proxy.customize()?.AddRadioButtonList(id) }
            }
            fn AddCheckButton(&self, id: u32, label: &PCWSTR, checked: BOOL) -> Result<()> {
                trace("AddCheckButton");
                unsafe {
                    self.proxy
                        .customize()?
                        .AddCheckButton(id, *label, checked.as_bool())
                }
            }
            fn AddEditBox(&self, id: u32, text: &PCWSTR) -> Result<()> {
                trace("AddEditBox");
                unsafe { self.proxy.customize()?.AddEditBox(id, *text) }
            }
            fn AddSeparator(&self, id: u32) -> Result<()> {
                trace("AddSeparator");
                unsafe { self.proxy.customize()?.AddSeparator(id) }
            }
            fn AddText(&self, id: u32, text: &PCWSTR) -> Result<()> {
                trace("AddText");
                unsafe { self.proxy.customize()?.AddText(id, *text) }
            }
            fn SetControlLabel(&self, id: u32, label: &PCWSTR) -> Result<()> {
                trace("SetControlLabel");
                unsafe { self.proxy.customize()?.SetControlLabel(id, *label) }
            }
            fn GetControlState(&self, id: u32) -> Result<CDCONTROLSTATEF> {
                trace("GetControlState");
                unsafe { self.proxy.customize()?.GetControlState(id) }
            }
            fn SetControlState(&self, id: u32, state: CDCONTROLSTATEF) -> Result<()> {
                trace("SetControlState");
                unsafe { self.proxy.customize()?.SetControlState(id, state) }
            }
            fn GetEditBoxText(&self, id: u32) -> Result<*mut u16> {
                trace("GetEditBoxText");
                unsafe { self.proxy.customize()?.GetEditBoxText(id) }
            }
            fn SetEditBoxText(&self, id: u32, text: &PCWSTR) -> Result<()> {
                trace("SetEditBoxText");
                unsafe { self.proxy.customize()?.SetEditBoxText(id, *text) }
            }
            fn GetCheckButtonState(&self, id: u32) -> Result<BOOL> {
                trace("GetCheckButtonState");
                unsafe { self.proxy.customize()?.GetCheckButtonState(id) }
            }
            fn SetCheckButtonState(&self, id: u32, checked: BOOL) -> Result<()> {
                trace("SetCheckButtonState");
                unsafe {
                    self.proxy
                        .customize()?
                        .SetCheckButtonState(id, checked.as_bool())
                }
            }
            fn AddControlItem(&self, id: u32, item: u32, label: &PCWSTR) -> Result<()> {
                trace("AddControlItem");
                unsafe { self.proxy.customize()?.AddControlItem(id, item, *label) }
            }
            fn RemoveControlItem(&self, id: u32, item: u32) -> Result<()> {
                trace("RemoveControlItem");
                unsafe { self.proxy.customize()?.RemoveControlItem(id, item) }
            }
            fn RemoveAllControlItems(&self, id: u32) -> Result<()> {
                trace("RemoveAllControlItems");
                unsafe { self.proxy.customize()?.RemoveAllControlItems(id) }
            }
            fn GetControlItemState(&self, id: u32, item: u32) -> Result<CDCONTROLSTATEF> {
                trace("GetControlItemState");
                unsafe { self.proxy.customize()?.GetControlItemState(id, item) }
            }
            fn SetControlItemState(
                &self,
                id: u32,
                item: u32,
                state: CDCONTROLSTATEF,
            ) -> Result<()> {
                unsafe { self.proxy.customize()?.SetControlItemState(id, item, state) }
            }
            fn GetSelectedControlItem(&self, id: u32) -> Result<u32> {
                trace("GetSelectedControlItem");
                unsafe { self.proxy.customize()?.GetSelectedControlItem(id) }
            }
            fn SetSelectedControlItem(&self, id: u32, item: u32) -> Result<()> {
                trace("SetSelectedControlItem");
                unsafe { self.proxy.customize()?.SetSelectedControlItem(id, item) }
            }
            fn StartVisualGroup(&self, id: u32, label: &PCWSTR) -> Result<()> {
                trace("StartVisualGroup");
                unsafe { self.proxy.customize()?.StartVisualGroup(id, *label) }
            }
            fn EndVisualGroup(&self) -> Result<()> {
                trace("EndVisualGroup");
                unsafe { self.proxy.customize()?.EndVisualGroup() }
            }
            fn MakeProminent(&self, id: u32) -> Result<()> {
                trace("MakeProminent");
                unsafe { self.proxy.customize()?.MakeProminent(id) }
            }
            fn SetControlItemText(&self, id: u32, item: u32, label: &PCWSTR) -> Result<()> {
                trace("SetControlItemText");
                unsafe { self.proxy.customize()?.SetControlItemText(id, item, *label) }
            }
        }
    };
}

/// `#[implement]` puts the COM traits on `<Type>_Impl`.
macro_rules! concat_idents_impl {
    (OpenDialog) => {
        OpenDialog_Impl
    };
    (SaveDialog) => {
        SaveDialog_Impl
    };
}

#[implement(IFileOpenDialog, IFileDialog2, IFileDialogCustomize, IOleWindow)]
pub(crate) struct OpenDialog {
    proxy: Proxy,
    real: IFileOpenDialog,
}

impl OpenDialog {
    pub(crate) fn new(real: IFileOpenDialog) -> Self {
        Self {
            proxy: Proxy::new((*real).clone()),
            real,
        }
    }
}

forward_dialog!(OpenDialog, IFileOpenDialog, false);

impl IFileOpenDialog_Impl for OpenDialog_Impl {
    fn GetResults(&self) -> Result<IShellItemArray> {
        trace("GetResults");
        match self.proxy.picked_paths() {
            Some(paths) => {
                let array = shell_item_array(&paths);
                trace(&format!(
                    "GetResults -> {:?}",
                    array
                        .as_ref()
                        .map(|a| unsafe { a.GetCount() }.unwrap_or(0))
                        .map_err(|e| e.code())
                ));
                array
            }
            None => unsafe { self.real.GetResults() },
        }
    }

    fn GetSelectedItems(&self) -> Result<IShellItemArray> {
        trace("GetSelectedItems");
        match self.proxy.picked_paths() {
            Some(paths) => shell_item_array(&paths),
            None => unsafe { self.real.GetSelectedItems() },
        }
    }
}

#[implement(IFileSaveDialog, IFileDialog2, IFileDialogCustomize, IOleWindow)]
pub(crate) struct SaveDialog {
    proxy: Proxy,
    real: IFileSaveDialog,
}

impl SaveDialog {
    pub(crate) fn new(real: IFileSaveDialog) -> Self {
        Self {
            proxy: Proxy::new((*real).clone()),
            real,
        }
    }
}

forward_dialog!(SaveDialog, IFileSaveDialog, true);

impl IFileSaveDialog_Impl for SaveDialog_Impl {
    fn SetSaveAsItem(&self, item: Ref<IShellItem>) -> Result<()> {
        trace("SetSaveAsItem");
        unsafe { self.real.SetSaveAsItem(item.ok()?) }
    }
    fn SetProperties(&self, store: Ref<IPropertyStore>) -> Result<()> {
        trace("SetProperties");
        unsafe { self.real.SetProperties(store.ok()?) }
    }
    fn SetCollectedProperties(
        &self,
        list: Ref<IPropertyDescriptionList>,
        append_default: BOOL,
    ) -> Result<()> {
        unsafe {
            self.real
                .SetCollectedProperties(list.ok()?, append_default.as_bool())
        }
    }
    fn GetProperties(&self) -> Result<IPropertyStore> {
        trace("GetProperties");
        unsafe { self.real.GetProperties() }
    }
    fn ApplyProperties(
        &self,
        item: Ref<IShellItem>,
        store: Ref<IPropertyStore>,
        hwnd: HWND,
        sink: Ref<IFileOperationProgressSink>,
    ) -> Result<()> {
        unsafe {
            self.real
                .ApplyProperties(item.ok()?, store.ok()?, hwnd, sink.as_ref())
        }
    }
}

fn pcwstr(s: PCWSTR) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: a non-null PCWSTR from the caller is NUL-terminated.
    unsafe { s.to_string() }.ok()
}

/// Take ownership of a CoTaskMem string (as dialog getters return).
fn take_co_string(s: PWSTR) -> Option<String> {
    if s.is_null() {
        return None;
    }
    // SAFETY: `s` is a NUL-terminated CoTaskMem allocation we now own.
    let text = unsafe { s.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(s.0 as _)) };
    text
}

/// A CoTaskMem copy of `text`, which the caller frees (getter contract).
fn co_string(text: &str) -> Result<PWSTR> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: allocating and filling exactly `wide.len()` u16s.
    unsafe {
        let ptr = CoTaskMemAlloc(wide.len() * 2) as *mut u16;
        if ptr.is_null() {
            return Err(E_FAIL.into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        Ok(PWSTR(ptr))
    }
}

/// A shell item for `path`, which may not exist yet (a Save target).
/// Parsing a missing path fails unless the bind context carries
/// `STGM_CREATE`, which tells the file-system folder to build a simple
/// item for it — what the real Save dialog returns for a new file.
fn shell_item(path: &Path) -> Result<IShellItem> {
    let wide = windows::core::HSTRING::from(path.as_os_str());
    // SAFETY: parsing a file-system path.
    if let Ok(item) = unsafe { SHCreateItemFromParsingName(&wide, None) } {
        return Ok(item);
    }
    // SAFETY: a fresh bind context and a fully initialised BIND_OPTS.
    unsafe {
        let ctx = CreateBindCtx(0)?;
        let opts = BIND_OPTS {
            cbStruct: std::mem::size_of::<BIND_OPTS>() as u32,
            grfMode: STGM_CREATE.0,
            ..Default::default()
        };
        ctx.SetBindOptions(&opts)?;
        SHCreateItemFromParsingName(&wide, &ctx)
    }
}

fn item_path(item: &IShellItem) -> Option<PathBuf> {
    // SAFETY: getter returning a CoTaskMem string we own.
    let name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    take_co_string(name).map(PathBuf::from)
}

fn shell_item_array(paths: &[PathBuf]) -> Result<IShellItemArray> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    let result = (|| {
        for path in paths {
            let item = shell_item(path)
                .inspect_err(|e| trace(&format!("shell_item {path:?} -> {:?}", e.code())))?;
            // SAFETY: returns an owned PIDL, freed below.
            pidls.push(unsafe { SHGetIDListFromObject(&item)? });
        }
        let refs: Vec<*const ITEMIDLIST> = pidls.iter().map(|&p| p as *const _).collect();
        // SAFETY: every PIDL is valid until freed below.
        unsafe { SHCreateShellItemArrayFromIDLists(&refs) }
    })();
    for pidl in pidls {
        // SAFETY: each PIDL came from SHGetIDListFromObject.
        unsafe { CoTaskMemFree(Some(pidl as _)) };
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};

    /// Regression: Chrome answers OnFileOk with E_NOTIMPL ("no handler"),
    /// which was read as a refusal, so every Chrome pick fell back to the
    /// system dialog.
    #[test]
    fn on_file_ok_not_implemented_is_acceptance() {
        use windows::Win32::Foundation::{E_ACCESSDENIED, S_FALSE};
        assert!(file_ok_accepts(S_OK));
        assert!(file_ok_accepts(E_NOTIMPL));
        assert!(!file_ok_accepts(S_FALSE));
        assert!(!file_ok_accepts(E_ACCESSDENIED));
    }

    /// Regression: a Save to a new file failed in the host with "file not
    /// found", because parsing a path that does not exist yet fails
    /// without `STGM_CREATE` in the bind context.
    #[test]
    fn save_targets_that_do_not_exist_yet_become_items() {
        // SAFETY: STA init for this test thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let dir = std::env::temp_dir();
        let missing = dir.join("filex-shim-test-not-created.txt");
        let _ = std::fs::remove_file(&missing);
        let item = shell_item(&missing).expect("item for a new file");
        assert_eq!(item_path(&item), Some(missing.clone()));
        let array = shell_item_array(&[missing.clone(), dir.clone()]).expect("array");
        // SAFETY: plain getter.
        assert_eq!(unsafe { array.GetCount() }.unwrap(), 2);
        assert!(
            !missing.exists(),
            "making the item must not create the file"
        );
    }
}

/// Opt-in call trace for diagnosing a host app: set
/// `FILEX_DIALOG_SHIM_TRACE` to a file path in that app's environment and
/// every dialog call is appended to it as `<pid> <method>`. Off (one
/// cached env lookup) otherwise.
fn trace(what: &str) {
    use std::io::Write as _;
    static PATH: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let Some(path) =
        PATH.get_or_init(|| std::env::var_os("FILEX_DIALOG_SHIM_TRACE").map(PathBuf::from))
    else {
        return;
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{} {what}", std::process::id());
    }
}
