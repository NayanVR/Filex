//! Manual end-to-end check without registering anything: load the built
//! DLL the way COM would, create a dialog through it and `Show` it, then
//! print what the app would see.
//!
//!   cargo build -p filex-dialog-shim --bin filex   (both into target/debug)
//!   cargo run -p filex-dialog-shim --example show_dialog -- open <folder>
//!   cargo run -p filex-dialog-shim --example show_dialog -- save <folder>
//!   … -- multi <folder>    (Open with FOS_ALLOWMULTISELECT, prints GetResults)
//!   … -- folder <folder>   (FOS_PICKFOLDERS)

#[cfg(target_os = "windows")]
fn main() -> windows::core::Result<()> {
    use windows::Win32::System::Com::{
        COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IClassFactory,
    };
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
    use windows::Win32::UI::Shell::{
        FOS_ALLOWMULTISELECT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog, IFileDialog,
        IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
    };
    use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, s, w};

    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let save = mode == "save";
    let folder = args.next();
    let dll = std::env::current_exe()?
        .parent()
        .and_then(|examples| examples.parent())
        .map(|debug| debug.join("filex_dialog_shim.dll"))
        .expect("target dir");
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let module = LoadLibraryW(&HSTRING::from(dll.as_os_str()))?;
        let get: unsafe extern "system" fn(
            *const GUID,
            *const GUID,
            *mut *mut core::ffi::c_void,
        ) -> HRESULT =
            std::mem::transmute(GetProcAddress(module, s!("DllGetClassObject")).expect("export"));
        let clsid = if save { FileSaveDialog } else { FileOpenDialog };
        let mut out = std::ptr::null_mut();
        get(&clsid, &IClassFactory::IID, &mut out).ok()?;
        let factory = IClassFactory::from_raw(out);
        let dialog: IFileDialog = factory.CreateInstance::<Option<&IUnknown>, _>(None)?;
        dialog.SetTitle(w!("Shim end-to-end"))?;
        dialog.SetFileTypes(&[
            COMDLG_FILTERSPEC {
                pszName: w!("Text files"),
                pszSpec: w!("*.txt"),
            },
            COMDLG_FILTERSPEC {
                pszName: w!("All files"),
                pszSpec: w!("*.*"),
            },
        ])?;
        match mode.as_str() {
            "multi" => dialog.SetOptions(dialog.GetOptions()? | FOS_ALLOWMULTISELECT)?,
            "folder" => dialog.SetOptions(dialog.GetOptions()? | FOS_PICKFOLDERS)?,
            _ => {}
        }
        if save {
            dialog.SetFileName(w!("e2e"))?;
            dialog.SetDefaultExtension(w!("txt"))?;
        }
        if let Some(folder) = folder {
            let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(folder), None)?;
            dialog.SetFolder(&item)?;
        }
        match dialog.Show(None) {
            Ok(()) => {
                let item = dialog.GetResult()?;
                let path = item.GetDisplayName(SIGDN_FILESYSPATH)?;
                println!(
                    "picked {} (type {})",
                    path.to_string().unwrap(),
                    dialog.GetFileTypeIndex()?
                );
                CoTaskMemFree(Some(path.0 as _));
                if mode == "multi" {
                    let items = dialog.cast::<IFileOpenDialog>()?.GetResults()?;
                    for i in 0..items.GetCount()? {
                        let name = items.GetItemAt(i)?.GetDisplayName(SIGDN_FILESYSPATH)?;
                        println!("  result {i}: {}", name.to_string().unwrap());
                        CoTaskMemFree(Some(name.0 as _));
                    }
                }
            }
            Err(err) => println!("show returned {:?}", err.code()),
        }
        let _ = dialog.cast::<IFileDialog>()?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn main() {}
