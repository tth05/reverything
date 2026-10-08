//! Opening files the way Explorer does. This runs in the UI process, as the logged on user,
//! never in the service.
//!
//! File managers like OneCommander, File Pilot or Directory Opus replace Explorer by making
//! their own verb the default one of `Directory`/`Folder`. Everything here goes through the
//! default verb so they are respected.

use std::ffi::c_void;

use reverything_protocol::Row;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{OleFlushClipboard, OleSetClipboard};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CLASSES_ROOT, RRF_RT_REG_SZ};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, ILCreateFromPathW, ILFree, SHCreateShellItemArrayFromIDLists,
    SHFileOperationW, SHOpenFolderAndSelectItems, ShellExecuteExW, ShellExecuteW, FOF_ALLOWUNDO,
    FO_DELETE, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW, SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn full_path(row: &Row) -> String {
    let mut path = row.folder.clone();
    if !path.ends_with('\\') {
        path.push('\\');
    }
    path.push_str(&row.name);
    path
}

/// The folder containing `path`, `C:\` for entries at the root.
pub fn parent(path: &str) -> Option<String> {
    match path.trim_end_matches('\\').rsplit_once('\\') {
        // Keep the separator of drive roots, `C:` alone means the current directory of C:
        Some((parent, _)) if parent.len() == 2 => Some(format!("{}\\", parent)),
        Some((parent, _)) => Some(parent.to_string()),
        None => None,
    }
}

/// Opens a file with its default program, or a folder with the default file manager.
pub fn open(path: &str) {
    unsafe {
        // No verb means the default one, which replacement file managers take over
        ShellExecuteW(
            None,
            PCWSTR::null(),
            &HSTRING::from(path),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

/// Opens the folder containing the entry. In Explorer the entry is selected; another default
/// file manager opens the folder.
pub fn reveal(path: &str) {
    if explorer_is_default() && select_in_explorer(path) {
        return;
    }
    if let Some(folder) = parent(path) {
        open(&folder);
    }
}

/// Whether folders open in Explorer, i.e. no other file manager made its verb the default.
fn explorer_is_default() -> bool {
    let mut buf = [0u16; 128];
    let mut len = size_of_val(&buf) as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CLASSES_ROOT,
            w!(r"Directory\shell"),
            PCWSTR::null(),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut len),
        )
    };
    if result.is_err() {
        return true;
    }
    let verb = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
    matches!(
        verb.to_ascii_lowercase().as_str(),
        "" | "open" | "explore" | "none"
    )
}

/// Item id lists of paths, freed on drop.
struct Pidls(Vec<*mut ITEMIDLIST>);

impl Pidls {
    /// `None` if a path does not exist (any more).
    fn new<S: AsRef<str>>(paths: &[S]) -> Option<Self> {
        let mut pidls = Pidls(Vec::with_capacity(paths.len()));
        for path in paths {
            let pidl = unsafe { ILCreateFromPathW(&HSTRING::from(path.as_ref())) };
            if pidl.is_null() {
                return None;
            }
            pidls.0.push(pidl);
        }
        Some(pidls)
    }

    fn as_const(&self) -> Vec<*const ITEMIDLIST> {
        self.0.iter().map(|&p| p as *const _).collect()
    }
}

impl Drop for Pidls {
    fn drop(&mut self) {
        for &pidl in &self.0 {
            unsafe { ILFree(Some(pidl)) };
        }
    }
}

fn select_in_explorer(path: &str) -> bool {
    let Some(item) = Pidls::new(&[path]) else {
        return false;
    };
    // An item alone opens its parent folder with the item selected
    unsafe { SHOpenFolderAndSelectItems(item.0[0], None, 0).is_ok() }
}

/// Deletes the entries like Explorer: to the Recycle Bin, or for good if `permanently`, which
/// Windows always asks to confirm. Blocks while a dialog is open. Returns whether anything was
/// deleted.
pub fn delete(paths: &[String], permanently: bool, window: Option<HWND>) -> bool {
    // A list of paths, each null terminated, ending with an empty one
    let from = paths
        .iter()
        .flat_map(|p| p.encode_utf16().chain([0]))
        .chain([0])
        .collect::<Vec<u16>>();
    let mut operation = SHFILEOPSTRUCTW {
        hwnd: window.unwrap_or_default(),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: if permanently {
            0
        } else {
            FOF_ALLOWUNDO.0 as u16
        },
        ..Default::default()
    };
    let result = unsafe { SHFileOperationW(&mut operation) };
    if result != 0 {
        crate::log::write(&format!("Deleting {:?} failed with {}", paths, result));
    }
    result == 0 && !operation.fAnyOperationsAborted.as_bool()
}

/// Shows Explorer's properties dialog for the entry.
pub fn properties(path: &str) {
    let file = HSTRING::from(path);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        lpVerb: w!("properties"),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut info);
    }
}

/// The entries as the shell's data object, the way Explorer copies or drags them.
fn data_object(paths: &[String]) -> windows::core::Result<IDataObject> {
    let pidls = Pidls::new(paths).ok_or_else(|| {
        windows::core::Error::new(
            windows::Win32::Foundation::E_INVALIDARG,
            "An entry does not exist any more",
        )
    })?;
    unsafe {
        SHCreateShellItemArrayFromIDLists(&pidls.as_const())?
            .BindToHandler::<_, IDataObject>(None, &BHID_DataObject)
    }
}

/// Puts the entries on the clipboard the way Explorer's copy does, so they can be pasted into
/// Explorer or any other program.
pub fn copy_to_clipboard(paths: &[String]) {
    let result = data_object(paths).and_then(|data| unsafe {
        OleSetClipboard(&data)?;
        // Keeps the data on the clipboard after the app exits
        OleFlushClipboard()
    });
    if let Err(e) = result {
        crate::log::write(&format!("Copying {:?} failed: {}", paths, e));
    }
}
