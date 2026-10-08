//! Tray icon, global hotkey, single instance and hiding/showing the window.
//!
//! Nothing here polls: tray, menu and hotkey events arrive through callbacks, a second start
//! through a Windows event, and all of them are handled when they happen. A window that stays
//! hidden for a while is closed to free its memory and opened again when it is needed.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use gpui_kit::*;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentProcess, OpenEventW, SetEvent, WaitForSingleObject,
    EVENT_MODIFY_STATE, INFINITE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowPlacement, IsIconic, IsWindowVisible, SetForegroundWindow,
    SetWindowPlacement, ShowWindow, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWMAXIMIZED,
    WINDOWPLACEMENT,
};

use crate::settings::{HotkeyChoice, Settings, WindowPlacement};
use crate::view::{FocusSearch, OpenSettings};

/// Signalled when Reverything is started a second time
pub struct SecondInstance(usize);

/// Makes sure only one instance runs. Returns `None` in a second instance after asking the
/// first one to show its window.
pub fn single_instance() -> Option<SecondInstance> {
    // A development build talking to another pipe runs next to the installed app
    let suffix = std::env::var(reverything_protocol::PIPE_ENV)
        .map(|pipe| format!(".{}", pipe.rsplit('\\').next().unwrap_or_default()))
        .unwrap_or_default();
    let mutex_name = HSTRING::from(format!(r"Local\Reverything.UI{}", suffix));
    let event_name = HSTRING::from(format!(r"Local\Reverything.Show{}", suffix));
    unsafe {
        // Held for the lifetime of the process
        let _mutex = CreateMutexW(None, true, &mutex_name).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            for _ in 0..50 {
                if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, &event_name) {
                    let _ = SetEvent(event);
                    let _ = CloseHandle(event);
                    break;
                }
                // The first instance may not have created the event yet
                std::thread::sleep(Duration::from_millis(20));
            }
            return None;
        }
        let event = CreateEventW(None, false, false, &event_name).ok()?;
        Some(SecondInstance(event.0 as usize))
    }
}

#[derive(Debug, Clone, Copy)]
enum Event {
    Show,
    Toggle,
    OpenSettings,
    Quit,
}

/// Opens the main window, shown and focused.
pub type OpenWindow = fn(cx: &mut App) -> AnyWindowHandle;

/// Lives as long as the app; dropping it removes the tray icon.
pub struct Desktop {
    /// `None` while the window is closed
    window: Option<AnyWindowHandle>,
    open_window: OpenWindow,
    /// Closes the window after it was hidden for a while, dropped when it is shown again
    close_hidden: Option<Task<()>>,
    _tray: Option<TrayIcon>,
    hotkeys: Option<GlobalHotKeyManager>,
    hotkey: Option<HotKey>,
    /// Id of the registered hotkey, read by the hotkey callback
    hotkey_id: Arc<AtomicU32>,
    /// The shortcut that is registered right now
    pub active_hotkey: Option<HotkeyChoice>,
    /// Why the chosen shortcut could not be registered
    pub hotkey_error: Option<String>,
}

impl Global for Desktop {}

impl Desktop {
    /// `window` is `None` when the app started hidden without opening it.
    pub fn install(
        window: Option<AnyWindowHandle>,
        open_window: OpenWindow,
        second_instance: SecondInstance,
        cx: &mut App,
    ) {
        let (events, received) = async_channel::unbounded::<Event>();

        let show = MenuItem::new("Open Reverything", true, None);
        let settings = MenuItem::new("Settings", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&show, &settings, &PredefinedMenuItem::separator(), &quit]);
        let (show_id, settings_id, quit_id) =
            (show.id().clone(), settings.id().clone(), quit.id().clone());
        MenuEvent::set_event_handler(Some({
            let events = events.clone();
            move |event: MenuEvent| {
                let event = if event.id == show_id {
                    Event::Show
                } else if event.id == settings_id {
                    Event::OpenSettings
                } else if event.id == quit_id {
                    Event::Quit
                } else {
                    return;
                };
                let _ = events.try_send(event);
            }
        }));
        TrayIconEvent::set_event_handler(Some({
            let events = events.clone();
            move |event: TrayIconEvent| match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => {
                    let _ = events.try_send(Event::Toggle);
                }
                TrayIconEvent::DoubleClick { .. } => {
                    let _ = events.try_send(Event::Show);
                }
                _ => {}
            }
        }));
        let hotkey_id = Arc::new(AtomicU32::new(0));
        GlobalHotKeyEvent::set_event_handler(Some({
            let events = events.clone();
            let hotkey_id = hotkey_id.clone();
            move |event: GlobalHotKeyEvent| {
                if event.id == hotkey_id.load(Ordering::Relaxed)
                    && event.state == HotKeyState::Pressed
                {
                    let _ = events.try_send(Event::Toggle);
                }
            }
        }));
        let SecondInstance(event) = second_instance;
        std::thread::spawn(move || loop {
            unsafe { WaitForSingleObject(HANDLE(event as *mut _), INFINITE) };
            if events.send_blocking(Event::Show).is_err() {
                return;
            }
        });

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
            open_window,
            close_hidden: None,
            _tray: tray,
            hotkeys,
            hotkey: None,
            hotkey_id,
            active_hotkey: None,
            hotkey_error: None,
        };
        desktop.apply_hotkey(cx.global::<Settings>().hotkey);
        cx.set_global(desktop);

        cx.spawn(async move |cx| {
            while let Ok(event) = received.recv().await {
                cx.update(|cx| Desktop::handle(event, cx));
            }
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
        self.hotkey_id.store(0, Ordering::Relaxed);
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
                    self.hotkey_id.store(hotkey.id(), Ordering::Relaxed);
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

    fn handle(event: Event, cx: &mut App) {
        if let Event::Quit = event {
            if let Some(window) = cx.global::<Desktop>().window {
                let _ = window.update(cx, |_, window, cx| remember_bounds(window, cx));
            }
            cx.quit();
            return;
        }
        let window = cx.global::<Desktop>().window;
        let Some(window) = window else {
            // Closed while hidden, open it again
            let open = cx.global::<Desktop>().open_window;
            let window = open(cx);
            let desktop = cx.global_mut::<Desktop>();
            desktop.window = Some(window);
            desktop.close_hidden = None;
            if let Event::OpenSettings = event {
                let _ = window.update(cx, |_, window, cx| {
                    window.dispatch_action(Box::new(OpenSettings), cx)
                });
            }
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            let toggle = matches!(event, Event::Toggle);
            if toggle && is_visible(window) && window.is_window_active() {
                hide(window, cx);
            } else {
                show_window(window, cx);
                if let Event::OpenSettings = event {
                    window.dispatch_action(Box::new(OpenSettings), cx);
                }
            }
        });
    }

    /// Closes the window if it is still hidden, freeing its memory. The tray icon and hotkey
    /// open it again.
    fn close_hidden_window(cx: &mut App) {
        let Some(window) = cx.global::<Desktop>().window else {
            return;
        };
        let closed = window
            .update(cx, |_, window, _| {
                if is_visible(window) {
                    return false;
                }
                window.remove_window();
                true
            })
            .unwrap_or(true);
        if closed {
            crate::log::write("Closed the hidden window");
            let desktop = cx.global_mut::<Desktop>();
            desktop.window = None;
            desktop.close_hidden = None;
            // The renderer keeps its caches, at least they do not need to stay in memory
            unsafe {
                let _ = K32EmptyWorkingSet(GetCurrentProcess());
            }
        }
    }
}

