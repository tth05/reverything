//! The search window: search box, result table and a status bar with the service health.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme, Disableable, Icon, IconName, Selectable, Sizable, StyledExt,
    TitleBar, WindowExt,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use reverything_protocol::{
    Request, Response, SavedIndex, ScanTimings, Status, VolumeState, VolumeStatus,
};

use crate::client::ServiceClient;
use crate::desktop::{self, Desktop};
use crate::drives::{Drive, DriveChoice};
use crate::format;
use crate::menu::{Entry, Menu, MenuIcon};
use crate::results::{MenuTarget, NameHighlight, Results, Show};
use crate::settings::{ExplorerMenu, MenuStyle, Settings, ThemeChoice};
use crate::shell;
use crate::shell_menu::{ShellItem, ShellMenu};
use crate::update;

gpui_kit::actions!(
    reverything,
    [
        OpenSelected,
        RevealSelected,
        CopyFile,
        CopyPath,
        CopyName,
        ShowProperties,
        DeleteSelected,
        DeletePermanently,
        SearchInFolder,
        ShowContextMenu,
        SelectAll,
        FocusSearch,
        HideWindow,
        OpenSettings,
        OpenAbout,
        CycleShow,
        ToggleFiles,
        ToggleFolders,
    ]
);

pub const KEY_CONTEXT: &str = "Reverything";

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-enter", RevealSelected, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-c", CopyFile, Some(KEY_CONTEXT)),
        // Also in the search box, which usually has the focus. Text selected there is still
        // copied as text, see `on_copy_file`.
        KeyBinding::new("ctrl-c", CopyFile, Some("Reverything > Input")),
        KeyBinding::new("ctrl-shift-c", CopyPath, Some(KEY_CONTEXT)),
        KeyBinding::new("alt-enter", ShowProperties, Some(KEY_CONTEXT)),
        // The search box has its own Delete and Ctrl+A, these work in the results
        KeyBinding::new("delete", DeleteSelected, Some(KEY_CONTEXT)),
        KeyBinding::new("shift-delete", DeletePermanently, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-a", SelectAll, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-f", FocusSearch, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-l", FocusSearch, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-,", OpenSettings, Some(KEY_CONTEXT)),
        KeyBinding::new("alt-f", ToggleFiles, Some(KEY_CONTEXT)),
        KeyBinding::new("alt-d", ToggleFolders, Some(KEY_CONTEXT)),
        KeyBinding::new("escape", HideWindow, Some(KEY_CONTEXT)),
    ]
}

