//! User settings, stored as JSON in `%APPDATA%\Reverything\settings.json`. Starting with
//! Windows is not stored there but in the registry, which is the source of truth for it.

use std::path::PathBuf;

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};
use windows::core::{w, HSTRING};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: windows::core::PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: windows::core::PCWSTR = w!("Reverything");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::System => "System",
            ThemeChoice::Light => "Light",
            ThemeChoice::Dark => "Dark",
        }
    }
}

/// Global shortcut that shows the window from anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HotkeyChoice {
    None,
    CtrlAltSpace,
    WinAltF,
    WinShiftF,
    CtrlAltF,
    CtrlShiftSpace,
}

impl HotkeyChoice {
    pub const ALL: [HotkeyChoice; 6] = [
        HotkeyChoice::CtrlAltSpace,
        HotkeyChoice::WinShiftF,
        HotkeyChoice::WinAltF,
        HotkeyChoice::CtrlAltF,
        HotkeyChoice::CtrlShiftSpace,
        HotkeyChoice::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HotkeyChoice::None => "None",
            HotkeyChoice::CtrlAltSpace => "Ctrl+Alt+Space",
            HotkeyChoice::WinAltF => "Win+Alt+F",
            HotkeyChoice::WinShiftF => "Win+Shift+F",
            HotkeyChoice::CtrlAltF => "Ctrl+Alt+F",
            HotkeyChoice::CtrlShiftSpace => "Ctrl+Shift+Space",
        }
    }

    pub fn hotkey(self) -> Option<HotKey> {
        match self {
            HotkeyChoice::None => None,
            HotkeyChoice::CtrlAltSpace => Some(HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::ALT),
                Code::Space,
            )),
            HotkeyChoice::WinAltF => Some(HotKey::new(
                Some(Modifiers::SUPER | Modifiers::ALT),
                Code::KeyF,
            )),
            HotkeyChoice::WinShiftF => Some(HotKey::new(
                Some(Modifiers::SUPER | Modifiers::SHIFT),
                Code::KeyF,
            )),
            HotkeyChoice::CtrlAltF => Some(HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::ALT),
                Code::KeyF,
            )),
            HotkeyChoice::CtrlShiftSpace => Some(HotKey::new(
                Some(Modifiers::CONTROL | Modifiers::SHIFT),
                Code::Space,
            )),
        }
    }
}

/// Where the window was, in logical pixels, to open it there again.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowPlacement {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

/// A visible column of the result table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSetting {
    /// See `results::ColumnKind::key`
    pub key: String,
    pub width: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    /// `None` picks the first shortcut that no other program uses
    pub hotkey: Option<HotkeyChoice>,
    /// Closing the window keeps the app running in the tray
    pub close_to_tray: bool,
    #[serde(skip)]
    pub start_with_windows: bool,
    /// Visible result columns in display order, empty for the default columns
    pub columns: Vec<ColumnSetting>,
    /// The status popup shows every timing instead of a short summary
    pub detailed_status: bool,
    /// Size and position of the window when it was last hidden or closed
    pub window: Option<WindowPlacement>,
    /// Look for a new release once a day
    pub check_updates: bool,
    /// Unix time of the last check for a new release
    pub last_update_check: u64,
    /// A window hidden in the tray for this many minutes is closed to save memory, 0 for never
    pub close_hidden_after_mins: u64,
    /// Index changes refresh the results at most this often
    pub refresh_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            hotkey: None,
            close_to_tray: true,
            start_with_windows: false,
            columns: Vec::new(),
            detailed_status: false,
            window: None,
            check_updates: true,
            last_update_check: 0,
            close_hidden_after_mins: 10,
            refresh_secs: 10,
        }
    }
}

impl Global for Settings {}

impl Settings {
    fn path() -> Option<PathBuf> {
        std::env::var_os("APPDATA")
            .map(|dir| PathBuf::from(dir).join("Reverything").join("settings.json"))
    }

    pub fn load() -> Self {
        let mut settings = Self::path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
            .unwrap_or_default();
        settings.start_with_windows = autostart_enabled();
        settings
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Changes the settings, saves them and redraws all windows.
    pub fn update(cx: &mut App, f: impl FnOnce(&mut Settings)) {
        let mut settings = cx.global::<Settings>().clone();
        let before = settings.clone();
        f(&mut settings);
        if settings.start_with_windows != before.start_with_windows {
            set_autostart(settings.start_with_windows);
            settings.start_with_windows = autostart_enabled();
        }
        settings.save();
        cx.set_global(settings);
        cx.refresh_windows();
    }
}

fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}

/// Starts the app hidden in the tray when the user logs on.
fn set_autostart(enabled: bool) {
    unsafe {
        if !enabled {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let command = HSTRING::from(format!("\"{}\" --background", exe.display()));
        let bytes =
            std::slice::from_raw_parts(command.as_ptr() as *const u8, (command.len() + 1) * 2);
        let _ = RegSetKeyValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            RUN_VALUE,
            REG_SZ.0,
            Some(bytes.as_ptr() as *const _),
            bytes.len() as u32,
        );
    }
}
