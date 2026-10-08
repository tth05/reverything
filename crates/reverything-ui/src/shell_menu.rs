//! Explorer's context menu for entries, read into a list so the app can draw it in its own menu
//! style, the way File Pilot or OneCommander do, or shown as Windows' own menu, which tools like
//! Nilesoft Shell restyle. Shell extensions (7-Zip, Git, editors) show up as in Explorer, and
//! the chosen entry runs through the shell.
//!
//! Everything here runs on the UI thread: shell extensions expect to be used on the thread that
//! created them, and that thread has to pump messages for the dialogs they open.

use std::ffi::c_void;
use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Frame, RgbaImage};
use windows::core::{Interface, HSTRING, PCSTR, PCWSTR, PSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODA_DRAWENTIRE, ODS_DEFAULT, ODT_MENU};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_SFUIObject, DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3,
    ILCreateFromPathW, ILFree, RemoveWindowSubclass, SHCreateShellItemArrayFromIDLists,
    SHGetDesktopFolder, SetWindowSubclass, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL,
    CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DeleteMenu, DestroyMenu, GetCursorPos, GetMenuItemCount, GetMenuItemInfoW,
    TrackPopupMenuEx, HMENU, MENUITEMINFOW, MFS_CHECKED, MFS_DEFAULT, MFS_DISABLED, MFT_OWNERDRAW,
    MFT_SEPARATOR, MF_BYPOSITION, MIIM_BITMAP, MIIM_DATA, MIIM_FTYPE, MIIM_ID, MIIM_STATE,
    MIIM_STRING, MIIM_SUBMENU, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM,
    WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

/// Command ids start here, so 0 can mean "none"
const FIRST_ID: u32 = 1;
const LAST_ID: u32 = 0x7FFF;
/// The verbs the shell menu offers that only work inside Explorer's own window
const HIDDEN_VERBS: [&str; 1] = ["rename"];
/// `CMIC_MASK_UNICODE`, missing from the bindings
const CMIC_MASK_UNICODE: u32 = 0x4000;
/// Size of the icons the menu draws itself
const ICON_SIZE: i32 = 16;

/// An entry of the menu.
pub enum ShellItem {
    Separator,
    Command {
        id: u32,
        label: String,
        icon: Option<Arc<RenderImage>>,
        disabled: bool,
        checked: bool,
        /// What a double click in Explorer does, shown in bold
        default: bool,
    },
    Submenu {
        label: String,
        items: Vec<ShellItem>,
    },
}

/// The menu of some entries. Holds the shell's objects until it is dropped, so a chosen entry
/// can still run.
pub struct ShellMenu {
    menu: IContextMenu,
    menu3: Option<IContextMenu3>,
    menu2: Option<IContextMenu2>,
    hmenu: HMENU,
    pub items: Vec<ShellItem>,
}

impl ShellMenu {
    /// Asks the shell for the menu of `paths`, with the extended entries like Shift+right click
    /// in Explorer if `extended`. Its entries are read into [`ShellMenu::items`] if `read`,
    /// otherwise it is only for [`ShellMenu::show_native`].
    pub fn new(paths: &[String], extended: bool, read: bool) -> windows::core::Result<Self> {
        let menu = context_menu(paths)?;
        let hmenu = unsafe { CreatePopupMenu()? };
        let mut flags = CMF_NORMAL | CMF_EXPLORE;
        if extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        let result = unsafe { menu.QueryContextMenu(hmenu, 0, FIRST_ID, LAST_ID, flags) };
        if let Err(e) = result.ok() {
            unsafe {
                let _ = DestroyMenu(hmenu);
            }
            return Err(e);
        }
        let mut shell = Self {
            menu3: menu.cast().ok(),
            menu2: menu.cast().ok(),
            menu,
            hmenu,
            items: Vec::new(),
        };
        if read {
            shell.items = shell.read(hmenu, 0);
        } else {
            shell.remove_hidden_verbs();
        }
        Ok(shell)
    }