/// Applies the theme setting.
pub fn apply_theme(window: Option<&mut Window>, cx: &mut App) {
    use gpui_kit::component::{Theme, ThemeMode};
    match cx.global::<Settings>().theme {
        ThemeChoice::System => Theme::sync_system_appearance(window, cx),
        ThemeChoice::Light => Theme::change(ThemeMode::Light, window, cx),
        ThemeChoice::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
    let theme = Theme::global_mut(cx);
    // Focused inputs only get a colored border instead of an extra ring around them
    theme.focus_ring = false;
    // The dark theme's table header text is barely readable, use the muted text color like the
    // light theme does
    theme.table_head_foreground = theme.muted_foreground;
    // Selected results are highlighted on their name only, like in Explorer: the table's
    // highlight of its selected row becomes invisible, its color goes to the names. A theme that
    // was not reset still has it invisible, then the color from before stays.
    // Hovering a row shows nothing
    theme.table_hover = transparent_black();
    theme.tokens.table_hover = transparent_black().into();
    let table_active = theme.tokens.table_active.color;
    theme.list.active_highlight = true;
    theme.table_active = transparent_black();
    theme.tokens.table_active = transparent_black().into();
    if table_active.a > 0. {
        cx.set_global(NameHighlight(table_active));
    } else if !cx.has_global::<NameHighlight>() {
        let accent = cx.theme().accent;
        cx.set_global(NameHighlight(accent));
    }
}

/// Progress of looking for and installing an update
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateState {
    Idle,
    Checking,
    /// A check asked for in the About dialog found nothing
    UpToDate,
    /// A check asked for in the About dialog failed
    CheckFailed(String),
    Installing,
    InstallFailed(String),
}

/// The app icon, shown in the title bar
static APP_ICON: &[u8] = include_bytes!("../../../assets/reverything.png");

/// How often the service status is polled while the window is active. Nothing is polled while
/// it is not.
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// After the window became active, index changes refresh the results right away for this long,
/// while the service catches up with what happened in the meantime
const ACTIVATION_REFRESH: Duration = Duration::from_secs(5);
/// Lower than the default title bar (34px)
const TITLE_BAR_HEIGHT: Pixels = px(30.);

pub struct MainView {
    input: Entity<InputState>,
    table: Entity<TableState<Results>>,
    client: Arc<ServiceClient>,
    status: Option<Status>,
    status_error: Option<String>,
    status_round_trip: Option<Duration>,
    /// Index generation the current results were searched at
    searched_generation: Option<u64>,
    /// Number of searchable volumes when the current results were searched
    searched_volumes: usize,
    last_search: Instant,
    /// The settings page is shown instead of the results, the drive selection is applied when
    /// it closes
    settings_open: bool,
    settings_focus: FocusHandle,
    /// The window has the focus, as last told to the service
    active: bool,
    activated_at: Instant,
    /// The status polling loop runs
    polling: bool,
    /// Changing the indexed drives failed
    drive_error: Option<String>,
    /// Changing a setting of the service failed
    settings_error: Option<String>,
    /// Takes the keys while the global shortcut is being recorded
    recording_shortcut: Option<Subscription>,
    /// Why the recorded shortcut was not taken
    shortcut_error: Option<String>,
    time_fields: TimeFields,
    /// Where the results area is, for drawing the selection rectangle
    results_bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Where the mouse is while a selection rectangle is dragged
    marquee_pointer: Option<Point<Pixels>>,
    /// Scrolls while the selection rectangle is dragged past the top or bottom of the list
    autoscroll: Option<Task<()>>,
    /// The context menu while it is open, where it opened, and its closing subscription
    context_menu: Option<(Entity<Menu>, Point<Pixels>, Subscription)>,
    /// A newer release, offered at the bottom left
    update: Option<update::Update>,
    /// What the update is doing, shown at the bottom left
    update_state: UpdateState,
    started: Instant,
    first_frame: Option<Duration>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(started: Instant, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The window opens focused. Being active makes the service load the index right away.
        let client = Arc::new(ServiceClient::new(true));
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(
                "Search, e.g. report, *.pdf, photos\\2024, +C:\\code\\, !node_modules\\",
            )
        });
        let columns = cx.global::<Settings>().columns.clone();
        let table = cx.new(|cx| {
            TableState::new(Results::new(client.clone(), &columns), window, cx)
                .row_selectable(true)
                .col_resizable(true)
                .col_movable(true)
                .sortable(true)
        });
        // The results need the scroll position for the selection rectangle
        table.update(cx, |table, _| {
            let scroll = table.vertical_scroll_handle.clone();
            table.delegate_mut().scroll = Some(scroll);
        });

        let time_fields = TimeFields::new(window, cx);
        let mut subscriptions = time_fields
            .all()
            .map(|field| {
                cx.subscribe_in(field, window, |view, field, event, window, cx| {
                    if let InputEvent::Change = event {
                        // Whole numbers only, anything else typed or pasted is dropped
                        let value = field.read(cx).value();
                        if !value.bytes().all(|b| b.is_ascii_digit()) || value.len() > 4 {
                            let digits = value
                                .chars()
                                .filter(char::is_ascii_digit)
                                .take(4)
                                .collect::<String>();
                            field.update(cx, |field, cx| field.set_value(digits, window, cx));
                        }
                        view.apply_time_fields(cx);
                    }
                })
            })
            .collect::<Vec<_>>();
        subscriptions.extend([
            cx.subscribe_in(&input, window, |view, _, event, _, cx| match event {
                InputEvent::Change => view.search(true, cx),
                InputEvent::PressEnter { secondary, .. } => {
                    let paths = view.target_paths(cx);
                    if *secondary {
                        if let [path] = paths.as_slice() {
                            shell::reveal(path);
                        }
                    } else {
                        paths.iter().for_each(|path| shell::open(path));
                    }
                }
                _ => {}
            }),
            cx.subscribe_in(&table, window, |view, table, event, _, cx| match event {
                // Not for a double click next to the name
                TableEvent::DoubleClickedRow(row)
                    if table.read(cx).delegate().pressed_empty != Some(*row) =>
                {
                    view.open_row(*row, cx)
                }
                // The table selects one row, the results keep the multi-selection
                TableEvent::SelectRow(row) => table.update(cx, |table, cx| {
                    // A click next to the name leaves the selection to the rectangle
                    if table.delegate_mut().empty_select.take() == Some(*row) {
                        return;
                    }
                    if !table.delegate_mut().table_selected(*row) {
                        // Ctrl+click took it out of the selection
                        table.delegate_mut().keep_selection = true;
                        table.clear_selection(cx);
                    }
                    cx.notify();
                }),
                TableEvent::ClearSelection => table.update(cx, |table, cx| {
                    let results = table.delegate_mut();
                    if !std::mem::take(&mut results.keep_selection) {
                        results.selection.clear();
                    }
                    cx.notify();
                }),
                TableEvent::ColumnWidthsChanged(widths) => table.update(cx, |table, cx| {
                    table.delegate_mut().set_widths(widths);
                    table.delegate().save_columns(cx);
                }),
                _ => {}
            }),
        ]);

        input.update(cx, |input, cx| input.focus(window, cx));

        let mut view = Self {
            input,
            table,
            client,
            status: None,
            status_error: None,
            status_round_trip: None,
            searched_generation: None,
            searched_volumes: 0,
            last_search: Instant::now(),
            settings_open: false,
            settings_focus: cx.focus_handle(),
            active: true,
            activated_at: Instant::now(),
            polling: false,
            drive_error: None,
            settings_error: None,
            recording_shortcut: None,
            shortcut_error: None,
            time_fields,
            results_bounds: Rc::default(),
            marquee_pointer: None,
            autoscroll: None,
            context_menu: None,
            update: None,
            update_state: UpdateState::Idle,
            started,
            first_frame: None,
            _subscriptions: subscriptions,
        };
        cx.observe_window_activation(window, Self::on_activation)
            .detach();
        view.search(true, cx);
        view.poll_status(window, cx);
        view.run_script(window, cx);
        view
    }

    /// For measuring: `RV_UI_SCRIPT=d||e||` puts each `|` separated text into the search box,
    /// one per second, as if typed
    fn run_script(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(script) = std::env::var("RV_UI_SCRIPT") else {
            return;
        };
        let steps = script.split('|').map(str::to_string).collect::<Vec<_>>();
        cx.spawn_in(window, async move |view, cx| {
            for step in steps {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let _ = view.update_in(cx, |view, window, cx| {
                    view.input
                        .update(cx, |input, cx| input.set_value(step, window, cx));
                    view.search(true, cx);
                });
            }
        })
        .detach();
    }

    fn search(&mut self, scroll_to_top: bool, cx: &mut Context<Self>) {
        let query = self.input.read(cx).value().to_string();
        self.last_search = Instant::now();
        self.searched_generation = self.status.as_ref().map(|s| s.generation);
        self.searched_volumes = self.status.as_ref().map_or(0, searchable_volumes);
        self.table.update(cx, |table, cx| {
            table.delegate_mut().search(query, scroll_to_top, cx)
        });
    }

    fn open_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let path = self
            .table
            .read(cx)
            .delegate()
            .row(row)
            .map(shell::full_path);
        if let Some(path) = path {
            shell::open(&path);
        }
    }

    /// The entries actions apply to: the selection, or the entry of the open context menu if
    /// it is not selected, or without a selection the first result.
    fn target_paths(&mut self, cx: &mut Context<Self>) -> Vec<String> {
        self.table.update(cx, |table, _| {
            let first = table.selected_row().unwrap_or(0);
            let results = table.delegate_mut();
            if let Some(row) = results.menu_row.take() {
                let Some(path) = results.row(row).map(shell::full_path) else {
                    return Vec::new();
                };
                if results.selection.contains(&path) {
                    return results.selection.paths.clone();
                }
                return vec![path];
            }
            if !results.selection.paths.is_empty() {
                return results.selection.paths.clone();
            }
            results
                .row(first)
                .map(shell::full_path)
                .into_iter()
                .collect()
        })
    }

    fn on_open(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        for path in self.target_paths(cx) {
            shell::open(&path);
        }
    }

    /// Only for a single entry, like the properties.
    fn on_reveal(&mut self, _: &RevealSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let [path] = self.target_paths(cx).as_slice() {
            shell::reveal(path);
        }
    }

    /// Puts the folder of the entry in front of the search as `+"folder"\`, replacing one put
    /// there before.
    fn on_search_in_folder(
        &mut self,
        _: &SearchInFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let entry = self.table.update(cx, |table, _| {
            let row = table.delegate_mut().menu_row.take();
            let row = row.or_else(|| table.selected_row()).unwrap_or(0);
            let row = table.delegate().row(row)?;
            Some((shell::full_path(row), row.directory))
        });
        let Some((path, directory)) = entry else {
            return;
        };
        let folder = if directory {
            Some(path)
        } else {
            shell::parent(&path)
        };
        let Some(folder) = folder else {
            return;
        };
        let query = self.input.read(cx).value().to_string();
        let query = with_folder(&query, folder.trim_end_matches('\\'));
        // Undoable like typing, and searches through the input's change event
        self.input.update(cx, |input, cx| {
            input.replace_all(query, window, cx);
            input.focus(window, cx);
        });
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.table.update(cx, |table, cx| {
            // The table's own range, it is up to date also before any scrolling
            let visible = table.visible_range().rows().clone();
            table.delegate_mut().select_visible(visible);
            cx.notify();
        });
    }

    /// Copies the entry as a file, like Explorer. In the search box with text selected, the
    /// text is copied instead.
    fn on_copy_file(&mut self, _: &CopyFile, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        let range = input.selected_range();
        if input.focus_handle(cx).is_focused(window) && !range.is_empty() {
            let text = input.value().get(range).unwrap_or_default().to_string();
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            return;
        }
        let paths = self.target_paths(cx);
        if !paths.is_empty() {
            shell::copy_to_clipboard(&paths);
        }
    }

    fn on_copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        let paths = self.target_paths(cx);
        if !paths.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(paths.join("\r\n")));
        }
    }

    fn on_copy_name(&mut self, _: &CopyName, _: &mut Window, cx: &mut Context<Self>) {
        let names = self
            .target_paths(cx)
            .iter()
            .filter_map(|path| path.rsplit('\\').next().map(str::to_string))
            .collect::<Vec<_>>();
        if !names.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(names.join("\r\n")));
        }
    }

    fn on_properties(&mut self, _: &ShowProperties, _: &mut Window, cx: &mut Context<Self>) {
        if let [path] = self.target_paths(cx).as_slice() {
            shell::properties(path);
        }
    }

    fn on_delete(&mut self, _: &DeleteSelected, window: &mut Window, cx: &mut Context<Self>) {
        self.delete(false, window, cx);
    }

    fn on_delete_permanently(
        &mut self,
        _: &DeletePermanently,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.delete(true, window, cx);
    }

    /// Deletes the entries, to the Recycle Bin unless `permanently`, then refreshes the results.
    fn delete(&mut self, permanently: bool, window: &mut Window, cx: &mut Context<Self>) {
        let paths = self.target_paths(cx);
        if paths.is_empty() {
            return;
        }
        let hwnd = desktop::hwnd(window).map(|h| h.0 as usize);
        // The confirmation dialog is modal, the window keeps drawing meanwhile
        let task = cx.background_executor().spawn(async move {
            let hwnd = hwnd.map(|h| windows::Win32::Foundation::HWND(h as *mut _));
            shell::delete(&paths, permanently, hwnd)
        });
        cx.spawn(async move |view, cx| {
            if task.await {
                // Give the service a moment to see the change in the journal
                cx.background_executor()
                    .timer(Duration::from_millis(300))
                    .await;
                let _ = view.update(cx, |view, cx| view.search(false, cx));
            }
        })
        .detach();
    }

    /// Focuses the search box with its text selected, so typing replaces the last search. Also
    /// used when the window is shown again.
    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
    }

    fn on_hide(&mut self, _: &HideWindow, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        } else if self.context_menu.is_some() {
            self.close_context_menu(window, cx);
        } else if self.settings_open {
            self.close_settings(window, cx);
        } else if cx.global::<Settings>().close_to_tray {
            desktop::hide(window, cx);
        }
    }

    fn set_show(&mut self, show: impl FnOnce(Show) -> Show, cx: &mut Context<Self>) {
        self.table.update(cx, |table, _| {
            let results = table.delegate_mut();
            results.show = show(results.show);
        });
        self.search(true, cx);
    }

    fn on_cycle_show(&mut self, _: &CycleShow, _: &mut Window, cx: &mut Context<Self>) {
        self.set_show(Show::next, cx);
    }

    fn on_toggle_files(&mut self, _: &ToggleFiles, _: &mut Window, cx: &mut Context<Self>) {
        self.set_show(|show| show.toggle(Show::Files), cx);
    }

    fn on_toggle_folders(&mut self, _: &ToggleFolders, _: &mut Window, cx: &mut Context<Self>) {
        self.set_show(|show| show.toggle(Show::Folders), cx);
    }

    /// Back to the results, applying the drive selection.
    fn close_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = false;
        self.recording_shortcut = None;
        self.shortcut_error = None;
        self.apply_drive_choice(cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        if self.settings_open {
            self.close_settings(window, cx);
            return;
        }
        let volumes = self
            .status
            .as_ref()
            .map(|s| s.volumes.as_slice())
            .unwrap_or_default();
        let indexed = volumes
            .iter()
            .filter(|v| v.state != VolumeState::Disabled)
            .map(|v| v.letter)
            .collect::<Vec<_>>();
        cx.set_global(DriveChoice {
            drives: volumes.iter().map(|v| Drive::new(v.letter)).collect(),
            selected: indexed.clone(),
            indexed,
        });
        self.settings_open = true;
        self.settings_error = None;
        self.fill_time_fields(window, cx);
        self.settings_focus.focus(window, cx);
        cx.notify();

        // Drives can appear after the service started (a new disk, an unlocked BitLocker
        // drive), the service looks again and the list updates
        let client = self.client.clone();
        let task = cx
            .background_executor()
            .spawn(async move { client.request(&Request::RefreshVolumes) });
        cx.spawn_in(window, async move |view, cx| {
            if let Ok(Response::Status(status)) = task.await {
                let letters = status.volumes.iter().map(|v| v.letter).collect::<Vec<_>>();
                let _ = view.update_in(cx, |view, window, cx| {
                    let unknown = view.status.is_none();
                    view.status = Some(status);
                    DriveChoice::set_drives(cx, &letters);
                    // Until now the service's settings were not known
                    if unknown {
                        view.fill_time_fields(window, cx);
                    }
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_open_about(&mut self, _: &OpenAbout, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        let view = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            dialog
                .title("About Reverything")
                .width(px(440.))
                .child(about_panel(view.clone(), cx))
        });
    }

    /// Sends the drive selection of the settings page to the service, if it changed.
    fn apply_drive_choice(&mut self, cx: &mut Context<Self>) {
        let Some(choice) = cx.try_global::<DriveChoice>() else {
            return;
        };
        if choice.selected == choice.indexed {
            return;
        }
        let request = Request::SetVolumes {
            volumes: choice.selected.clone(),
        };
        let client = self.client.clone();
        let task = cx
            .background_executor()
            .spawn(async move { client.request(&request) });
        cx.spawn(async move |view, cx| {
            let response = task.await;
            let _ = view.update(cx, |view, cx| {
                view.drive_error = match response {
                    Ok(Response::Done) => None,
                    Ok(Response::Error(e)) => Some(e),
                    Ok(other) => Some(format!("Unexpected response {:?}", other)),
                    Err(e) => Some(e),
                };
                // Disabled drives disappear from the results right away
                view.search(false, cx);
            });
        })
        .detach();
    }

    /// Tells the service when the window gets or loses the focus, and polls the status only
    /// while it has it.
    fn on_activation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let active = window.is_window_active();
        if active == self.active {
            return;
        }
        self.active = active;
        let client = self.client.clone();
        cx.background_executor()
            .spawn(async move {
                let _ = client.set_active(active);
            })
            .detach();
        if active {
            self.activated_at = Instant::now();
            self.poll_status(window, cx);
            self.check_for_update(false, cx);
        }
    }

    /// Looks for a newer release, at most once a day unless `now`. Never in development builds
    /// or when turned off.
    pub fn check_for_update(&mut self, now: bool, cx: &mut Context<Self>) {
        let settings = cx.global::<Settings>();
        let due = update::now().saturating_sub(settings.last_update_check)
            >= update::CHECK_INTERVAL.as_secs();
        if !update::enabled() || matches!(self.update_state, UpdateState::Checking) {
            return;
        }
        if !now && (!settings.check_updates || !due) {
            return;
        }
        Settings::update(cx, |s| s.last_update_check = update::now());
        self.update_state = UpdateState::Checking;
        let task = cx.background_executor().spawn(async { update::check() });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                view.update_state = match result {
                    Ok(found) => {
                        view.update = found;
                        if view.update.is_none() && now {
                            UpdateState::UpToDate
                        } else {
                            UpdateState::Idle
                        }
                    }
                    Err(e) => {
                        crate::log::write(&format!("Checking for updates failed: {}", e));
                        if now {
                            UpdateState::CheckFailed(e)
                        } else {
                            UpdateState::Idle
                        }
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Looks for the newest release again, then downloads, verifies and runs its installer,
    /// which updates the app and the service and starts the app again.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        if matches!(self.update_state, UpdateState::Installing) {
            return;
        }
        self.update_state = UpdateState::Installing;
        cx.notify();
        let known = self.update.clone();
        let task = cx.background_executor().spawn(async move {
            let newest = update::check()?.or(known);
            match newest {
                Some(newest) => update::install(&newest),
                None => Err("No update found".into()),
            }
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                if let Err(e) = result {
                    crate::log::write(&format!("Updating failed: {}", e));
                    view.update_state = UpdateState::InstallFailed(e);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches the service status every second while the window is active, and refreshes the
    /// results when the index changed: right after the window became active and when a drive
    /// became searchable, otherwise at most as often as the settings say.
    fn poll_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.polling {
            return;
        }
        self.polling = true;
        let client = self.client.clone();
        cx.spawn_in(window, async move |view, cx| loop {
            let t = Instant::now();
            // Only this loop asks for the status, so nothing replaces the request
            let response = client.send(Request::Status).await;
            let round_trip = t.elapsed();

            let alive = view.update_in(cx, |view, window, cx| {
                match response.unwrap_or_else(|| Err("The status request was dropped".into())) {
                    Ok(Response::Status(status)) => {
                        let reconnected = view.status.is_none() && view.status_error.is_some();
                        let changed = view
                            .searched_generation
                            .is_some_and(|g| g != status.generation);
                        let new_volumes = searchable_volumes(&status) != view.searched_volumes;
                        view.searched_generation.get_or_insert(status.generation);
                        view.status = Some(status);
                        view.status_error = None;
                        view.status_round_trip = Some(round_trip);

                        // After reconnecting the results may be empty or stale. A refresh
                        // waits until the current search and its rows arrived: it would
                        // replace the result set they come from.
                        let busy = view.table.read(cx).delegate().busy();
                        let refresh = changed
                            && !busy
                            && (new_volumes
                                || view.activated_at.elapsed() < ACTIVATION_REFRESH
                                || view.last_search.elapsed() >= refresh_interval(cx));
                        if reconnected || refresh {
                            view.search(false, cx);
                        }
                    }
                    Ok(other) => {
                        view.status_error = Some(format!("Unexpected response {:?}", other))
                    }
                    Err(e) => {
                        view.status = None;
                        view.status_error = Some(e);
                    }
                }
                cx.notify();
                if !window.is_window_active() {
                    view.polling = false;
                    return false;
                }
                true
            });
            if !matches!(alive, Ok(true)) {
                return;
            }
            cx.background_executor().timer(STATUS_INTERVAL).await;
        })
        .detach();
    }

    /// Icon, name and settings on the left, the window controls on the right.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = std::sync::Arc::new(Image::from_bytes(ImageFormat::Png, APP_ICON.to_vec()));
        // As far from the edge as the search box
        TitleBar::new().h(TITLE_BAR_HEIGHT).pl_2().child(
            h_flex()
                .gap_2()
                .child(img(icon).size_4())
                .child(
                    div()
                        .text_sm()
                        .font_semibold()
                        .text_color(cx.theme().foreground)
                        .child("Reverything"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(concat!("v", env!("CARGO_PKG_VERSION"))),
                )
                .child(
                    // Occluding keeps the title bar's drag area from swallowing the click
                    h_flex()
                        .id("settings-area")
                        .occlude()
                        .child(
                            Button::new("settings")
                                .ghost()
                                .small()
                                .icon(IconName::Settings)
                                .tooltip("Settings (Ctrl+,)")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.on_open_settings(&OpenSettings, window, cx)
                                })),
                        )
                        .child(
                            Button::new("about")
                                .ghost()
                                .small()
                                .icon(IconName::Info)
                                .tooltip("About")
                                .on_click(cx.listener(|view, _, window, cx| {
                                    view.on_open_about(&OpenAbout, window, cx)
                                })),
                        ),
                ),
        )
    }

    /// Search box, the file and folder filter and the syntax help.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (icon, tooltip) = match self.table.read(cx).delegate().show {
            Show::All => (
                IconName::Asterisk,
                "Files and folders, click for only files (Alt+F, Alt+D)",
            ),
            Show::Files => (IconName::File, "Only files, click for only folders (Alt+F)"),
            Show::Folders => (
                IconName::Folder,
                "Only folders, click for files and folders (Alt+D)",
            ),
        };

        h_flex()
            .px_2()
            .py_1p5()
            .gap_1()
            .child(
                div().flex_1().child(
                    Input::new(&self.input)
                        .small()
                        .text_xs()
                        .prefix(Icon::new(IconName::Search).small())
                        .cleanable(true),
                ),
            )
            .child(
                Button::new("show")
                    .ghost()
                    .small()
                    .icon(icon)
                    .tooltip(tooltip)
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.on_cycle_show(&CycleShow, window, cx)
                    })),
            )
            .child(
                HoverCard::new("search-help")
                    .anchor(Anchor::TopRight)
                    .open_delay(Duration::from_millis(150))
                    .trigger(
                        div()
                            .id("search-help-trigger")
                            .px_1()
                            .cursor_pointer()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                Icon::new(gpui_kit::assets::IconName::CircleQuestionMark).small(),
                            ),
                    )
                    .child(search_help(cx)),
            )
    }

    /// Shown instead of the results while no drive is indexed.
    fn render_no_drives(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::HardDrive)
                    .size_10()
                    .text_color(theme.muted_foreground),
            )
            .child(
                div()
                    .text_base()
                    .font_semibold()
                    .child("No drives are indexed yet"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Choose the drives Reverything should search in the settings."),
            )
            .child(
                Button::new("open-settings")
                    .primary()
                    .small()
                    .label("Open settings")
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.on_open_settings(&OpenSettings, window, cx)
                    })),
            )
    }

    fn render_status_bar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.table.read(cx).delegate();
        let left = match (&self.drive_error, &results.error) {
            (Some(e), _) => format!("Changing the drives failed: {}", e),
            // The health indicator already says that the service is not reachable
            (None, Some(_)) if self.status.is_none() => String::new(),
            (None, Some(e)) => e.clone(),
            (None, None) => match results.selection.paths.len() {
                0 | 1 => format!("{} objects", format::group_digits(results.total() as u64)),
                selected => format!(
                    "{} objects, {} selected",
                    format::group_digits(results.total() as u64),
                    selected
                ),
            },
        };
        let update_notice = match (&self.update_state, &self.update) {
            // Only when there is something to install; up to date needs no notice
            (UpdateState::Installing, _) => Some("Downloading the update...".to_string()),
            (UpdateState::InstallFailed(e), _) => Some(format!("Update failed: {}", e)),
            (_, Some(update)) => Some(format!(
                "Reverything {} is available, click to update",
                update.version
            )),
            _ => None,
        };

        h_flex()
            .px_3()
            .py_1()
            .gap_4()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(div().flex_1().truncate().child(left))
            .children(update_notice.map(|notice| {
                let clickable =
                    self.update.is_some() && !matches!(self.update_state, UpdateState::Installing);
                div()
                    .id("update-notice")
                    .flex_shrink_0()
                    .text_color(cx.theme().primary)
                    .when(clickable, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.underline())
                            .on_click(cx.listener(|view, _, _, cx| view.install_update(cx)))
                    })
                    .child(notice)
            }))
            .child(self.render_health(window, cx))
    }

    /// Icon with a short label, showing a summary and, in detailed mode, every timing we
    /// collect on hover.
    fn render_health(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let icon = |name: IconName, color: Hsla| {
            Icon::new(name).small().text_color(color).into_any_element()
        };
        let (label, indicator): (String, AnyElement) = match (&self.status, &self.status_error) {
            (None, Some(e)) => (e.clone(), icon(IconName::CircleX, theme.danger)),
            (None, None) => (
                "Connecting".into(),
                Spinner::new().small().into_any_element(),
            ),
            (Some(status), _) => {
                let states = status
                    .volumes
                    .iter()
                    .map(|v| &v.state)
                    .filter(|s| **s != VolumeState::Disabled)
                    .collect::<Vec<_>>();
                if states.is_empty() {
                    (
                        "No drives".into(),
                        icon(IconName::Info, theme.muted_foreground),
                    )
                } else if states.iter().any(|s| matches!(s, VolumeState::Failed(_))) {
                    (
                        "Problem".into(),
                        icon(IconName::TriangleAlert, theme.warning),
                    )
                } else if states.iter().any(|s| {
                    matches!(
                        s,
                        VolumeState::Waiting
                            | VolumeState::Loading
                            | VolumeState::Indexing
                            | VolumeState::Asleep
                    )
                }) {
                    ("Indexing".into(), Spinner::new().small().into_any_element())
                } else if states.iter().any(|s| matches!(s, VolumeState::Offline)) {
                    ("Offline".into(), icon(IconName::Info, theme.info))
                } else {
                    (
                        "Up to date".into(),
                        icon(IconName::CircleCheck, theme.success),
                    )
                }
            }
        };

        // The popup can not leave the window, so it scrolls when the window is small
        let viewport = window.viewport_size();
        let max_height = (viewport.height - px(56.)).max(px(120.));
        let max_width = (viewport.width - px(32.)).max(px(200.));

        HoverCard::new("health")
            .anchor(Anchor::BottomRight)
            .open_delay(Duration::from_millis(150))
            .trigger(
                h_flex()
                    .id("health-trigger")
                    .gap_1p5()
                    .cursor_pointer()
                    .child(indicator)
                    .child(label),
            )
            .child(self.render_status_popup(max_width, max_height, cx))
    }

    fn render_status_popup(
        &self,
        max_width: Pixels,
        max_height: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let detailed = cx.global::<Settings>().detailed_status;
        let panel = if detailed {
            self.detailed_status(cx)
        } else {
            self.summary_status(cx)
        };
        let width = px(if detailed { 480. } else { 360. }).min(max_width);
        // Grows with the content up to the maximum height, then the content scrolls
        let scrolling = |content: AnyElement| {
            v_flex().w(width).max_h(max_height).child(
                v_flex().flex_1().overflow_hidden().child(
                    div().flex_1().overflow_hidden().child(
                        v_flex()
                            .id("status-details")
                            .size_full()
                            // Room for the scrollbar
                            .pr_3()
                            .overflow_y_scrollbar()
                            .child(content),
                    ),
                ),
            )
        };
        scrolling(
            v_flex()
                .gap_2()
                .child(
                    h_flex()
                        .justify_between()
                        .gap_4()
                        .child(div().text_sm().font_semibold().child("Status"))
                        .child(
                            Switch::new("detailed-status")
                                .small()
                                .checked(detailed)
                                .label("Details")
                                .on_click(|checked, _, cx| {
                                    let checked = *checked;
                                    Settings::update(cx, |s| s.detailed_status = checked)
                                }),
                        ),
                )
                .child(panel.render(cx))
                .into_any_element(),
        )
    }

    /// What a user wants to know: is everything indexed and how fast is it.
    fn summary_status(&self, cx: &mut Context<Self>) -> Panel {
        let results = self.table.read(cx).delegate();
        let mut panel = Panel::default();

        panel.section("Search");
        panel.row("Results", format::group_digits(results.total() as u64));
        if let Some(t) = results.last_search {
            panel.row("Search took", format::duration(t.service));
        }

        let Some(status) = &self.status else {
            panel.section("Service");
            panel.row(
                "State",
                self.status_error
                    .clone()
                    .unwrap_or_else(|| "Connecting".into()),
            );
            return panel;
        };

        panel.section("Drives");
        let enabled = status
            .volumes
            .iter()
            .filter(|v| v.state != VolumeState::Disabled)
            .collect::<Vec<_>>();
        if enabled.is_empty() {
            panel.row("Indexed drives", "None, choose them in the settings".into());
        }
        for v in &enabled {
            let state = match &v.state {
                VolumeState::Ready | VolumeState::Offline => format!(
                    "{} entries, {}",
                    format::group_digits(v.entries),
                    state_text(&v.state).to_lowercase()
                ),
                state => state_text(state),
            };
            panel.row(&format!("{}:", v.letter), state);
            // How long the index took to build this time
            match (&v.scan, v.load_us) {
                (Some(scan), _) => panel.row(
                    &format!("{}: indexed in", v.letter),
                    format!("{} (full scan)", format::micros(scan_total(scan))),
                ),
                (None, Some(us)) => panel.row(
                    &format!("{}: indexed in", v.letter),
                    format!("{} (saved index)", format::micros(us)),
                ),
                (None, None) => {}
            }
        }
        if let Some(at) = enabled
            .iter()
            .filter_map(|v| v.last_batch.as_ref().map(|b| b.at))
            .max()
        {
            panel.row("Last change", format::ago(at));
        }
        panel.row("Memory used", format::size(status.private_bytes));
        panel
    }

    fn detailed_status(&self, cx: &mut Context<Self>) -> Panel {
        let results = self.table.read(cx).delegate();
        let mut panel = Panel::default();

        panel.section("Search");
        match results.last_search {
            Some(t) => {
                panel.row("Search in the service", format::duration(t.service));
                panel.row("Search incl. round trip", format::duration(t.round_trip));
            }
            None => panel.row("Search", "none yet".into()),
        }
        if let Some(t) = results.last_page {
            panel.row("Last page of rows", format::duration(t));
        }
        panel.row("Results", format::group_digits(results.total() as u64));

        panel.section("App");
        if let Some(t) = self.first_frame {
            panel.row("Start to first frame", format::duration(t));
        }
        if let Some(t) = self.client.connect_time() {
            panel.row("Connecting to the service", format::duration(t));
        }
        if let Some(t) = self.status_round_trip {
            panel.row("Status round trip", format::duration(t));
        }

        match &self.status {
            Some(status) => {
                panel.section("Service");
                panel.row("Version", status.service_version.clone());
                panel.row("Uptime", format::micros(status.uptime_us));
                panel.row(
                    "Memory",
                    format!(
                        "{} private, {} working set",
                        format::size(status.private_bytes),
                        format::size(status.working_set_bytes)
                    ),
                );
                panel.row("Searches", format::group_digits(status.searches));
                for volume in &status.volumes {
                    volume_timings(&mut panel, volume);
                }
            }
            None => {
                panel.section("Service");
                panel.row(
                    "State",
                    self.status_error
                        .clone()
                        .unwrap_or_else(|| "Connecting".into()),
                );
            }
        }
        panel
    }
}