/// Centers the window on the monitor with the mouse, unless it is already on it. Keeps its
/// size and maximized state.
fn move_to_cursor_monitor(hwnd: HWND) {
    unsafe {
        let mut cursor = POINT::default();
        if GetCursorPos(&mut cursor).is_err() {
            return;
        }
        let target = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        if target == MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) {
            return;
        }
        let primary = MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY);
        let (Some(to), Some(primary)) = (monitor_info(target), monitor_info(primary)) else {
            return;
        };
        let mut placement = WINDOWPLACEMENT {
            length: size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        if GetWindowPlacement(hwnd, &mut placement).is_err() {
            return;
        }
        // The placement is in workspace coordinates, which are offset from screen coordinates
        // by a taskbar on the left or top of the primary monitor
        let dx = primary.rcWork.left - primary.rcMonitor.left;
        let dy = primary.rcWork.top - primary.rcMonitor.top;
        let r = placement.rcNormalPosition;
        let work = to.rcWork;
        let width = (r.right - r.left).min(work.right - work.left);
        let height = (r.bottom - r.top).min(work.bottom - work.top);
        let left = work.left + (work.right - work.left - width) / 2 - dx;
        let top = work.top + (work.bottom - work.top - height) / 2 - dy;
        placement.rcNormalPosition = RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        };
        let _ = SetWindowPlacement(hwnd, &placement);
    }
}

fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFO> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some(info)
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

/// Saves the window's size and position, to open it there the next time.
pub fn remember_bounds(window: &Window, cx: &mut App) {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => (bounds, false),
        WindowBounds::Maximized(bounds) | WindowBounds::Fullscreen(bounds) => (bounds, true),
    };
    let placement = WindowPlacement {
        x: bounds.origin.x.into(),
        y: bounds.origin.y.into(),
        width: bounds.size.width.into(),
        height: bounds.size.height.into(),
        maximized,
    };
    if cx.global::<Settings>().window != Some(placement) {
        Settings::update(cx, |s| s.window = Some(placement));
    }
}

/// Hides the window to the tray. If it stays hidden for as long as the settings say, it is
/// closed, and opened again when needed.
pub fn hide(window: &Window, cx: &mut App) {
    remember_bounds(window, cx);
    if let Some(h) = hwnd(window) {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
    let minutes = cx.global::<Settings>().close_hidden_after_mins;
    if cx.has_global::<Desktop>() && minutes > 0 {
        let task = cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_secs(minutes * 60))
                .await;
            cx.update(Desktop::close_hidden_window);
        });
        cx.global_mut::<Desktop>().close_hidden = Some(task);
    }
}

/// Shows, restores and focuses the window, with the search box focused. It moves to the
/// monitor with the mouse first.
pub fn show_window(window: &mut Window, cx: &mut App) {
    present(window, cx, false);
}

/// [`show_window`], maximized if `maximize`.
pub fn present(window: &mut Window, cx: &mut App, maximize: bool) {
    if cx.has_global::<Desktop>() {
        cx.global_mut::<Desktop>().close_hidden = None;
    }
    if let Some(h) = hwnd(window) {
        move_to_cursor_monitor(h);
        unsafe {
            let _ = ShowWindow(
                h,
                if maximize {
                    SW_SHOWMAXIMIZED
                } else if IsIconic(h).as_bool() {
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
