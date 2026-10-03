//! Tray icon, global hotkey, single instance and hiding/showing the window.

use std::sync::mpsc;
use std::time::Duration;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui_kit::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
    INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW,
};

use crate::settings::{HotkeyChoice, Settings};
use crate::view::{FocusSearch, OpenSettings};

/// How often tray, hotkey and second-instance events are checked
const POLL_INTERVAL: Duration = Duration::from_millis(50);

const INSTANCE_MUTEX: windows::core::PCWSTR = w!(r"Local\Reverything.UI");
const SHOW_EVENT: windows::core::PCWSTR = w!(r"Local\Reverything.Show");

/// Makes sure only one window exists. Returns `None` in a second instance after asking the
/// first one to show its window.
pub fn single_instance() -> Option<mpsc::Receiver<()>> {
    unsafe {
        // Held for the lifetime of the process
        let _mutex = CreateMutexW(None, true, INSTANCE_MUTEX).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            for _ in 0..50 {
                if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, SHOW_EVENT) {
                    let _ = SetEvent(event);
                    let _ = CloseHandle(event);
                    break;
                }
                // The first instance may not have created the event yet
                std::thread::sleep(Duration::from_millis(20));
            }
            return None;
        }

        let (tx, rx) = mpsc::channel();
        let event = CreateEventW(None, false, false, SHOW_EVENT).ok()?;
        let event = event.0 as usize;
        std::thread::spawn(move || loop {
            WaitForSingleObject(HANDLE(event as *mut _), INFINITE);
            if tx.send(()).is_err() {
                return;
            }
        });
        Some(rx)
    }
}

/// Lives as long as the app; dropping it removes the tray icon.
pub struct Desktop {
    window: AnyWindowHandle,
    _tray: Option<TrayIcon>,
    hotkeys: Option<GlobalHotKeyManager>,
    hotkey: Option<HotKey>,
    /// The shortcut that is registered right now
    pub active_hotkey: Option<HotkeyChoice>,
    /// Why the chosen shortcut could not be registered
    pub hotkey_error: Option<String>,
    show_id: MenuId,
    settings_id: MenuId,
    quit_id: MenuId,
    second_instance: mpsc::Receiver<()>,
}

impl Global for Desktop {}

impl Desktop {
    pub fn install(window: AnyWindowHandle, second_instance: mpsc::Receiver<()>, cx: &mut App) {
        let show = MenuItem::new("Open Reverything", true, None);
        let settings = MenuItem::new("Settings", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&show, &settings, &PredefinedMenuItem::separator(), &quit]);

        let tray = match tray_icon::Icon::from_resource(1, None) {
            Ok(icon) => TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip("Reverything")
                .with_menu(Box::new(menu))
                // Left click shows the window, the menu is on the right button
                .with_menu_on_left_click(false)
                .build()
                .inspect_err(|e| {
                    crate::log::write(&format!("Failed to create the tray icon: {}", e))
                })
                .ok(),
            Err(e) => {
                crate::log::write(&format!("Failed to load the tray icon: {}", e));
                None
            }
        };
        let hotkeys = GlobalHotKeyManager::new()
            .inspect_err(|e| crate::log::write(&format!("Global hotkeys unavailable: {}", e)))
            .ok();

        let mut desktop = Desktop {
            window,
            _tray: tray,
            hotkeys,
            hotkey: None,
            active_hotkey: None,
            hotkey_error: None,
            show_id: show.id().clone(),
            settings_id: settings.id().clone(),
            quit_id: quit.id().clone(),
            second_instance,
        };
        desktop.apply_hotkey(cx.global::<Settings>().hotkey);
        cx.set_global(desktop);

        cx.spawn(async move |cx| loop {
            cx.background_executor().timer(POLL_INTERVAL).await;
            cx.update(Desktop::poll);
        })
        .detach();
    }

    /// Registers the chosen shortcut instead of the current one. Without a choice, the first
    /// one that no other program uses.
    pub fn apply_hotkey(&mut self, choice: Option<HotkeyChoice>) {
        let Some(manager) = &self.hotkeys else { return };
        if let Some(old) = self.hotkey.take() {
            let _ = manager.unregister(old);
        }
        self.active_hotkey = None;
        self.hotkey_error = None;

        let candidates = match choice {
            Some(choice) => vec![choice],
            None => HotkeyChoice::ALL
                .into_iter()
                .filter(|c| *c != HotkeyChoice::None)
                .collect(),
        };
        for candidate in candidates {
            let Some(hotkey) = candidate.hotkey() else {
                return;
            };
            match manager.register(hotkey) {
                Ok(()) => {
                    self.hotkey = Some(hotkey);
                    self.active_hotkey = Some(candidate);
                    return;
                }
                Err(e) => crate::log::write(&format!("Failed to register {}: {}", hotkey, e)),
            }
        }
        self.hotkey_error = Some(match choice {
            Some(_) => "Another program already uses this shortcut".into(),
            None => "All shortcuts are used by other programs".into(),
        });
    }

    fn poll(cx: &mut App) {
        let desktop = cx.global::<Desktop>();
        let window = desktop.window;
        let hotkey_id = desktop.hotkey.map(|h| h.id());
        let (show_id, settings_id, quit_id) = (
            desktop.show_id.clone(),
            desktop.settings_id.clone(),
            desktop.quit_id.clone(),
        );

        let mut show = desktop.second_instance.try_recv().is_ok();
        let mut toggle = false;
        let mut open_settings = false;
        let mut quit = false;

        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => toggle = true,
                TrayIconEvent::DoubleClick { .. } => show = true,
                _ => {}
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == show_id {
                show = true;
            } else if event.id == settings_id {
                show = true;
                open_settings = true;
            } else if event.id == quit_id {
                quit = true;
            }
        }
        while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
            if Some(event.id) == hotkey_id && event.state == HotKeyState::Pressed {
                toggle = true;
            }
        }

        if quit {
            cx.quit();
            return;
        }
        if !(show || toggle) {
            return;
        }
        let _ = window.update(cx, |_, window, cx| {
            if toggle && !show && is_visible(window) && window.is_window_active() {
                hide(window);
            } else {
                show_window(window, cx);
                if open_settings {
                    window.dispatch_action(Box::new(OpenSettings), cx);
                }
            }
        });
    }
}

pub fn hwnd(window: &Window) -> Option<HWND> {
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut _)),
        _ => None,
    }
}

fn is_visible(window: &Window) -> bool {
    hwnd(window).is_some_and(|h| unsafe { IsWindowVisible(h).as_bool() })
}

pub fn hide(window: &Window) {
    if let Some(h) = hwnd(window) {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// Shows, restores and focuses the window, with the search box focused.
pub fn show_window(window: &mut Window, cx: &mut App) {
    if let Some(h) = hwnd(window) {
        unsafe {
            let _ = ShowWindow(
                h,
                if IsIconic(h).as_bool() {
                    SW_RESTORE
                } else {
                    SW_SHOW
                },
            );
            let _ = SetForegroundWindow(h);
        }
    }
    window.activate_window();
    window.dispatch_action(Box::new(FocusSearch), cx);
}