/// Volumes that can be searched
fn searchable_volumes(status: &Status) -> usize {
    status
        .volumes
        .iter()
        .filter(|v| matches!(v.state, VolumeState::Ready | VolumeState::Offline))
        .count()
}

fn state_text(state: &VolumeState) -> String {
    match state {
        VolumeState::Disabled => "Not indexed".to_string(),
        VolumeState::Waiting => "Waiting".to_string(),
        VolumeState::Loading => "Loading the saved index".to_string(),
        VolumeState::Indexing => "Reading the MFT".to_string(),
        VolumeState::Ready => "Up to date".to_string(),
        VolumeState::Asleep => "Unloaded while not in use".to_string(),
        VolumeState::Offline => "Offline, not updated".to_string(),
        VolumeState::Failed(e) => format!("Failed: {}", e),
    }
}

fn scan_total(s: &ScanTimings) -> u64 {
    s.open_us + s.read_parse_us + s.merge_us + s.sort_us + s.folder_sizes_us
}

fn volume_timings(panel: &mut Panel, v: &VolumeStatus) {
    panel.section(&format!("Volume {}:", v.letter));
    panel.row("State", state_text(&v.state));
    if v.state == VolumeState::Disabled {
        return;
    }
    panel.row("Entries", format::group_digits(v.entries));
    panel.row("Index memory", format::size(v.index_bytes));
    match &v.saved_index {
        SavedIndex::NotChecked => {}
        SavedIndex::Loaded => {
            if let Some(us) = v.load_us {
                panel.row("Loading the saved index", format::micros(us));
            }
        }
        SavedIndex::Missing => panel.row(
            "Saved index",
            if v.last_save.is_some() {
                "None at start, saved now".into()
            } else {
                "None at start".into()
            },
        ),
        SavedIndex::Discarded(e) => panel.row("Saved index not used", e.clone()),
    }
    if let Some(s) = &v.scan {
        let total = scan_total(s);
        let mb_per_s = s.bytes_read as f64 / 1048576.0 / (s.read_parse_us.max(1) as f64 / 1e6);
        panel.row("Full scan", format::micros(total));
        panel.row("  Opening the volume", format::micros(s.open_us));
        panel.row(
            "  Reading and parsing",
            format!(
                "{} ({} at {:.0} MB/s)",
                format::micros(s.read_parse_us),
                format::size(s.bytes_read),
                mb_per_s
            ),
        );
        panel.row("  Merging", format::micros(s.merge_us));
        panel.row("  Sorting", format::micros(s.sort_us));
        panel.row("  Folder sizes", format::micros(s.folder_sizes_us));
        panel.row(
            "  Records",
            format!(
                "{} plus {} hard links",
                format::group_digits(s.records),
                format::group_digits(s.links)
            ),
        );
    }
    if let Some(b) = &v.catch_up {
        panel.row(
            "Journal catch-up",
            format!(
                "{} records, {} updates in {}",
                b.records,
                b.updates,
                format::micros(b.fetch_us + b.apply_us)
            ),
        );
    }
    if let Some(b) = &v.last_batch {
        panel.row(
            "Last update",
            format!(
                "{} updates, fetch {} + apply {}, {}",
                b.updates,
                format::micros(b.fetch_us),
                format::micros(b.apply_us),
                format::ago(b.at)
            ),
        );
    }
    if v.batches > 0 {
        panel.row(
            "Updates since start",
            format!(
                "{} in {} batches",
                format::group_digits(v.records_updated),
                format::group_digits(v.batches)
            ),
        );
    }
    if let Some(s) = &v.last_save {
        panel.row(
            "Last save",
            format!(
                "{} in {}, {}",
                format::size(s.bytes),
                format::micros(s.took_us),
                format::ago(s.at)
            ),
        );
    }
}