    /// Shows the menu as Windows draws it at `point` (screen coordinates) and runs the chosen
    /// entry. Blocks until the menu closes, with a message loop of its own: the caller must not
    /// hold on to app state the window needs meanwhile.
    pub fn show_native(&self, window: HWND, point: POINT) {
        // Submenus like Send to and Open with are filled, and some entries drawn, by the shell
        // when the window passes on the menu messages it gets
        let forwarding = unsafe {
            SetWindowSubclass(
                window,
                Some(forward),
                SUBCLASS_ID,
                self as *const Self as usize,
            )
        }
        .as_bool();
        let chosen = unsafe {
            TrackPopupMenuEx(
                self.hmenu,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
                point.x,
                point.y,
                window,
                None,
            )
        }
        .0 as u32;
        if forwarding {
            unsafe {
                let _ = RemoveWindowSubclass(window, Some(forward), SUBCLASS_ID);
            }
        }
        if chosen != 0 {
            self.invoke(chosen, Some(window));
        }
    }

    /// Takes out the entries that only work in Explorer's window, see [`HIDDEN_VERBS`].
    fn remove_hidden_verbs(&self) {
        let count = unsafe { GetMenuItemCount(Some(self.hmenu)) }.max(0) as u32;
        for index in (0..count).rev() {
            let mut info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID,
                ..Default::default()
            };
            let hidden = unsafe { GetMenuItemInfoW(self.hmenu, index, true, &mut info) }.is_ok()
                && self
                    .verb(info.wID)
                    .is_some_and(|verb| HIDDEN_VERBS.contains(&verb.as_str()));
            if hidden {
                unsafe {
                    let _ = DeleteMenu(self.hmenu, index, MF_BYPOSITION);
                }
            }
        }
    }

    /// The entries of the submenu at `path`, indices of submenus from the top.
    pub fn items_at(&self, path: &[usize]) -> &[ShellItem] {
        let mut items = self.items.as_slice();
        for &index in path {
            match items.get(index) {
                Some(ShellItem::Submenu { items: inner, .. }) => items = inner,
                _ => return &[],
            }
        }
        items
    }

    /// Runs the entry with command `id`.
    pub fn invoke(&self, id: u32, window: Option<HWND>) {
        let offset = (id - FIRST_ID) as usize;
        let mut point = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut point);
        }
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
            hwnd: window.unwrap_or_default(),
            // The command as an integer resource, `MAKEINTRESOURCE`
            lpVerb: PCSTR(offset as *const u8),
            lpVerbW: PCWSTR(offset as *const u16),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: point,
            ..Default::default()
        };
        let result = unsafe {
            self.menu
                .InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO)
        };
        if let Err(e) = result {
            crate::log::write(&format!("Running the shell menu entry failed: {}", e));
        }
    }

    /// The entries of `menu`, the `depth`th level.
    fn read(&self, menu: HMENU, depth: usize) -> Vec<ShellItem> {
        let count = unsafe { GetMenuItemCount(Some(menu)) }.max(0) as u32;
        let mut items = Vec::new();
        for index in 0..count {
            let mut text = [0u16; 512];
            let mut info = MENUITEMINFOW {
                cbSize: size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_FTYPE
                    | MIIM_STATE
                    | MIIM_ID
                    | MIIM_SUBMENU
                    | MIIM_STRING
                    | MIIM_BITMAP
                    | MIIM_DATA,
                dwTypeData: windows::core::PWSTR(text.as_mut_ptr()),
                cch: text.len() as u32 - 1,
                ..Default::default()
            };
            if unsafe { GetMenuItemInfoW(menu, index, true, &mut info) }.is_err() {
                continue;
            }
            if info.fType.contains(MFT_SEPARATOR) {
                // No separators at the start, the end or twice in a row
                if !matches!(items.last(), None | Some(ShellItem::Separator)) {
                    items.push(ShellItem::Separator);
                }
                continue;
            }
            // Owner drawn entries have no text to show
            if info.fType.contains(MFT_OWNERDRAW) {
                continue;
            }
            let label = menu_label(&text[..info.cch as usize]);
            if label.is_empty() {
                continue;
            }
            if !info.hSubMenu.is_invalid() {
                // Many submenus (Send to, Open with) are filled when they open
                self.handle(
                    WM_INITMENUPOPUP,
                    WPARAM(info.hSubMenu.0 as usize),
                    LPARAM(index as isize),
                );
                let children = if depth < 4 {
                    self.read(info.hSubMenu, depth + 1)
                } else {
                    Vec::new()
                };
                if !children.is_empty() {
                    items.push(ShellItem::Submenu {
                        label,
                        items: children,
                    });
                }
                continue;
            }
            if self
                .verb(info.wID)
                .is_some_and(|verb| HIDDEN_VERBS.contains(&verb.as_str()))
            {
                continue;
            }
            let state = info.fState;
            items.push(ShellItem::Command {
                id: info.wID,
                label,
                icon: self.icon(&info, menu),
                disabled: state.contains(MFS_DISABLED),
                checked: state.contains(MFS_CHECKED),
                default: state.contains(MFS_DEFAULT),
            });
        }
        if matches!(items.last(), Some(ShellItem::Separator)) {
            items.pop();
        }
        items
    }

    /// The language independent name of a command, like `open` or `rename`.
    fn verb(&self, id: u32) -> Option<String> {
        if !(FIRST_ID..=LAST_ID).contains(&id) {
            return None;
        }
        let mut buf = [0u16; 128];
        unsafe {
            self.menu
                .GetCommandString(
                    (id - FIRST_ID) as usize,
                    GCS_VERBW,
                    None,
                    PSTR(buf.as_mut_ptr() as *mut u8),
                    buf.len() as u32,
                )
                .ok()?;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..len]).to_ascii_lowercase())
    }

    /// Passes a menu message to the extensions that draw or fill their entries themselves.
    fn handle(&self, message: u32, wparam: WPARAM, lparam: LPARAM) {
        unsafe {
            if let Some(menu) = &self.menu3 {
                let _ = menu.HandleMenuMsg2(message, wparam, lparam, None);
            } else if let Some(menu) = &self.menu2 {
                let _ = menu.HandleMenuMsg(message, wparam, lparam);
            }
        }
    }

    /// The icon of an entry: a bitmap of its own, or drawn by the extension on request.
    fn icon(&self, info: &MENUITEMINFOW, menu: HMENU) -> Option<Arc<RenderImage>> {
        let bitmap = info.hbmpItem;
        // Small values are the predefined `HBMMENU_*` images, -1 asks the owner to draw it
        let value = bitmap.0 as isize;
        if value == -1 {
            return self.draw_icon(info, menu);
        }
        if bitmap.is_invalid() || (0..=12).contains(&value) {
            return None;
        }
        let (width, height, mut bgra) = unsafe { crate::icons::read_bitmap(bitmap)? };
        opaque_if_no_alpha(&mut bgra);
        to_image(width, height, bgra)
    }

    /// Lets the extension draw the icon of an entry into a bitmap.
    fn draw_icon(&self, info: &MENUITEMINFOW, menu: HMENU) -> Option<Arc<RenderImage>> {
        unsafe {
            let dc = CreateCompatibleDC(None);
            let header = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: ICON_SIZE,
                    biHeight: -ICON_SIZE,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let Ok(bitmap) =
                CreateDIBSection(Some(dc), &header, DIB_RGB_COLORS, &mut bits, None, 0)
            else {
                let _ = DeleteDC(dc);
                return None;
            };
            let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
            let mut draw = DRAWITEMSTRUCT {
                CtlType: ODT_MENU,
                itemID: info.wID,
                itemAction: ODA_DRAWENTIRE,
                itemState: if info.fState.contains(MFS_DEFAULT) {
                    ODS_DEFAULT
                } else {
                    Default::default()
                },
                hwndItem: HWND(menu.0),
                hDC: dc,
                rcItem: RECT {
                    left: 0,
                    top: 0,
                    right: ICON_SIZE,
                    bottom: ICON_SIZE,
                },
                itemData: info.dwItemData,
                ..Default::default()
            };
            self.handle(WM_DRAWITEM, WPARAM(0), LPARAM(&mut draw as *mut _ as isize));
            let len = (ICON_SIZE * ICON_SIZE * 4) as usize;
            let mut bgra = std::slice::from_raw_parts(bits as *const u8, len).to_vec();
            SelectObject(dc, previous);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
            if bgra.iter().all(|&b| b == 0) {
                return None;
            }
            opaque_if_no_alpha(&mut bgra);
            to_image(ICON_SIZE as u32, ICON_SIZE as u32, bgra)
        }
    }
}

