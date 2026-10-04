//! The result table. The service keeps the result set; the table only knows its size and
//! fetches the rows around the visible range in pages.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{h_flex, ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::*;
use reverything_protocol::{Request, Response, Row, Sort, SortColumn};

use crate::client::ServiceClient;
use crate::format;
use crate::icons::FileIcons;
use crate::settings::{ColumnSetting, Settings};
use crate::view::{CopyFile, CopyName, CopyPath, OpenSelected, RevealSelected, ShowProperties};

/// Rows fetched per request
const PAGE: usize = 256;
/// Compact rows, smaller than the table's smallest preset (26px)
pub const ROW_SIZE: gpui_kit::component::Size = gpui_kit::component::Size::Size(px(24.));
/// Cells get 4px vertical padding at custom sizes, the text has to fit in the rest. 16px is
/// what 12px text needs including descenders.
const CELL_LINE_HEIGHT: Pixels = px(16.);
/// How far the mouse has to move with the button down before a row is dragged out
const DRAG_THRESHOLD: Pixels = px(6.);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Name,
    Folder,
    Size,
    Modified,
    Created,
    Attributes,
}

impl ColumnKind {
    pub const ALL: [ColumnKind; 6] = [
        ColumnKind::Name,
        ColumnKind::Folder,
        ColumnKind::Size,
        ColumnKind::Modified,
        ColumnKind::Created,
        ColumnKind::Attributes,
    ];

    /// Stored in the settings
    fn key(self) -> &'static str {
        match self {
            ColumnKind::Name => "name",
            ColumnKind::Folder => "folder",
            ColumnKind::Size => "size",
            ColumnKind::Modified => "modified",
            ColumnKind::Created => "created",
            ColumnKind::Attributes => "attributes",
        }
    }

    fn title(self) -> &'static str {
        match self {
            ColumnKind::Name => "Name",
            ColumnKind::Folder => "Folder",
            ColumnKind::Size => "Size",
            ColumnKind::Modified => "Date Modified",
            ColumnKind::Created => "Date Created",
            ColumnKind::Attributes => "Attributes",
        }
    }

    fn default_width(self) -> f32 {
        match self {
            ColumnKind::Name => 300.,
            ColumnKind::Folder => 520.,
            ColumnKind::Size => 90.,
            ColumnKind::Modified | ColumnKind::Created => 140.,
            ColumnKind::Attributes => 96.,
        }
    }

    fn sort_column(self) -> SortColumn {
        match self {
            ColumnKind::Name => SortColumn::Name,
            ColumnKind::Folder => SortColumn::Path,
            ColumnKind::Size => SortColumn::Size,
            ColumnKind::Modified => SortColumn::Modified,
            ColumnKind::Created => SortColumn::Created,
            ColumnKind::Attributes => SortColumn::Attributes,
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.key() == key)
    }
}

#[derive(Debug, Clone, Copy)]
struct VisibleColumn {
    kind: ColumnKind,
    width: Pixels,
}

/// The visible columns from the settings. The name column is always shown.
fn columns_from_settings(settings: &[ColumnSetting]) -> Vec<VisibleColumn> {
    let mut columns: Vec<VisibleColumn> = Vec::new();
    for setting in settings {
        if let Some(kind) = ColumnKind::from_key(&setting.key) {
            if columns.iter().all(|c| c.kind != kind) {
                columns.push(VisibleColumn {
                    kind,
                    width: px(setting.width.clamp(20., 4000.)),
                });
            }
        }
    }
    if columns.is_empty() {
        columns = ColumnKind::ALL
            .into_iter()
            .map(|kind| VisibleColumn {
                kind,
                width: px(kind.default_width()),
            })
            .collect();
    }
    if columns.iter().all(|c| c.kind != ColumnKind::Name) {
        columns.insert(
            0,
            VisibleColumn {
                kind: ColumnKind::Name,
                width: px(ColumnKind::Name.default_width()),
            },
        );
    }
    columns
}

#[derive(Debug, Clone, Copy)]
pub struct SearchTiming {
    /// Time the service spent searching
    pub service: Duration,
    /// Time until the response arrived, including the pipe round trip
    pub round_trip: Duration,
}