/// Two column list of labels and values, grouped in sections.
#[derive(Default)]
struct Panel {
    items: Vec<(String, Option<String>)>,
}

impl Panel {
    fn section(&mut self, title: &str) {
        self.items.push((title.to_string(), None));
    }

    fn row(&mut self, label: &str, value: String) {
        self.items.push((label.to_string(), Some(value)));
    }

    fn render(self, cx: &App) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_0p5()
            .text_xs()
            .children(
                self.items
                    .into_iter()
                    .enumerate()
                    .map(|(i, (label, value))| match value {
                        None => div()
                            .when(i > 0, |d| d.mt_2())
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child(label)
                            .into_any_element(),
                        Some(value) => h_flex()
                            .gap_4()
                            .justify_between()
                            .items_start()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(theme.muted_foreground)
                                    .child(label),
                            )
                            // Long values (errors) wrap instead of overflowing the popup
                            .child(
                                div()
                                    .min_w_0()
                                    .text_right()
                                    .text_color(theme.foreground)
                                    .child(value),
                            )
                            .into_any_element(),
                    }),
            )
    }
}

/// Short explanation of the query syntax, shown when hovering the help icon.
fn search_help(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let examples = [
        ("report", "Names containing \"report\""),
        ("'my file'", "Names containing \"my file\", with the space"),
        ("\"readme.md\"", "Names that are exactly \"readme.md\""),
        ("*.pdf", "Files by extension (* and ? match the whole name)"),
        ("!draft", "Leave out names containing \"draft\""),
        (
            "photos\\2024",
            "\"2024\" directly in a folder matching \"photos\"",
        ),
        (
            "+C:\\code\\",
            "Only what is anywhere below matching folders",
        ),
        ("C:\\**\\temp\\", "** stands for any number of folders"),
        (
            "!node_modules\\",
            "Leave out everything below matching folders",
        ),
        ("size:>1gb", "Larger than 1 GB, also <, .. like 1mb..5mb"),
        ("dm:today", "Modified today, also thisweek, 2024, >=2024-05"),
    ];
    v_flex()
        .gap_1()
        .text_xs()
        .w(px(380.))
        .child(div().text_sm().font_semibold().child("Search syntax"))
        .children(examples.into_iter().map(|(example, meaning)| {
            h_flex()
                .gap_3()
                .child(
                    div()
                        .w(px(110.))
                        .flex_shrink_0()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.foreground)
                        .child(example),
                )
                .child(div().text_color(theme.muted_foreground).child(meaning))
        }))
        .child(div().mt_1().text_color(theme.muted_foreground).child(
            "Separate terms with spaces, every term has to match. Case does not matter. \
                     + and ! folders only look at the folders an entry is in. Best matches come \
                     first, sort by Name for plain name order.",
        ))
}