/// Identifies the window subclass of [`ShellMenu::show_native`]
const SUBCLASS_ID: usize = 0x5245_5645;

/// Passes the menu messages the window gets while a native menu is open to the shell.
unsafe extern "system" fn forward(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    shell: usize,
) -> LRESULT {
    // WM_DRAWITEM and WM_MEASUREITEM with a control id are not for the menu
    let for_menu = match message {
        WM_INITMENUPOPUP | WM_MENUCHAR => true,
        WM_DRAWITEM | WM_MEASUREITEM => wparam.0 == 0,
        _ => false,
    };
    if for_menu {
        let shell = unsafe { &*(shell as *const ShellMenu) };
        let mut result = LRESULT(0);
        unsafe {
            if let Some(menu) = &shell.menu3 {
                if menu
                    .HandleMenuMsg2(message, wparam, lparam, Some(&mut result))
                    .is_ok()
                {
                    return result;
                }
            } else if let Some(menu) = &shell.menu2 {
                if menu.HandleMenuMsg(message, wparam, lparam).is_ok() {
                    return LRESULT(0);
                }
            }
        }
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

impl Drop for ShellMenu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.hmenu);
        }
    }
}

/// The shell's context menu object for the entries, which may be in different folders.
fn context_menu(paths: &[String]) -> windows::core::Result<IContextMenu> {
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(paths.len());
    let result = (|| {
        for path in paths {
            let pidl = unsafe { ILCreateFromPathW(&HSTRING::from(path.as_str())) };
            if pidl.is_null() {
                return Err(windows::core::Error::new(
                    windows::Win32::Foundation::E_INVALIDARG,
                    "An entry does not exist any more",
                ));
            }
            pidls.push(pidl);
        }
        let items = pidls.iter().map(|&p| p as *const _).collect::<Vec<_>>();
        unsafe {
            SHCreateShellItemArrayFromIDLists(&items)?
                .BindToHandler::<_, IContextMenu>(None, &BHID_SFUIObject)
                // Entries of different folders, which the shell item array does not take: the
                // desktop takes absolute item ids
                .or_else(|_| {
                    SHGetDesktopFolder()?.GetUIObjectOf::<IContextMenu>(
                        HWND::default(),
                        &items,
                        None,
                    )
                })
        }
    })();
    for pidl in pidls {
        unsafe { ILFree(Some(pidl)) };
    }
    result
}

