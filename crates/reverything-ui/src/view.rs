//! The search window: search box, result table and a status bar with the service health.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::hover_card::HoverCard;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::table::{DataTable, TableEvent, TableState};
use gpui_kit::component::{
    h_flex, v_flex, ActiveTheme, Icon, IconName, Selectable, Sizable, StyledExt, TitleBar,
    WindowExt,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use reverything_protocol::{Request, Response, Status, VolumeState, VolumeStatus};

use crate::client::ServiceClient;
use crate::desktop::{self, Desktop};
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
}

/// The app icon, shown in the title bar
static APP_ICON: &[u8] = include_bytes!("../../../assets/reverything.png");

/// How often the service status is polled
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Minimum time between the last search and a refresh caused by index changes. Typing or
/// sorting always searches right away.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

pub struct MainView {
    input: Entity<InputState>,
    table: Entity<TableState<Results>>,
    client: Arc<ServiceClient>,
    status: Option<Status>,
    status_error: Option<String>,
    status_round_trip: Option<Duration>,
    /// Index generation the current results were searched at
    searched_generation: Option<u64>,
    last_search: Instant,
    started: Instant,
    first_frame: Option<Duration>,
    _subscriptions: Vec<Subscription>,
}

impl MainView {
    pub fn new(started: Instant, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let client = Arc::new(ServiceClient::default());
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(
                "Search, e.g. notepad, windows\\system32\\, report !draft, .rs !target\\",
            )
        });
        let table = cx.new(|cx| {
            TableState::new(Results::new(client.clone()), window, cx)
                .row_selectable(true)
                .col_resizable(true)
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
            cx.subscribe_in(&table, window, |view, _, event, _, cx| {
                if let TableEvent::DoubleClickedRow(row) = event {
                    view.open_row(*row, cx);
                }
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
            last_search: Instant::now(),
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

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        window.open_dialog(cx, |dialog, _, cx| {
            dialog
                .title("Settings")
                .width(px(520.))
                .child(settings_panel(cx))
        });
    }

    /// Fetches the service status every second, and refreshes the results when the index
    /// changed, at most every [`REFRESH_INTERVAL`] and only while the window is active.
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
                        view.searched_generation.get_or_insert(status.generation);
                        view.status = Some(status);
                        view.status_error = None;
                        view.status_round_trip = Some(round_trip);

                        // After reconnecting the results may be empty or stale
                        let refresh = changed
                            && view.last_search.elapsed() >= REFRESH_INTERVAL
                            && window.is_window_active();
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
        TitleBar::new().child(
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
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(OpenSettings), cx)
                            }),
                    ),
                ),
        )
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.table.read(cx).delegate();
        let left = match &results.error {
            // The health indicator already says that the service is not reachable
            Some(_) if self.status.is_none() => String::new(),
            Some(e) => e.clone(),
            None => format!("{} objects", format::group_digits(results.total() as u64)),
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
            .child(self.render_health(cx))
    }

    /// Icon with a short label, showing every timing we collect on hover.
    fn render_health(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (label, indicator): (String, AnyElement) = match (&self.status, &self.status_error) {
            (None, Some(e)) => (
                e.clone(),
                Icon::new(IconName::CircleX)
                    .small()
                    .text_color(theme.danger)
                    .into_any_element(),
            ),
            (None, None) => (
                "Connecting".into(),
                Spinner::new().small().into_any_element(),
            ),
            (Some(status), _) => {
                let states = status.volumes.iter().map(|v| &v.state).collect::<Vec<_>>();
                if states.iter().any(|s| matches!(s, VolumeState::Failed(_))) {
                    (
                        "Problem".into(),
                        Icon::new(IconName::TriangleAlert)
                            .small()
                            .text_color(theme.warning)
                            .into_any_element(),
                    )
                } else if states.iter().any(|s| {
                    matches!(
                        s,
                        VolumeState::Waiting | VolumeState::Loading | VolumeState::Indexing
                    )
                }) {
                    ("Indexing".into(), Spinner::new().small().into_any_element())
                } else if states.iter().any(|s| matches!(s, VolumeState::Offline)) {
                    (
                        "Offline".into(),
                        Icon::new(IconName::Info)
                            .small()
                            .text_color(theme.info)
                            .into_any_element(),
                    )
                } else {
                    (
                        "Up to date".into(),
                        Icon::new(IconName::CircleCheck)
                            .small()
                            .text_color(theme.success)
                            .into_any_element(),
                    )
                }
            }
        };

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
            .child(self.render_timings(cx))
    }

    fn render_timings(&self, cx: &mut Context<Self>) -> impl IntoElement {
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

        panel.render(cx)
    }
}

fn volume_timings(panel: &mut Panel, v: &VolumeStatus) {
    panel.section(&format!("Volume {}:", v.letter));
    let state = match &v.state {
        VolumeState::Waiting => "Waiting".to_string(),
        VolumeState::Loading => "Loading the saved index".to_string(),
        VolumeState::Indexing => "Reading the MFT".to_string(),
        VolumeState::Ready => "Up to date".to_string(),
        VolumeState::Offline => "Offline, not updated".to_string(),
        VolumeState::Failed(e) => format!("Failed: {}", e),
    };
    panel.row("State", state);
    panel.row("Entries", format::group_digits(v.entries));
    panel.row("Index memory", format::size(v.index_bytes));
    if let Some(us) = v.ready_after_us {
        panel.row("Searchable after", format::micros(us));
    }
    if let Some(us) = v.load_us {
        panel.row("Loading the saved index", format::micros(us));
    }
    if let Some(e) = &v.load_error {
        panel.row("Saved index not used", e.clone());
    }
    if let Some(s) = &v.scan {
        let total = s.open_us + s.read_parse_us + s.merge_us + s.sort_us + s.folder_sizes_us;
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
            .min_w(px(380.))
            .max_w(px(560.))
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

fn hint_owned(theme: &gpui_kit::component::Theme, text: String) -> Div {
    div()
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text)
}

/// Settings dialog content. Rebuilt on every render from the [`Settings`] global.
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

    v_flex()
        .gap_5()
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.first_frame.is_none() {
            let elapsed = self.started.elapsed();
            self.first_frame = Some(elapsed);
            if std::env::var_os("REVERYTHING_LOG_STARTUP").is_some() {
                crate::log::write(&format!("first frame after {:?}", elapsed));
            }
        }

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
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_title_bar(cx))
            .child(
                div().px_2().py_1p5().child(
                    Input::new(&self.input)
                        .small()
                        .prefix(Icon::new(IconName::Search).small())
                        .cleanable(true),
                ),
            )
            .child(
                div().flex_1().overflow_hidden().child(
                    DataTable::new(&self.table)
                        .stripe(true)
                        .with_size(crate::results::ROW_SIZE),
                ),
            )
            .child(self.render_status_bar(cx))
    }
}