const REPOSITORY: &str = "https://github.com/tth05/reverything";

/// About dialog content: version, links, logs and checking for updates.
fn about_panel(view: WeakEntity<MainView>, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let update_state = view
        .upgrade()
        .map(|v| (v.read(cx).update_state.clone(), v.read(cx).update.clone()));
    let status = match &update_state {
        Some((UpdateState::Checking, _)) => Some("Checking...".to_string()),
        Some((UpdateState::UpToDate, _)) => Some("You have the latest version".to_string()),
        Some((UpdateState::CheckFailed(e), _)) => Some(e.clone()),
        Some((_, Some(update))) => Some(format!(
            "Version {} is available, click the notice at the bottom left",
            update.version
        )),
        _ if !update::enabled() => Some(update::disabled_reason().to_string()),
        _ => None,
    };
    // The license file next to the installed exe, the repository otherwise
    // Files next to the installed exe
    let shipped = |name: &str| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
            .filter(|path| path.exists())
            .map(|path| path.display().to_string())
    };
    let license =
        shipped("LICENSE.txt").unwrap_or_else(|| format!("{}/blob/master/LICENSE", REPOSITORY));
    // Generated when the installer is built
    let notices = shipped("THIRD-PARTY-NOTICES.html");
    let logs = std::env::var_os("LOCALAPPDATA")
        .map(|dir| std::path::PathBuf::from(dir).join("Reverything"));

    v_flex()
        .gap_4()
        .text_sm()
        .child(
            v_flex()
                .gap_1()
                .child(
                    div()
                        .font_semibold()
                        .child(concat!("Reverything ", env!("CARGO_PKG_VERSION"))),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Fast file name search for NTFS drives."),
                ),
        )
        .child(
            v_flex()
                .gap_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child("The indexing library (reverything-core) is MIT licensed.")
                .child("The app is free to use; its source code is all rights reserved."),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new("about-github")
                        .small()
                        .icon(IconName::ExternalLink)
                        .label("GitHub")
                        .on_click(|_, _, _| shell::open(REPOSITORY)),
                )
                .child(
                    Button::new("about-license")
                        .small()
                        .label("License")
                        .on_click(move |_, _, _| shell::open(&license)),
                )
                .child(
                    Button::new("about-notices")
                        .small()
                        .label("Third-party licenses")
                        .disabled(notices.is_none())
                        .on_click(move |_, _, _| {
                            if let Some(notices) = &notices {
                                shell::open(notices);
                            }
                        }),
                )
                .child(
                    Button::new("about-logs")
                        .small()
                        .icon(IconName::FolderOpen)
                        .label("Open log folder")
                        .disabled(logs.is_none())
                        .on_click(move |_, _, _| {
                            if let Some(logs) = &logs {
                                let _ = std::fs::create_dir_all(logs);
                                shell::open(&logs.display().to_string());
                            }
                        }),
                )
                .child(
                    Button::new("about-update")
                        .small()
                        .label("Check for updates")
                        .disabled(!update::enabled())
                        .on_click(move |_, _, cx| {
                            let _ = view.update(cx, |view, cx| view.check_for_update(true, cx));
                        }),
                ),
        )
        .children(status.map(|status| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(status)
        }))
}

