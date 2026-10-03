//! The search window: search box, result table and a status bar with the service health.

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
    h_flex, v_flex, ActiveTheme, Icon, IconName, Selectable, Sizable, StyledExt, TitleBar,
    WindowExt,
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
use crate::results::Results;
use crate::settings::{HotkeyChoice, Settings, ThemeChoice};
use crate::shell;

gpui_kit::actions!(
    reverything,
    [
        OpenSelected,
        RevealSelected,
        CopyPath,
        CopyName,
        ShowProperties,
        FocusSearch,
        HideWindow,
        OpenSettings,
        ToggleFiles,
        ToggleFolders,
    ]
);

pub const KEY_CONTEXT: &str = "Reverything";

pub fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-enter", RevealSelected, Some(KEY_CONTEXT)),
        KeyBinding::new("ctrl-shift-c", CopyPath, Some(KEY_CONTEXT)),
        KeyBinding::new("alt-enter", ShowProperties, Some(KEY_CONTEXT)),
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
    // Focused inputs only get a colored border instead of an extra ring around them
    Theme::global_mut(cx).focus_ring = false;
}

/// The app icon, shown in the title bar
static APP_ICON: &[u8] = include_bytes!("../../../assets/reverything.png");

/// How often the service status is polled
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum time between the last search and a refresh caused by index changes. Typing or
/// sorting always searches right away.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);
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
    /// The settings dialog is open, the drive selection is applied when it closes
    settings_open: bool,
    /// Changing the indexed drives failed
    drive_error: Option<String>,
    started: Instant,
    first_frame: Option<Duration>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(started: Instant, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let client = Arc::new(ServiceClient::default());
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search, e.g. report, .pdf, photos\\2024, !draft, !node_modules\\")
        });
        let columns = cx.global::<Settings>().columns.clone();
        let table = cx.new(|cx| {
            TableState::new(Results::new(client.clone(), &columns), window, cx)
                .row_selectable(true)
                .col_resizable(true)
                .col_movable(true)
                .sortable(true)
        });

        let subscriptions = vec![
            cx.subscribe_in(&input, window, |view, _, event, _, cx| match event {
                InputEvent::Change => view.search(true, cx),
                InputEvent::PressEnter { secondary, .. } => {
                    if let Some(path) = view.target_path(cx) {
                        if *secondary {
                            shell::reveal(&path);
                        } else {
                            shell::open(&path);
                        }
                    }
                }
                _ => {}
            }),
            cx.subscribe_in(&table, window, |view, table, event, _, cx| match event {
                TableEvent::DoubleClickedRow(row) => view.open_row(*row, cx),
                TableEvent::ColumnWidthsChanged(widths) => table.update(cx, |table, cx| {
                    table.delegate_mut().set_widths(widths);
                    table.delegate().save_columns(cx);
                }),
                _ => {}
            }),
        ];

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
            drive_error: None,
            started,
            first_frame: None,
            _subscriptions: subscriptions,
        };
        view.search(true, cx);
        view.poll_status(window, cx);
        view
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

    /// The entry actions apply to: the row of the open context menu, the selected row, or the
    /// first result.
    fn target_path(&mut self, cx: &mut Context<Self>) -> Option<String> {
        self.table.update(cx, |table, _| {
            let row = table
                .delegate_mut()
                .menu_row
                .take()
                .or_else(|| table.selected_row())
                .unwrap_or(0);
            table.delegate().row(row).map(shell::full_path)
        })
    }

    fn target_name(&mut self, cx: &mut Context<Self>) -> Option<String> {
        self.target_path(cx)
            .and_then(|path| path.rsplit('\\').next().map(str::to_string))
    }

    fn on_open(&mut self, _: &OpenSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.target_path(cx) {
            shell::open(&path);
        }
    }

    fn on_reveal(&mut self, _: &RevealSelected, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.target_path(cx) {
            shell::reveal(&path);
        }
    }

    fn on_copy_path(&mut self, _: &CopyPath, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.target_path(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(path));
        }
    }

    fn on_copy_name(&mut self, _: &CopyName, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.target_name(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(name));
        }
    }

    fn on_properties(&mut self, _: &ShowProperties, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.target_path(cx) {
            shell::properties(&path);
        }
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    fn on_hide(&mut self, _: &HideWindow, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        } else if cx.global::<Settings>().close_to_tray {
            desktop::hide(window);
        }
    }

    fn on_toggle_files(&mut self, _: &ToggleFiles, _: &mut Window, cx: &mut Context<Self>) {
        self.table.update(cx, |table, _| {
            let results = table.delegate_mut();
            results.files = !results.files;
        });
        self.search(true, cx);
    }

    fn on_toggle_folders(&mut self, _: &ToggleFolders, _: &mut Window, cx: &mut Context<Self>) {
        self.table.update(cx, |table, _| {
            let results = table.delegate_mut();
            results.folders = !results.folders;
        });
        self.search(true, cx);
    }

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
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
        window.open_dialog(cx, |dialog, _, cx| {
            dialog
                .title("Settings")
                .width(px(520.))
                .child(settings_panel(cx))
        });
    }

    /// Sends the drive selection of the settings dialog to the service, if it changed.
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

    /// Fetches the service status every second, and refreshes the results when the index
    /// changed, at most every [`REFRESH_INTERVAL`] and only while the window is active. A drive
    /// that became searchable refreshes them right away.
    fn poll_status(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |view, cx| loop {
            let task = cx.background_executor().spawn({
                let client = client.clone();
                async move {
                    let t = Instant::now();
                    (client.request(&Request::Status), t.elapsed())
                }
            });
            let (response, round_trip) = task.await;

            let alive = view.update_in(cx, |view, window, cx| {
                match response {
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

                        // After reconnecting the results may be empty or stale
                        let refresh = changed
                            && (new_volumes
                                || view.last_search.elapsed() >= REFRESH_INTERVAL
                                    && window.is_window_active());
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
            });
            if alive.is_err() {
                return;
            }
            cx.background_executor().timer(STATUS_INTERVAL).await;
        })
        .detach();
    }

    /// Icon, name and settings on the left, the window controls on the right.
    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = std::sync::Arc::new(Image::from_bytes(ImageFormat::Png, APP_ICON.to_vec()));
        TitleBar::new().h(TITLE_BAR_HEIGHT).child(
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
                    div().id("settings-area").occlude().child(
                        Button::new("settings")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Settings)
                            .tooltip("Settings (Ctrl+,)")
                            .on_click(cx.listener(|view, _, window, cx| {
                                view.on_open_settings(&OpenSettings, window, cx)
                            })),
                    ),
                ),
        )
    }

    /// Search box, the file and folder filters and the syntax help.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.table.read(cx).delegate();
        let (files, folders) = (results.files, results.folders);
        let toggle = |id: &'static str, icon: IconName, on: bool, tooltip: &'static str| {
            Button::new(id)
                .ghost()
                .small()
                .icon(icon)
                .selected(on)
                .tooltip(tooltip)
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
                toggle("files", IconName::File, files, "Show files (Alt+F)").on_click(cx.listener(
                    |view, _, window, cx| view.on_toggle_files(&ToggleFiles, window, cx),
                )),
            )
            .child(
                toggle("folders", IconName::Folder, folders, "Show folders (Alt+D)").on_click(
                    cx.listener(|view, _, window, cx| {
                        view.on_toggle_folders(&ToggleFolders, window, cx)
                    }),
                ),
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
            (None, None) => format!("{} objects", format::group_digits(results.total() as u64)),
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
                        VolumeState::Waiting | VolumeState::Loading | VolumeState::Indexing
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
        (".pdf", "Files by extension"),
        ("!draft", "Leave out names containing \"draft\""),
        (
            "!node_modules\\",
            "Leave out matching folders with everything inside",
        ),
        (
            "!C:\\Windows",
            "Leave out this folder with everything inside",
        ),
        (
            "photos\\2024",
            "\"2024\" inside a folder matching \"photos\"",
        ),
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
        .child(
            div()
                .mt_1()
                .text_color(theme.muted_foreground)
                .child("Separate terms with spaces, every term has to match. Case does not matter, quotes keep spaces in a term."),
        )
}

