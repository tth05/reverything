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
use windows::Win32::System::Ole::{IDropSource, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CLASSES_ROOT, RRF_RT_REG_SZ};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, IShellItem, SHCreateItemFromParsingName, SHDoDragDrop, ShellExecuteExW,
    ShellExecuteW, SEE_MASK_INVOKEIDLIST, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::Shell::{ILFree, SHOpenFolderAndSelectItems, SHParseDisplayName};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

pub fn full_path(row: &Row) -> String {
    let mut path = row.folder.clone();
    if !path.ends_with('\\') {
        path.push('\\');
    }
    path.push_str(&row.name);
    path
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
    let folder = match path.trim_end_matches('\\').rsplit_once('\\') {
        // Keep the separator of drive roots, `C:` alone means the current directory of C:
        Some((parent, _)) if parent.len() == 2 => format!("{}\\", parent),
        Some((parent, _)) => parent.to_string(),
        None => return,
    };
    open(&folder);
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

fn select_in_explorer(path: &str) -> bool {
    unsafe {
        let mut item: *mut ITEMIDLIST = std::ptr::null_mut();
        if SHParseDisplayName(&HSTRING::from(path), None, &mut item, 0, None).is_err() {
            return false;
        }
        // An item alone opens its parent folder with the item selected
        let ok = SHOpenFolderAndSelectItems(item, None, 0).is_ok();
        ILFree(Some(item));
        ok
    }
}

/// Shows Explorer's properties dialog for the entry.
pub fn properties(path: &str) {
    let file = HSTRING::from(path);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_INVOKEIDLIST,
        lpVerb: w!("properties"),
        lpFile: windows::core::PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        let _ = ShellExecuteExW(&mut info);
    }
}

/// Drags the entry to wherever the user drops it (Explorer, the desktop, other programs), with
/// the shell's own drag image and copy/move/link handling. Blocks until the drop.
pub fn drag_out(path: &str, window: Option<HWND>) {
    let result = unsafe {
        SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(path), None)
            .and_then(|item| item.BindToHandler::<_, IDataObject>(None, &BHID_DataObject))
            .and_then(|data| {
                SHDoDragDrop(
                    window,
                    &data,
                    None::<&IDropSource>,
                    DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
                )
            })
    };
    if let Err(e) = result {
        crate::log::write(&format!("Dragging {} failed: {}", path, e));
    }
}