/// The text of an entry without its `&` access key and the shortcut after a tab.
fn menu_label(text: &[u16]) -> String {
    let text = String::from_utf16_lossy(text);
    let text = text.split('\t').next().unwrap_or_default();
    let mut label = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            // `&&` is a literal `&`
            if chars.peek() == Some(&'&') {
                label.push('&');
                chars.next();
            }
            continue;
        }
        label.push(c);
    }
    label.trim().to_string()
}

/// Bitmaps drawn without alpha: everything drawn becomes opaque.
fn opaque_if_no_alpha(bgra: &mut [u8]) {
    let pixels = bgra.as_chunks_mut::<4>().0;
    if pixels.iter().all(|p| p[3] == 0) {
        for p in pixels {
            if p[0] != 0 || p[1] != 0 || p[2] != 0 {
                p[3] = 255;
            }
        }
    }
}

fn to_image(width: u32, height: u32, bgra: Vec<u8>) -> Option<Arc<RenderImage>> {
    let buffer = RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new([Frame::new(buffer)])))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the real menu of entries in different folders, which needs the shell. Prints it.
    #[test]
    #[ignore]
    fn menu_of_entries_in_different_folders() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        fn print(items: &[ShellItem], depth: usize) {
            for item in items {
                match item {
                    ShellItem::Separator => println!("{}---", "  ".repeat(depth)),
                    ShellItem::Command { label, icon, .. } => {
                        println!("{}{} {}", "  ".repeat(depth), label, icon.is_some() as u8)
                    }
                    ShellItem::Submenu { label, items } => {
                        println!("{}{} >", "  ".repeat(depth), label);
                        print(items, depth + 1);
                    }
                }
            }
        }
        let paths = [
            r"C:\Windows\notepad.exe".to_string(),
            r"C:\Windows\System32\cmd.exe".to_string(),
        ];
        let same = [
            r"C:\Windows\notepad.exe".to_string(),
            r"C:\Windows\explorer.exe".to_string(),
        ];
        println!(
            "same folder: {:?}",
            ShellMenu::new(&same, false, true).map(|m| m.items.len())
        );
        print(
            &ShellMenu::new(&paths[1..], false, true).expect("One").items,
            0,
        );
        println!("=====");
        let menu = ShellMenu::new(&paths, false, true).expect("Menu");
        print(&menu.items, 0);
        assert!(!menu.items.is_empty());
    }

    #[test]
    fn labels() {
        let label = |s: &str| menu_label(&s.encode_utf16().collect::<Vec<_>>());
        assert_eq!(label("&Open"), "Open");
        assert_eq!(label("Save && close"), "Save & close");
        assert_eq!(label("Copy\tCtrl+C"), "Copy");
        assert_eq!(label("  7-Zip "), "7-Zip");
    }
}
