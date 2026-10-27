//! The proxy as COM sees it: created through `DllGetClassObject`, wrapping
//! a real comdlg32 dialog, forwarding state both ways. Never calls `Show`,
//! so it runs headless in CI.
#![cfg(target_os = "windows")]

use filex_dialog_shim::DllGetClassObject;
use windows::Win32::Foundation::CLASS_E_CLASSNOTAVAILABLE;
use windows::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, IClassFactory,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog, IFileDialog,
    IFileDialog2, IFileDialogCustomize, IFileOpenDialog, IFileSaveDialog,
};
use windows::core::{GUID, IUnknown, Interface, w};

fn factory(clsid: &GUID) -> windows::core::Result<IClassFactory> {
    // SAFETY: STA init for this test thread; repeated init is harmless.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let mut out = std::ptr::null_mut();
    // SAFETY: valid pointers per the export's contract.
    unsafe { DllGetClassObject(clsid, &IClassFactory::IID, &mut out) }.ok()?;
    // SAFETY: success hands us an owned IClassFactory.
    Ok(unsafe { IClassFactory::from_raw(out) })
}

#[test]
fn save_dialog_forwards_state_to_the_real_dialog() {
    let factory = factory(&FileSaveDialog).unwrap();
    let dialog: IFileSaveDialog =
        unsafe { factory.CreateInstance::<Option<&IUnknown>, _>(None) }.unwrap();
    unsafe {
        dialog.SetFileName(w!("report.txt")).unwrap();
        let name = dialog.GetFileName().unwrap();
        assert_eq!(name.to_string().unwrap(), "report.txt");
        CoTaskMemFree(Some(name.0 as _));

        let options = dialog.GetOptions().unwrap();
        assert!(
            options.contains(FOS_OVERWRITEPROMPT),
            "real dialog defaults"
        );

        let specs = [COMDLG_FILTERSPEC {
            pszName: w!("Text"),
            pszSpec: w!("*.txt"),
        }];
        dialog.SetFileTypes(&specs).unwrap();
        dialog.SetFileTypeIndex(1).unwrap();
        assert_eq!(dialog.GetFileTypeIndex().unwrap(), 1);
    }
    // The interfaces apps commonly query for are all there.
    dialog.cast::<IFileDialog>().unwrap();
    dialog.cast::<IFileDialog2>().unwrap();
    dialog.cast::<IFileDialogCustomize>().unwrap();
    assert!(dialog.cast::<IFileOpenDialog>().is_err());
}

#[test]
fn open_dialog_round_trips_folder_options() {
    let factory = factory(&FileOpenDialog).unwrap();
    let dialog: IFileOpenDialog =
        unsafe { factory.CreateInstance::<Option<&IUnknown>, _>(None) }.unwrap();
    unsafe {
        let options = dialog.GetOptions().unwrap();
        dialog.SetOptions(options | FOS_PICKFOLDERS).unwrap();
        assert!(dialog.GetOptions().unwrap().contains(FOS_PICKFOLDERS));
        dialog.SetTitle(w!("Choose a folder")).unwrap();
    }
    assert!(dialog.cast::<IFileSaveDialog>().is_err());
}

#[test]
fn other_classes_are_refused() {
    let unknown = GUID::from_u128(0x12345678_1234_1234_1234_123456789abc);
    assert_eq!(
        factory(&unknown).unwrap_err().code(),
        CLASS_E_CLASSNOTAVAILABLE
    );
}
