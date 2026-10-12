//! Explorer's Open with: the apps Windows recommends for a file type, and its dialog to choose
//! another one.

use std::sync::Arc;

use gpui_kit::RenderImage;
use windows::core::{HSTRING, PWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, IAssocHandler, ILCreateFromPathW, ILFree, SHAssocEnumHandlers,
    SHCreateShellItemArrayFromIDLists, SHOpenWithDialog, ASSOC_FILTER_RECOMMENDED,
    OAIF_ALLOW_REGISTRATION, OAIF_EXEC, OAIF_REGISTER_EXT, OPENASINFO,
};

/// An app that opens the file type.
pub struct App {
    pub name: String,
    handler: IAssocHandler,
}

impl App {
    /// Its icon, from its program or package.
    pub fn icon(&self) -> Option<Arc<RenderImage>> {
        let mut path = PWSTR::null();
        let mut index = 0;
        unsafe { self.handler.GetIconLocation(&mut path, &mut index).ok()? };
        let location = take_string(path)?;
        crate::icons::load_location(&location, index)
    }

    /// Opens `paths` with the app.
    pub fn open(&self, paths: &[String]) {
        let result = data_object(paths).and_then(|data| unsafe { self.handler.Invoke(&data) });
        if let Err(e) = result {
            crate::log::write(&format!("Opening with {} failed: {}", self.name, e));
        }
    }
}

/// The apps Windows recommends for files with `extension` (without the dot), as in Explorer's
/// Open with menu.
pub fn apps(extension: &str) -> Vec<App> {
    let Ok(handlers) = (unsafe {
        SHAssocEnumHandlers(
            &HSTRING::from(format!(".{}", extension)),
            ASSOC_FILTER_RECOMMENDED,
        )
    }) else {
        return Vec::new();
    };
    let mut apps = Vec::new();
    loop {
        let mut next = [None];
        let mut fetched = 0;
        if unsafe { handlers.Next(&mut next, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let Some(handler) = next[0].take() else {
            break;
        };
        let Some(name) = unsafe { handler.GetUIName() }.ok().and_then(take_string) else {
            continue;
        };
        apps.push(App { name, handler });
    }
    apps
}

/// Windows' dialog to choose the app for `path`, which then opens it.
pub fn choose(window: Option<HWND>, path: &str) {
    let path = HSTRING::from(path);
    let info = OPENASINFO {
        pcszFile: windows::core::PCWSTR(path.as_ptr()),
        oaifInFlags: OAIF_ALLOW_REGISTRATION | OAIF_REGISTER_EXT | OAIF_EXEC,
        ..Default::default()
    };
    // Cancelling is an error too
    let _ = unsafe { SHOpenWithDialog(window, &info) };
}

/// The files as the shell passes them to apps.
fn data_object(paths: &[String]) -> windows::core::Result<IDataObject> {
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
                .BindToHandler::<_, IDataObject>(None, &BHID_DataObject)
        }
    })();
    for pidl in pidls {
        unsafe { ILFree(Some(pidl)) };
    }
    result
}

/// A string the shell allocated, freed.
fn take_string(text: PWSTR) -> Option<String> {
    if text.is_null() {
        return None;
    }
    let string = unsafe { text.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(text.0 as *const _)) };
    string.filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lists the apps for some types, which needs the shell. Prints them.
    #[test]
    #[ignore]
    fn recommended_apps() {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
        }
        for extension in ["txt", "png", "pdf", "rs", "nothing-opens-this"] {
            let start = std::time::Instant::now();
            let apps = apps(extension);
            println!("{} in {:?}", extension, start.elapsed());
            for app in &apps {
                let mut path = PWSTR::null();
                let mut index = 0;
                let _ = unsafe { app.handler.GetIconLocation(&mut path, &mut index) };
                let name = unsafe { app.handler.GetName() }.ok().and_then(take_string);
                let start = std::time::Instant::now();
                let icon = app.icon().is_some();
                println!(
                    "  {} | {:?} | {:?},{} | icon {} in {:?}",
                    app.name,
                    name,
                    take_string(path),
                    index,
                    icon,
                    start.elapsed()
                );
            }
        }
    }
}
