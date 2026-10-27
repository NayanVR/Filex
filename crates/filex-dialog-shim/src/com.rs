//! COM server plumbing: the DLL exports, the class factory, and creating
//! the *real* dialog objects.
//!
//! Assumes comdlg32.dll in System32 implements both dialog classes and
//! exports `DllGetClassObject` (true since Vista). We must go to it
//! directly: `CoCreateInstance` would resolve the CLSID through the
//! registry and come straight back to this DLL.

use crate::dialog::{OpenDialog, SaveDialog};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_NOINTERFACE, E_POINTER, S_FALSE,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog,
};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface, Ref, Result, implement, s, w};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Open,
    Save,
}

impl Kind {
    fn clsid(self) -> GUID {
        match self {
            Kind::Open => FileOpenDialog,
            Kind::Save => FileSaveDialog,
        }
    }
}

type GetClassObject =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT;

/// The real comdlg32 dialog of `kind`, bypassing the registry.
pub(crate) fn create_system_dialog(kind_clsid: &GUID) -> Result<IFileDialog> {
    // SAFETY: loading a System32 DLL by name with the System32-only search
    // flag; the module stays loaded for the process (comdlg32 never
    // unloads once used, and we never free it).
    let module = unsafe { LoadLibraryExW(w!("comdlg32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)? };
    // SAFETY: looking up a documented export.
    let proc = unsafe { GetProcAddress(module, s!("DllGetClassObject")) }
        .ok_or_else(|| windows::core::Error::from(CLASS_E_CLASSNOTAVAILABLE))?;
    // SAFETY: DllGetClassObject has exactly this signature.
    let get: GetClassObject = unsafe { std::mem::transmute(proc) };
    let mut factory: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: out-pointer for an IClassFactory.
    unsafe { get(kind_clsid, &IClassFactory::IID, &mut factory) }.ok()?;
    // SAFETY: the call succeeded, so `factory` is an owned IClassFactory.
    let factory = unsafe { IClassFactory::from_raw(factory) };
    // SAFETY: standard CreateInstance with no aggregation.
    unsafe { factory.CreateInstance::<Option<&IUnknown>, IFileDialog>(None) }
}

#[implement(IClassFactory)]
struct Factory {
    kind: Kind,
}

impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> Result<()> {
        if ppv.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked non-null above.
        unsafe { *ppv = std::ptr::null_mut() };
        if !outer.is_null() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let real = create_system_dialog(&self.kind.clsid())?;
        let unknown: IUnknown = match self.kind {
            Kind::Open => {
                let real: IFileOpenDialog = real.cast()?;
                OpenDialog::new(real).into()
            }
            Kind::Save => {
                let real: IFileSaveDialog = real.cast()?;
                SaveDialog::new(real).into()
            }
        };
        // SAFETY: `riid`/`ppv` are the caller's valid pointers.
        unsafe { unknown.query(riid, ppv) }.ok()
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

/// # Safety
/// COM calls this with valid pointers (the standard export contract).
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: checked non-null above.
    unsafe { *ppv = std::ptr::null_mut() };
    // SAFETY: as above.
    let clsid = unsafe { *rclsid };
    let kind = if clsid == FileOpenDialog {
        Kind::Open
    } else if clsid == FileSaveDialog {
        Kind::Save
    } else {
        return CLASS_E_CLASSNOTAVAILABLE;
    };
    let factory: IClassFactory = Factory { kind }.into();
    // SAFETY: caller's valid out-pointers.
    let hr = unsafe { factory.query(riid, ppv) };
    if hr.is_err() { E_NOINTERFACE } else { hr }
}

/// Never unload: a host may still hold interface pointers into us in ways
/// the refcount does not capture (cached vtables), and the DLL is small.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}