pub struct Results {
    client: Arc<ServiceClient>,
    columns: Vec<VisibleColumn>,
    query: String,
    /// Column the user sorted by, `None` for the default order by name
    sorted_by: Option<(ColumnKind, bool)>,
    /// Include files and folders in the results
    pub files: bool,
    pub folders: bool,
    /// Incremented for every search we start, to drop responses of outdated ones
    seq: u64,
    /// Id of the result set on the service side
    search: u64,
    total: usize,
    pages: HashMap<usize, Vec<Row>>,
    /// Pages of the previous result set, shown while a refresh fetches the new ones so the table
    /// does not blank out in between
    stale: HashMap<usize, Vec<Row>>,
    pending: HashSet<usize>,
    visible: Range<usize>,
    icons: FileIcons,
    /// Row the context menu was opened for
    pub menu_row: Option<usize>,
    /// Row and position of a left button press that may turn into dragging the entry out
    drag_start: Option<(usize, Point<Pixels>)>,
    pub last_search: Option<SearchTiming>,
    pub last_page: Option<Duration>,
    pub error: Option<String>,
}

impl Results {
    pub fn new(client: Arc<ServiceClient>, columns: &[ColumnSetting]) -> Self {
        Self {
            client,
            columns: columns_from_settings(columns),
            query: String::new(),
            sorted_by: None,
            files: true,
            folders: true,
            seq: 0,
            search: 0,
            total: 0,
            pages: HashMap::new(),
            stale: HashMap::new(),
            pending: HashSet::new(),
            visible: 0..0,
            icons: FileIcons::default(),
            menu_row: None,
            drag_start: None,
            last_search: None,
            last_page: None,
            error: None,
        }
    }

    pub fn total(&self) -> usize {
        self.total
    }

    pub fn row(&self, ix: usize) -> Option<&Row> {
        let page = ix / PAGE;
        self.pages
            .get(&page)
            .or_else(|| self.stale.get(&page))
            .and_then(|page| page.get(ix % PAGE))
    }

    fn sort(&self) -> Sort {
        match self.sorted_by {
            Some((kind, ascending)) => Sort {
                column: kind.sort_column(),
                ascending,
            },
            None => Sort::default(),
        }
    }