fn hint_owned(theme: &gpui_kit::component::Theme, text: String) -> Div {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text)
}

/// Settings dialog content. Rebuilt on every render from the [`Settings`] and
/// [`DriveChoice`] globals.
fn settings_panel(cx: &App) -> impl IntoElement {
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
    v_flex()
        .gap_5()
        .child(
            v_flex()
                .gap_3()
                .child(heading("Drives"))
                .map(|this| match drives {
                    Some(choice) if !choice.drives.is_empty() => {
                        this.children(choice.drives.iter().map(|drive| {
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
                            "Changes apply when the settings close. Turning a drive off deletes \
                             its index.",
                        ))
                    }
                    _ => this.child(hint(
                        "The drives can be chosen while the Reverything service is running.",
                    )),
                }),
        )
        .child(
            v_flex()
                .gap_2()
                .child(heading("Appearance"))
                .child(h_flex().gap_2().children(ThemeChoice::ALL.iter().enumerate().map(
                    |(i, &choice)| {
                        Button::new(("theme", i))
                            .label(choice.label())
                            .small()
                            .selected(settings.theme == choice)
                            .on_click(move |_, window, cx| {
                                Settings::update(cx, |s| s.theme = choice);
                                apply_theme(Some(window), cx);
                            })
                    },
                ))),
        )
        .child(
            v_flex()
                .gap_2()
                .child(heading("Shortcut to show Reverything"))
                .child(h_flex().gap_2().flex_wrap().children(
                    std::iter::once(None)
                        .chain(HotkeyChoice::ALL.iter().copied().map(Some))
                        .enumerate()
                        .map(|(i, choice)| {
                            Button::new(("hotkey", i))
                                .label(choice.map_or("Automatic", HotkeyChoice::label))
                                .small()
                                .selected(settings.hotkey == choice)
                                .on_click(move |_, _, cx| {
                                    Settings::update(cx, |s| s.hotkey = choice);
                                    if cx.has_global::<Desktop>() {
                                        cx.global_mut::<Desktop>().apply_hotkey(choice);
                                    }
                                })
                        }),
                ))
                .child(hint(
                    "Works from anywhere, also while the window is hidden in the tray. Automatic uses \
                     the first shortcut no other program uses.",
                ))
                .children(
                    cx.try_global::<Desktop>()
                        .and_then(|d| d.active_hotkey)
                        .filter(|c| *c != HotkeyChoice::None)
                        .map(|c| hint_owned(theme, format!("Active: {}", c.label()))),
                )
                .children(
                    cx.try_global::<Desktop>()
                        .and_then(|d| d.hotkey_error.clone())
                        .map(|e| div().text_xs().text_color(theme.danger).child(e)),
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
        )
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
        // However the dialog was closed (button, Escape, clicking outside)
        if self.settings_open && !window.has_active_dialog(cx) {
            self.settings_open = false;
            self.apply_drive_choice(cx);
        }

        let no_drives = self
            .status
            .as_ref()
            .is_some_and(|s| s.volumes.iter().all(|v| v.state == VolumeState::Disabled));

        v_flex()
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::on_open))
            .on_action(cx.listener(Self::on_reveal))
            .on_action(cx.listener(Self::on_copy_path))
            .on_action(cx.listener(Self::on_copy_name))
            .on_action(cx.listener(Self::on_properties))
            .on_action(cx.listener(Self::on_focus_search))
            .on_action(cx.listener(Self::on_hide))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_toggle_files))
            .on_action(cx.listener(Self::on_toggle_folders))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(self.render_toolbar(cx))
            .map(|this| {
                if no_drives {
                    this.child(self.render_no_drives(cx))
                } else {
                    this.child(
                        div().flex_1().overflow_hidden().child(
                            DataTable::new(&self.table)
                                .stripe(true)
                                .with_size(crate::results::ROW_SIZE),
                        ),
                    )
                }
            })
            .child(self.render_status_bar(window, cx))
    }
}