impl Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.first_frame.is_none() {
            let elapsed = self.started.elapsed();
            self.first_frame = Some(elapsed);
            if std::env::var_os("REVERYTHING_LOG_STARTUP").is_some() {
                crate::log::write(&format!("first frame after {:?}", elapsed));
            }
        }
        let no_drives = self
            .status
            .as_ref()
            .is_some_and(|s| s.volumes.iter().all(|v| v.state == VolumeState::Disabled));

        v_flex()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_reveal))
            .on_action(cx.listener(Self::on_copy_file))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_name))
            .on_action(cx.listener(Self::on_properties))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_delete_permanently))
            .on_action(cx.listener(Self::on_search_in_folder))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_cycle_show))
            .on_action(cx.listener(Self::on_show_context_menu))
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_hide))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_open_about))
            .on_action(cx.listener(Self::on_toggle_files))
            .on_action(cx.listener(Self::on_toggle_folders))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .children(self.context_menu.as_ref().map(|(menu, position, _)| {
                deferred(
                    anchored()
                        .position(*position)
                        .snap_to_window_with_margin(px(8.))
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
            .child(self.render_title_bar(cx))
            .map(|this| {
                if self.settings_open {
                    this.child(self.render_settings(cx))
                } else {
                    this.child(self.render_toolbar(cx))
                        .map(|this| self.render_results(this, no_drives, cx))
                        .child(self.render_status_bar(window, cx))
                }
            })
    }
}

impl MainView {
    fn render_results(&self, this: Div, no_drives: bool, cx: &mut Context<Self>) -> Div {
        this.map(|this| {
            if no_drives {
                this.child(self.render_no_drives(cx))
            } else {
                this.child(
                    div()
                        .flex_1()
                        .overflow_hidden()
                        // The table opens its row menu for any right click inside it, for the
                        // last right clicked row. Forget that row before a right click
                        // reaches the table, a row sets it again, the header and empty space
                        // do not.
                        .capture_any_mouse_down(cx.listener(
                            |view, event: &MouseDownEvent, _, cx| {
                                if event.button == MouseButton::Right {
                                    view.table.update(cx, |table, cx| {
                                        table.set_right_clicked_row(None, cx)
                                    });
                                }
                            },
                        ))
                        .relative()
                        .child(
                            DataTable::new(&self.table)
                                .stripe(false)
                                .with_size(crate::results::ROW_SIZE),
                        )
                        .child(self.render_marquee(cx)),
                )
            }
        })
    }
}

impl MainView {
    /// Opens Explorer's context menu for the entries of the last context menu, where that
    /// opened.
    /// Opens the context menu for the row the table was right clicked on: the app's entries,
    /// or Explorer's if they replace them.
    fn on_show_context_menu(
        &mut self,
        _: &ShowContextMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = self.table.read(cx).delegate().menu_target.clone();
        let Some(target) = target.filter(|t| !t.paths.is_empty()) else {
            return;
        };
        if cx.global::<Settings>().explorer_menu == ExplorerMenu::Replace {
            self.show_explorer_menu(target, window, cx);
            return;
        }
        let entries = self.app_entries(&target, cx);
        self.open_menu(entries, target.position, window, cx);
    }

    /// The app's entries for the entries the menu is for.
    fn app_entries(&self, target: &MenuTarget, cx: &mut Context<Self>) -> Vec<Entry> {
        let action = |icon: MenuIcon, label: &'static str, action: Box<dyn Action>| {
            Entry::item(icon, label, move |window, cx| {
                window.dispatch_action(action.boxed_clone(), cx)
            })
        };
        // Opening folders and properties are for one entry
        let single = target.paths.len() == 1;
        let mut entries = vec![action(
            MenuIcon::Svg(IconName::ExternalLink),
            "Open",
            Box::new(OpenSelected),
        )];
        if single {
            entries.push(
                action(
                    MenuIcon::Svg(IconName::FolderOpen),
                    "Open containing folder",
                    Box::new(RevealSelected),
                )
                .shortcut("Ctrl+Enter"),
            );
        }
        entries.push(action(
            MenuIcon::Svg(IconName::Search),
            if target.directory {
                "Search in this folder"
            } else {
                "Search in the containing folder"
            },
            Box::new(SearchInFolder),
        ));
        entries.push(Entry::Separator);
        entries.push(
            action(MenuIcon::Svg(IconName::Copy), "Copy", Box::new(CopyFile)).shortcut("Ctrl+C"),
        );
        entries.push(
            action(MenuIcon::None, "Copy full path", Box::new(CopyPath)).shortcut("Ctrl+Shift+C"),
        );
        entries.push(action(MenuIcon::None, "Copy name", Box::new(CopyName)));
        entries.push(Entry::Separator);
        entries.push(
            action(
                MenuIcon::Svg(IconName::Delete),
                "Delete",
                Box::new(DeleteSelected),
            )
            .shortcut("Delete"),
        );
        entries.push(
            action(
                MenuIcon::None,
                "Delete permanently",
                Box::new(DeletePermanently),
            )
            .shortcut("Shift+Delete"),
        );
        if single {
            entries.push(
                action(
                    MenuIcon::Svg(IconName::Info),
                    "Properties",
                    Box::new(ShowProperties),
                )
                .shortcut("Alt+Enter"),
            );
        }
        if cx.global::<Settings>().explorer_menu == ExplorerMenu::MoreOptions {
            let view = cx.entity().downgrade();
            let target = target.clone();
            entries.push(Entry::Separator);
            entries.push(Entry::item(
                MenuIcon::Svg(IconName::Ellipsis),
                "More options",
                move |window, cx| {
                    let target = target.clone();
                    let _ = view.update(cx, |view, cx| view.show_explorer_menu(target, window, cx));
                },
            ));
        }
        entries
    }

    /// Opens `entries` as the context menu at `position`.
    fn open_menu(
        &mut self,
        entries: Vec<Entry>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let menu = cx.new(|cx| Menu::new(entries, cx));
        let closed = cx.subscribe_in(&menu, window, |view, _, _: &DismissEvent, window, cx| {
            view.close_context_menu(window, cx)
        });
        menu.read(cx).focus_handle(cx).focus(window, cx);
        self.context_menu = Some((menu, position, closed));
        cx.notify();
    }

    /// Closes the context menu, the focus goes back to the results it was opened from.
    fn close_context_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context_menu = None;
        self.table.update(cx, |table, cx| {
            table.set_right_clicked_row(None, cx);
            table.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    /// Opens Explorer's context menu for `target`, in the app's menu or as Windows' own.
    fn show_explorer_menu(
        &mut self,
        target: MenuTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(hwnd) = desktop::hwnd(window) else {
            return;
        };
        let MenuTarget {
            paths,
            position,
            extended,
            ..
        } = target;
        if cx.global::<Settings>().explorer_menu_style == MenuStyle::Windows {
            let scale = window.scale_factor();
            let mut point = windows::Win32::Foundation::POINT {
                x: (f32::from(position.x) * scale) as i32,
                y: (f32::from(position.y) * scale) as i32,
            };
            // Windows' menu runs a message loop of its own until it closes, the window keeps
            // drawing meanwhile. So it runs as a task of its own, not while this view is being
            // updated.
            cx.spawn(async move |_, _| {
                unsafe {
                    let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut point);
                }
                match ShellMenu::new(&paths, extended, false) {
                    Ok(shell) => shell.show_native(hwnd, point),
                    Err(e) => crate::log::write(&format!("Explorer's context menu failed: {}", e)),
                }
            })
            .detach();
            return;
        }
        let shell = match ShellMenu::new(&paths, extended, true) {
            Ok(shell) => Rc::new(shell),
            Err(e) => {
                crate::log::write(&format!("Explorer's context menu failed: {}", e));
                return;
            }
        };
        let entries = explorer_entries(&shell, Vec::new(), hwnd.0 as usize);
        self.open_menu(entries, position, window, cx);
    }

    /// The selection rectangle while it is dragged, and mouse tracking for it anywhere in the
    /// window.
    fn render_marquee(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.table.read(cx).delegate();
        let dragging = results.marquee.is_some();
        let area = results
            .scroll
            .as_ref()
            .map(|s| s.0.borrow().base_handle.bounds());
        let origin = self.results_bounds.get().origin;
        let rectangle = results
            .marquee_bounds()
            // Not over the header
            .and_then(|b| area.map(|area| b.intersect(&area)))
            .filter(|b| !b.size.width.is_zero() || !b.size.height.is_zero())
            .map(|b| {
                div()
                    .absolute()
                    .left(b.origin.x - origin.x)
                    .top(b.origin.y - origin.y)
                    .w(b.size.width)
                    .h(b.size.height)
                    .bg(cx.theme().selection.opacity(0.25))
                    .border_1()
                    .border_color(cx.theme().selection)
            });

        let bounds = self.results_bounds.clone();
        let view = cx.entity();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .child(
                canvas(
                    move |area, _, _| bounds.set(area),
                    move |_, _, window, _| {
                        if !dragging {
                            return;
                        }
                        let moved = view.clone();
                        window.on_mouse_event(move |e: &MouseMoveEvent, phase, _, cx| {
                            if phase.bubble() {
                                let pressed = e.pressed_button == Some(MouseButton::Left);
                                moved.update(cx, |view, cx| {
                                    view.marquee_moved(e.position, pressed, cx)
                                });
                            }
                        });
                        let released = view.clone();
                        window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                            if phase.bubble() && e.button == MouseButton::Left {
                                released.update(cx, |view, cx| view.end_marquee(cx));
                            }
                        });
                    },
                )
                .size_full(),
            )
            .children(rectangle)
    }

    fn marquee_moved(&mut self, position: Point<Pixels>, pressed: bool, cx: &mut Context<Self>) {
        if !pressed {
            self.end_marquee(cx);
            return;
        }
        self.marquee_pointer = Some(position);
        self.table.update(cx, |table, cx| {
            table.delegate_mut().drag_marquee(position);
            cx.notify();
        });
        if self.autoscroll.is_none() {
            self.autoscroll = Some(cx.spawn(async move |view, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let going = view
                    .update(cx, |view, cx| view.autoscroll_step(cx))
                    .unwrap_or(false);
                if !going {
                    let _ = view.update(cx, |view, _| view.autoscroll = None);
                    return;
                }
            }));
        }
        cx.notify();
    }

    /// Scrolls towards the mouse while it is above or below the list during a drag. Returns
    /// whether to keep going.
    fn autoscroll_step(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(position) = self.marquee_pointer else {
            return false;
        };
        let scrolled = self.table.update(cx, |table, cx| {
            let results = table.delegate();
            let Some(handle) = results
                .scroll
                .as_ref()
                .map(|s| s.0.borrow().base_handle.clone())
            else {
                return false;
            };
            if results.marquee.is_none() {
                return false;
            }
            let area = handle.bounds();
            let distance = if position.y < area.top() {
                position.y - area.top()
            } else if position.y > area.bottom() {
                position.y - area.bottom()
            } else {
                return true;
            };
            let mut offset = handle.offset();
            let max = handle.max_offset();
            offset.y = (offset.y - distance / 2.).clamp(-max.y, px(0.));
            handle.set_offset(offset);
            table.delegate_mut().drag_marquee(position);
            cx.notify();
            true
        });
        cx.notify();
        scrolled
    }

    fn end_marquee(&mut self, cx: &mut Context<Self>) {
        self.marquee_pointer = None;
        self.autoscroll = None;
        self.table.update(cx, |table, cx| {
            table.delegate_mut().marquee = None;
            cx.notify();
        });
        cx.notify();
    }
}

