//! The icons Explorer shows for files, looked up by extension so no file has to be touched.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Frame, RgbaImage};
use windows::core::HSTRING;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::UI::Shell::{
    SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON, SHGFI_USEFILEATTRIBUTES,
};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

/// Extensions whose icon differs per file. Looking them up needs disk access, so they get the
/// generic icon for now.
const PER_FILE: [&str; 5] = ["exe", "ico", "lnk", "url", "cur"];

#[derive(Default)]
pub struct FileIcons {
    by_extension: HashMap<String, Option<Arc<RenderImage>>>,
    folder: Option<Option<Arc<RenderImage>>>,
}

impl FileIcons {
    pub fn get(&mut self, name: &str, directory: bool) -> Option<Arc<RenderImage>> {
        if directory {
            return self
                .folder
                .get_or_insert_with(|| load("folder", FILE_ATTRIBUTE_DIRECTORY.0))
                .clone();
        }

        let extension = name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .filter(|ext| !ext.is_empty() && ext.len() <= 16)
            .unwrap_or_default();
        if let Some(icon) = self.by_extension.get(&extension) {
            return icon.clone();
        }
        let lookup = if PER_FILE.contains(&extension.as_str()) || extension.is_empty() {
            "file".to_string()
        } else {
            format!("file.{}", extension)
        };
        let icon = load(&lookup, FILE_ATTRIBUTE_NORMAL.0);
        self.by_extension.insert(extension, icon.clone());
        icon
    }
}

/// Asks the shell for the small icon of a (nonexistent) file with the given name and attributes.
fn load(name: &str, attributes: u32) -> Option<Arc<RenderImage>> {
    let mut info = SHFILEINFOW::default();
    let ok = unsafe {
        SHGetFileInfoW(
            &HSTRING::from(name),
            windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(attributes),
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_ICON | SHGFI_SMALLICON | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if ok == 0 || info.hIcon.is_invalid() {
        return None;
    }
    let image = icon_to_image(info.hIcon);
    unsafe {
        let _ = DestroyIcon(info.hIcon);
    }
    image
}

/// Converts an icon to a BGRA image, which is what GPUI expects.
fn icon_to_image(icon: HICON) -> Option<Arc<RenderImage>> {
    let mut info = ICONINFO::default();
    unsafe { GetIconInfo(icon, &mut info).ok()? };
    let image = unsafe { read_bitmap(info.hbmColor) }.map(|(width, height, mut bgra)| {
        // Icons without an alpha channel use the mask for transparency
        if bgra.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
            let mask = unsafe { read_bitmap(info.hbmMask) };
            for (i, pixel) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let transparent = mask
                    .as_ref()
                    .is_some_and(|(_, _, m)| m.get(i * 4).is_some_and(|&b| b != 0));
                pixel[3] = if transparent { 0 } else { 255 };
            }
        }
        (width, height, bgra)
    });
    unsafe {
        let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
    }

    let (width, height, bgra) = image?;
    let buffer = RgbaImage::from_raw(width, height, bgra)?;
    Some(Arc::new(RenderImage::new([Frame::new(buffer)])))
}

/// Reads a bitmap as top-down 32 bit pixels.
unsafe fn read_bitmap(bitmap: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    if bitmap.is_invalid() {
        return None;
    }
    let mut bm = BITMAP::default();
    if GetObjectW(
        HGDIOBJ(bitmap.0),
        size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut c_void),
    ) == 0
    {
        return None;
    }
    let (width, height) = (bm.bmWidth as u32, bm.bmHeight.unsigned_abs());
    if width == 0 || height == 0 || width > 256 || height > 256 {
        return None;
    }

    let mut header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            // Negative height means top-down rows
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let dc = GetDC(None);
    let lines = GetDIBits(
        dc,
        bitmap,
        0,
        height,
        Some(pixels.as_mut_ptr() as *mut c_void),
        &mut header,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, dc);
    (lines == height as i32).then_some((width, height, pixels))
}