    /// Runs `query` on the service. Refreshes keep the scroll position, new searches start at the
    /// top.
    pub fn search(
        &mut self,
        query: String,
        scroll_to_top: bool,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.query = query.clone();
        self.seq += 1;
        let seq = self.seq;
        let request = Request::Search {
            query,
            sort: self.sort(),
            files: self.files,
            folders: self.folders,
        };

        let client = self.client.clone();
        let task = cx.background_executor().spawn(async move {
            let t = Instant::now();
            let response = client.request(&request);
            (response, t.elapsed())
        });

        cx.spawn(async move |table, cx| {
            let (response, round_trip) = task.await;
            let _ = table.update(cx, |table, cx| {
                let results = table.delegate_mut();
                if results.seq != seq {
                    return;
                }
                match response {
                    Ok(Response::Search {
                        search,
                        total,
                        took_us,
                    }) => {
                        results.search = search;
                        results.total = total as usize;
                        if scroll_to_top {
                            results.stale.clear();
                            results.pages.clear();
                        } else {
                            // Pages of an earlier refresh that did not arrive yet stay stale
                            let pages = std::mem::take(&mut results.pages);
                            results.stale.extend(pages);
                        }
                        results.pending.clear();
                        results.error = None;
                        results.last_search = Some(SearchTiming {
                            service: Duration::from_micros(took_us),
                            round_trip,
                        });
                    }
                    Ok(other) => results.error = Some(format!("Unexpected response {:?}", other)),
                    Err(e) => {
                        results.error = Some(e);
                        results.total = 0;
                        results.pages.clear();
                        results.stale.clear();
                    }
                }

                if scroll_to_top {
                    results.visible = 0..0;
                }
                let visible = results.visible.clone();
                results.fetch_rows(visible, cx);
                if scroll_to_top {
                    table.scroll_to_row(0, cx);
                }
                table.refresh(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Fetches the pages covering `range` plus one page in each direction.
    fn fetch_rows(&mut self, range: Range<usize>, cx: &mut Context<TableState<Self>>) {
        if self.total == 0 {
            return;
        }
        let first = (range.start / PAGE).saturating_sub(1);
        let last = (range.end.max(range.start + 1) - 1) / PAGE + 1;
        for page in first..=last {
            if page * PAGE >= self.total {
                break;
            }
            if !self.pages.contains_key(&page) && self.pending.insert(page) {
                self.fetch_page(page, cx);
            }
        }
    }

    fn fetch_page(&mut self, page: usize, cx: &mut Context<TableState<Self>>) {
        let search = self.search;
        let client = self.client.clone();
        let request = Request::Rows {
            search,
            start: (page * PAGE) as u64,
            count: PAGE as u32,
        };
        let task = cx.background_executor().spawn(async move {
            let t = Instant::now();
            let response = client.request(&request);
            (response, t.elapsed())
        });

        cx.spawn(async move |table, cx| {
            let (response, took) = task.await;
            let _ = table.update(cx, |table, cx| {
                let results = table.delegate_mut();
                if results.search != search {
                    return;
                }
                results.pending.remove(&page);
                if let Ok(Response::Rows { rows, .. }) = response {
                    results.stale.remove(&page);
                    results.pages.insert(page, rows);
                    results.last_page = Some(took);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn cell_text(row: &Row, kind: ColumnKind) -> String {
        match kind {
            ColumnKind::Name => row.name.clone(),
            ColumnKind::Folder => row.folder.clone(),
            ColumnKind::Size => format::size(row.size),
            ColumnKind::Modified => format::time(row.modified),
            ColumnKind::Created => format::time(row.created),
            ColumnKind::Attributes => format::attributes(row.attributes),
        }
    }

    /// Shows or hides a column. The name column is always shown.
    fn toggle_column(&mut self, kind: ColumnKind) {
        if kind == ColumnKind::Name {
            return;
        }
        if let Some(ix) = self.columns.iter().position(|c| c.kind == kind) {
            self.columns.remove(ix);
            return;
        }
        // Put it back in front of the first visible column that comes after it by default
        let order = |k: ColumnKind| ColumnKind::ALL.iter().position(|&a| a == k);
        let ix = self
            .columns
            .iter()
            .position(|c| order(c.kind) > order(kind))
            .unwrap_or(self.columns.len());
        self.columns.insert(
            ix,
            VisibleColumn {
                kind,
                width: px(kind.default_width()),
            },
        );
    }

    /// Takes over the column widths after the user resized one.
    pub fn set_widths(&mut self, widths: &[Pixels]) {
        for (column, &width) in self.columns.iter_mut().zip(widths) {
            column.width = width;
        }
    }

    pub fn save_columns(&self, cx: &mut App) {
        let columns = self
            .columns
            .iter()
            .map(|c| ColumnSetting {
                key: c.kind.key().to_string(),
                width: f32::from(c.width).round(),
            })
            .collect();
        Settings::update(cx, |s| s.columns = columns);
    }
}

impl TableDelegate for Results {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.total
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let VisibleColumn { kind, width } = self.columns[col_ix];
        let column = Column::new(kind.key(), kind.title()).width(width);
        let column = match self.sorted_by {
            Some((sorted, true)) if sorted == kind => column.ascending(),
            Some((sorted, false)) if sorted == kind => column.descending(),
            _ => column.sortable(),
        };
        if kind == ColumnKind::Size {
            column.text_right()
        } else {
            column
        }
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let kind = self.columns[col_ix].kind;
        self.sorted_by = match sort {
            ColumnSort::Ascending => Some((kind, true)),
            ColumnSort::Descending => Some((kind, false)),
            ColumnSort::Default => None,
        };
        self.search(self.query.clone(), true, cx);
    }

    fn move_column(
        &mut self,
        col_ix: usize,
        to_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let column = self.columns.remove(col_ix);
        self.columns.insert(to_ix, column);
        self.save_columns(cx);
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.row(row_ix) else {
            return div().into_any_element();
        };
        let kind = self.columns[col_ix].kind;
        let text = Self::cell_text(row, kind);
        if kind != ColumnKind::Name {
            return div()
                .text_xs()
                .line_height(CELL_LINE_HEIGHT)
                .truncate()
                .child(text)
                .into_any_element();
        }

        let (name, directory) = (row.name.clone(), row.directory);
        let icon = match self.icons.get(&name, directory) {
            Some(image) => img(image).size_4().flex_shrink_0().into_any_element(),
            None => Icon::new(if directory {
                IconName::Folder
            } else {
                IconName::File
            })
            .small()
            .text_color(cx.theme().muted_foreground)
            .into_any_element(),
        };
        h_flex()
            .gap_1p5()
            .overflow_hidden()
            .child(icon)
            .child(
                div()
                    .text_xs()
                    .line_height(CELL_LINE_HEIGHT)
                    .truncate()
                    .child(text),
            )
            .into_any_element()
    }

    /// Header cells, with a context menu to show and hide columns.
    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let table = cx.entity().downgrade();
        let visible = self.columns.iter().map(|c| c.kind).collect::<Vec<_>>();
        div()
            .id(("th", col_ix))
            .size_full()
            .text_xs()
            .line_height(CELL_LINE_HEIGHT)
            .truncate()
            .child(self.columns[col_ix].kind.title())
            .context_menu(move |menu, _, _| {
                ColumnKind::ALL.into_iter().fold(menu, |menu, kind| {
                    let table = table.clone();
                    menu.item(
                        PopupMenuItem::new(kind.title())
                            .checked(visible.contains(&kind))
                            .disabled(kind == ColumnKind::Name)
                            .on_click(move |_, _, cx| {
                                let _ = table.update(cx, |table, cx| {
                                    let results = table.delegate_mut();
                                    results.toggle_column(kind);
                                    results.save_columns(cx);
                                    table.refresh(cx);
                                    cx.notify();
                                });
                            }),
                    )
                })
            })
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        div()
            .id(("row", row_ix))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |table, event: &MouseDownEvent, _, _| {
                    table.delegate_mut().drag_start = Some((row_ix, event.position));
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|table, _: &MouseUpEvent, _, _| table.delegate_mut().drag_start = None),
            )
            .on_mouse_move(cx.listener(|table, event: &MouseMoveEvent, window, cx| {
                let results = table.delegate_mut();
                let Some((row, start)) = results.drag_start else {
                    return;
                };
                if event.pressed_button != Some(MouseButton::Left) {
                    results.drag_start = None;
                    return;
                }
                let moved = event.position - start;
                if moved.x.abs() < DRAG_THRESHOLD && moved.y.abs() < DRAG_THRESHOLD {
                    return;
                }
                results.drag_start = None;
                let Some(path) = results.row(row).map(crate::shell::full_path) else {
                    return;
                };
                let hwnd = crate::desktop::hwnd(window).map(|h| h.0 as usize);
                // The shell runs its own message loop until the drop, so start it outside of
                // this event handler
                cx.spawn(async move |_, _| {
                    let hwnd = hwnd.map(|h| windows::Win32::Foundation::HWND(h as *mut _));
                    crate::shell::drag_out(&path, hwnd);
                })
                .detach();
            }))
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        self.menu_row = Some(row_ix);
        menu.menu_with_icon("Open", IconName::ExternalLink, Box::new(OpenSelected))
            .menu_with_icon(
                "Open containing folder",
                IconName::FolderOpen,
                Box::new(RevealSelected),
            )
            .separator()
            .menu_with_icon("Copy", IconName::Copy, Box::new(CopyFile))
            .menu("Copy full path", Box::new(CopyPath))
            .menu("Copy name", Box::new(CopyName))
            .separator()
            .menu_with_icon("Properties", IconName::Info, Box::new(ShowProperties))
    }

    fn visible_rows_changed(
        &mut self,
        visible_range: Range<usize>,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        self.visible = visible_range.clone();
        self.fetch_rows(visible_range, cx);
    }
}