/// The entries of Explorer's menu at `path` (indices of submenus), for the app's menu.
fn explorer_entries(shell: &Rc<ShellMenu>, path: Vec<usize>, hwnd: usize) -> Vec<Entry> {
    shell
        .items_at(&path)
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            ShellItem::Separator => Entry::Separator,
            ShellItem::Submenu { label, .. } => {
                let shell = shell.clone();
                let mut path = path.clone();
                path.push(index);
                Entry::Submenu {
                    icon: MenuIcon::None,
                    label: label.clone().into(),
                    entries: Rc::new(move || explorer_entries(&shell, path.clone(), hwnd)),
                }
            }
            ShellItem::Command {
                id,
                label,
                icon,
                disabled,
                checked,
                default,
            } => {
                let (id, shell) = (*id, shell.clone());
                Entry::Item {
                    icon: icon.clone().map_or(MenuIcon::None, MenuIcon::Image),
                    label: label.clone().into(),
                    shortcut: None,
                    disabled: *disabled,
                    checked: *checked,
                    bold: *default,
                    run: Rc::new(move |_, cx| {
                        let shell = shell.clone();
                        // Entries can open dialogs with message loops of their own, which must
                        // not run while the app is being updated
                        cx.spawn(async move |_| {
                            shell.invoke(id, Some(windows::Win32::Foundation::HWND(hwnd as *mut _)))
                        })
                        .detach();
                    }),
                }
            }
        })
        .collect()
}

/// `query` with `+"folder"\` in front, instead of one put there before.
fn with_folder(query: &str, folder: &str) -> String {
    let rest = query
        .strip_prefix("+\"")
        .and_then(|q| q.split_once("\"\\"))
        .map_or(query, |(_, rest)| rest)
        .trim_start();
    format!("+\"{}\"\\ {}", folder, rest)
}

/// Index changes refresh the results at most this often. Typing or sorting always searches
/// right away.
fn refresh_interval(cx: &App) -> Duration {
    Duration::from_secs(cx.global::<Settings>().refresh_secs.max(1))
}

/// Number fields for the durations on the settings page, the larger unit first.
struct TimeFields {
    /// Hours and minutes without an active window before the service unloads the indices
    unload: [Entity<InputState>; 2],
    /// Hours and minutes in the tray before the window is closed
    close_hidden: [Entity<InputState>; 2],
    /// Minutes and seconds between refreshes
    refresh: [Entity<InputState>; 2],
}

impl TimeFields {
    fn new(window: &mut Window, cx: &mut Context<MainView>) -> Self {
        let mut field = || cx.new(|cx| InputState::new(window, cx).placeholder("0"));
        Self {
            unload: [field(), field()],
            close_hidden: [field(), field()],
            refresh: [field(), field()],
        }
    }

    fn all(&self) -> impl Iterator<Item = &Entity<InputState>> {
        self.unload
            .iter()
            .chain(&self.close_hidden)
            .chain(&self.refresh)
    }
}

/// The number in a time field, 0 if it is empty.
fn field_value(field: &Entity<InputState>, cx: &App) -> u64 {
    field.read(cx).value().parse().unwrap_or(0)
}

/// Puts `value` into the two fields of a duration, `unit` being how many of the smaller unit
/// make one of the larger.
fn set_fields(
    fields: &[Entity<InputState>; 2],
    value: u64,
    unit: u64,
    window: &mut Window,
    cx: &mut App,
) {
    for (field, part) in fields.iter().zip([value / unit, value % unit]) {
        field.update(cx, |field, cx| {
            field.set_value(part.to_string(), window, cx)
        });
    }
}

