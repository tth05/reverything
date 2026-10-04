//! Which drives the service indexes, chosen in the settings. Changes are sent to the service
//! when the settings dialog closes, because enabling a drive starts a full scan and disabling
//! one throws its index away.

use gpui_kit::{App, Global};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumeInformationW};

#[derive(Debug, Clone)]
pub struct Drive {
    pub letter: char,
    /// Volume label, e.g. "Windows"
    pub label: String,
    pub total_bytes: u64,
}

impl Drive {
    pub fn new(letter: char) -> Self {
        let root = HSTRING::from(format!(r"{}:\", letter));
        let mut name = [0u16; 128];
        let label = unsafe {
            GetVolumeInformationW(&root, Some(&mut name), None, None, None, None)
                .map(|()| {
                    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                    String::from_utf16_lossy(&name[..len])
                })
                .unwrap_or_default()
        };
        let mut total_bytes = 0u64;
        unsafe {
            let _ = GetDiskFreeSpaceExW(&root, None, Some(&mut total_bytes), None);
        }
        Self {
            letter,
            label,
            total_bytes,
        }
    }
}

/// The drive selection while the settings dialog is open.
pub struct DriveChoice {
    pub drives: Vec<Drive>,
    /// What the service indexes
    pub indexed: Vec<char>,
    /// What the user picked
    pub selected: Vec<char>,
}

impl Global for DriveChoice {}

impl DriveChoice {
    /// Updates the listed drives, keeping what the user picked so far.
    pub fn set_drives(cx: &mut App, letters: &[char]) {
        if !cx.has_global::<DriveChoice>() {
            return;
        }
        let choice = cx.global_mut::<DriveChoice>();
        if choice
            .drives
            .iter()
            .map(|d| d.letter)
            .eq(letters.iter().copied())
        {
            return;
        }
        choice.drives = letters.iter().map(|&l| Drive::new(l)).collect();
        cx.refresh_windows();
    }

    pub fn toggle(cx: &mut App, letter: char, on: bool) {
        if cx.has_global::<DriveChoice>() {
            let choice = cx.global_mut::<DriveChoice>();
            choice.selected.retain(|&c| c != letter);
            if on {
                choice.selected.push(letter);
                choice.selected.sort();
            }
        }
        cx.refresh_windows();
    }
}