/// A row of buttons to pick one of `choices`.
fn choice_buttons<T: Copy + PartialEq + 'static>(
    id: &'static str,
    choices: &[(T, &'static str)],
    current: Option<T>,
    disabled: bool,
    pick: impl Fn(T, &mut Window, &mut App) + Clone + 'static,
) -> impl IntoElement {
    h_flex()
        .gap_2()
        .flex_wrap()
        .children(choices.iter().enumerate().map(|(i, &(value, label))| {
            let pick = pick.clone();
            Button::new((id, i))
                .label(label)
                .small()
                .selected(current == Some(value))
                .disabled(disabled)
                .on_click(move |_, window, cx| pick(value, window, cx))
        }))
}

impl MainView {
    /// Tells the service how long to keep the indices without an active window.
    fn set_unload_after(&mut self, secs: u64, cx: &mut Context<Self>) {
        if let Some(status) = &mut self.status {
            status.unload_after_secs = secs;
        }
        let client = self.client.clone();
        let task = cx
            .background_executor()
            .spawn(async move { client.request(&Request::SetUnloadAfter { secs }) });
        cx.spawn(async move |view, cx| {
            let response = task.await;
            let _ = view.update(cx, |view, cx| {
                view.settings_error = match response {
                    Ok(_) => None,
                    Err(e) => Some(format!("Changing the setting failed: {}", e)),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Takes the next key combination as the global shortcut, instead of what it would do.
    fn record_shortcut(&mut self, cx: &mut Context<Self>) {
        self.shortcut_error = None;
        let view = cx.entity().downgrade();
        self.recording_shortcut = Some(cx.intercept_keystrokes(move |event, _, cx| {
            cx.stop_propagation();
            let keystroke = event.keystroke.clone();
            let _ = view.update(cx, |view, cx| view.recorded(&keystroke, cx));
        }));
    }

    fn recorded(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        let m = &keystroke.modifiers;
        if keystroke.key == "escape" && !(m.control || m.alt || m.shift || m.platform) {
            self.recording_shortcut = None;
            cx.notify();
            return;
        }
        match crate::shortcut::from_keystroke(keystroke) {
            // A modifier alone, the key is still to come
            Ok(None) => return,
            Err(e) => self.shortcut_error = Some(e.into()),
            Ok(Some(shortcut)) => {
                self.recording_shortcut = None;
                // A shortcut another program uses is not taken, the current one stays
                match cx.global_mut::<Desktop>().set_shortcut(Some(&shortcut)) {
                    Ok(()) => {
                        self.shortcut_error = None;
                        Settings::update(cx, |s| s.shortcut = Some(shortcut));
                    }
                    Err(e) => self.shortcut_error = Some(e),
                }
            }
        }
        cx.notify();
    }

    /// Applies what was typed into the time fields, if it changed anything.
    fn apply_time_fields(&mut self, cx: &mut Context<Self>) {
        let [hours, minutes] = &self.time_fields.unload;
        let unload = field_value(hours, cx) * 3600 + field_value(minutes, cx) * 60;
        if self
            .status
            .as_ref()
            .is_some_and(|s| s.unload_after_secs != unload)
        {
            self.set_unload_after(unload, cx);
        }

        let [hours, minutes] = &self.time_fields.close_hidden;
        let close_hidden = field_value(hours, cx) * 60 + field_value(minutes, cx);
        let [minutes, seconds] = &self.time_fields.refresh;
        let refresh = field_value(minutes, cx) * 60 + field_value(seconds, cx);
        let settings = cx.global::<Settings>();
        // Refreshing all the time is no choice, an empty field is still being typed
        let refresh = if refresh == 0 {
            settings.refresh_secs
        } else {
            refresh
        };
        if settings.close_hidden_after_mins != close_hidden || settings.refresh_secs != refresh {
            Settings::update(cx, |s| {
                s.close_hidden_after_mins = close_hidden;
                s.refresh_secs = refresh;
            });
        }
    }

    /// Fills the time fields with the current settings.
    fn fill_time_fields(&self, window: &mut Window, cx: &mut Context<Self>) {
        let settings = cx.global::<Settings>().clone();
        let fields = &self.time_fields;
        set_fields(&fields.refresh, settings.refresh_secs, 60, window, cx);
        set_fields(
            &fields.close_hidden,
            settings.close_hidden_after_mins,
            60,
            window,
            cx,
        );
        if let Some(status) = &self.status {
            set_fields(
                &fields.unload,
                status.unload_after_secs / 60,
                60,
                window,
                cx,
            );
        }
    }

    /// The settings, instead of the results, with a button back to them. Rebuilt on every
    /// render from the [`Settings`] and [`DriveChoice`] globals and the service status.
    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = cx.global::<Settings>().clone();
        let theme = cx.theme();
        let heading = |text: &'static str| {
            div()
                .text_sm()
                .font_semibold()
                .text_color(theme.foreground)
                .child(text)
        };
        let hint = |text: &'static str| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };

        let drives = cx.try_global::<DriveChoice>();
        let content = v_flex()
            .p_4()
            .gap_6()
            .max_w(px(680.))
            .child(v_flex().gap_3().child(heading("Drives")).map(|this| {
                match drives {
                    Some(choice) if !choice.drives.is_empty() => this
                        .children(choice.drives.iter().map(|drive| {
                            let letter = drive.letter;
                            let mut label = format!("{}:", letter);
                            if !drive.label.is_empty() {
                                label.push_str(&format!("  {}", drive.label));
                            }
                            if drive.total_bytes > 0 {
                                label.push_str(&format!("  ({})", format::size(drive.total_bytes)));
                            }
                            Switch::new(SharedString::from(format!("drive-{}", letter)))
                                .checked(choice.selected.contains(&letter))
                                .label(label)
                                .on_click(move |checked, _, cx| {
                                    DriveChoice::toggle(cx, letter, *checked)
                                })
                        }))
                        .child(hint(
                            "Changes apply when you leave the settings. Turning a drive off \
                                 deletes its index.",
                        )),
                    _ => this.child(hint(
                        "The drives can be chosen while the Reverything service is running.",
                    )),
                }
            }))
            .child(
                v_flex()
                    .gap_2()
                    .child(heading("Appearance"))
                    .child(choice_buttons(
                        "theme",
                        &ThemeChoice::ALL.map(|c| (c, c.label())),
                        Some(settings.theme),
                        false,
                        |choice, window, cx| {
                            Settings::update(cx, |s| s.theme = choice);
                            apply_theme(Some(window), cx);
                        },
                    )),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(heading("Shortcut to show Reverything"))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("shortcut")
                                    .small()
                                    .label(match (&self.recording_shortcut, &settings.shortcut) {
                                        (Some(_), _) => "Press the shortcut...".to_string(),
                                        (None, Some(shortcut)) => shortcut.clone(),
                                        (None, None) => "None, click to record one".to_string(),
                                    })
                                    .selected(self.recording_shortcut.is_some())
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        if view.recording_shortcut.is_some() {
                                            view.recording_shortcut = None;
                                        } else {
                                            view.record_shortcut(cx);
                                        }
                                        cx.notify();
                                    })),
                            )
                            .when(
                                settings.shortcut.is_some() && self.recording_shortcut.is_none(),
                                |row| {
                                    row.child(
                                        Button::new("shortcut-remove")
                                            .small()
                                            .ghost()
                                            .label("Remove")
                                            .on_click(cx.listener(|view, _, _, cx| {
                                                view.shortcut_error = cx
                                                    .global_mut::<Desktop>()
                                                    .set_shortcut(None)
                                                    .err();
                                                Settings::update(cx, |s| s.shortcut = None);
                                            })),
                                    )
                                },
                            ),
                    )
                    .child(hint(
                        "Works from anywhere, also while the window is hidden in the tray. Click \
                         and press a combination with Ctrl, Alt or Win, Escape cancels.",
                    ))
                    .children(
                        self.shortcut_error
                            .clone()
                            .or_else(|| {
                                cx.try_global::<Desktop>()
                                    .and_then(|d| d.hotkey_error.clone())
                            })
                            .map(|e| div().text_xs().text_color(theme.danger).child(e)),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(heading("Timing"))
                    .child(time_setting(
                        "Refresh the results when files change, at most every",
                        &self.time_fields.refresh,
                        ["min", "s"],
                        false,
                        None,
                        cx,
                    ))
                    .child(time_setting(
                        "Unload the index when Reverything was not used for",
                        &self.time_fields.unload,
                        ["h", "min"],
                        self.status.is_none(),
                        Some("0 for never"),
                        cx,
                    ))
                    .child(time_setting(
                        "Close the window when it was hidden in the tray for",
                        &self.time_fields.close_hidden,
                        ["h", "min"],
                        false,
                        Some("0 for never"),
                        cx,
                    ))
                    .child(hint(
                        "An unloaded index stays saved and up to date, loading it again takes a \
                         moment. A closed window opens again from the shortcut or the tray.",
                    ))
                    .children(
                        self.settings_error
                            .clone()
                            .map(|e| div().text_xs().text_color(theme.danger).child(e)),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(heading("Explorer's context menu"))
                    .child(choice_buttons(
                        "explorer-menu",
                        &ExplorerMenu::ALL.map(|m| (m, m.label())),
                        Some(settings.explorer_menu),
                        false,
                        |menu, _, cx| Settings::update(cx, |s| s.explorer_menu = menu),
                    ))
                    .child(choice_buttons(
                        "explorer-menu-style",
                        &MenuStyle::ALL.map(|m| (m, m.label())),
                        Some(settings.explorer_menu_style),
                        settings.explorer_menu == ExplorerMenu::Off,
                        |style, _, cx| Settings::update(cx, |s| s.explorer_menu_style = style),
                    ))
                    .child(hint(
                        "Explorer's entries for the selected results, including the ones \
                         installed programs add, like 7-Zip. Shift+right click shows the \
                         extended ones. Windows' style is the classic Windows menu, restyled by \
                         tools like Nilesoft Shell.",
                    )),
            )
            .child(
                v_flex().gap_3().child(heading("Updates")).child(
                    // winget and Scoop installs never check, the package manager updates them
                    Switch::new("check-updates")
                        .checked(settings.check_updates && update::managed_by().is_none())
                        .disabled(update::managed_by().is_some())
                        .label("Check for a new version once a day")
                        .on_click(|checked, _, cx| {
                            let checked = *checked;
                            Settings::update(cx, |s| s.check_updates = checked)
                        }),
                ),
            )
            .child(
                v_flex()
                    .gap_3()
                    .child(heading("Startup"))
                    .child(
                        Switch::new("start-with-windows")
                            .checked(settings.start_with_windows)
                            .label("Start with Windows (hidden in the tray)")
                            .on_click(|checked, _, cx| {
                                let checked = *checked;
                                Settings::update(cx, |s| s.start_with_windows = checked)
                            }),
                    )
                    .child(
                        Switch::new("close-to-tray")
                            .checked(settings.close_to_tray)
                            .label("Keep running in the tray when the window is closed")
                            .on_click(|checked, _, cx| {
                                let checked = *checked;
                                Settings::update(cx, |s| s.close_to_tray = checked)
                            }),
                    ),
            );

        v_flex()
            .id("settings")
            .track_focus(&self.settings_focus)
            .flex_1()
            .overflow_hidden()
            .child(
                h_flex()
                    .px_2()
                    .py_1p5()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("settings-back")
                            .ghost()
                            .small()
                            .icon(IconName::ArrowLeft)
                            .tooltip("Back to the results (Esc)")
                            .on_click(
                                cx.listener(|view, _, window, cx| view.close_settings(window, cx)),
                            ),
                    )
                    .child(div().text_sm().font_semibold().child("Settings")),
            )
            .child(
                div().flex_1().overflow_hidden().child(
                    v_flex()
                        .id("settings-content")
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(content),
                ),
            )
    }
}

/// A duration setting on one line: the label, then the two fields with their units inside.
fn time_setting(
    label: &'static str,
    fields: &[Entity<InputState>; 2],
    units: [&'static str; 2],
    disabled: bool,
    note: Option<&'static str>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .gap_2()
        .text_sm()
        .child(div().min_w(px(360.)).child(label))
        .children(fields.iter().zip(units).map(|(field, unit)| {
            Input::new(field)
                .small()
                .w(px(76.))
                .disabled(disabled)
                .suffix(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(unit),
                )
        }))
        .children(note.map(|note| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(note)
        }))
}
