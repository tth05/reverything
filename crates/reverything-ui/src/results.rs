//! The result table. The service keeps the result set; the table only knows its size and
//! fetches the rows around the visible range in pages.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::menu::PopupMenu;
use gpui_kit::component::table::{Column, ColumnSort, TableDelegate, TableState};
use gpui_kit::component::{h_flex, ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::*;
use reverything_protocol::{Request, Response, Row, Sort, SortColumn};

use crate::client::ServiceClient;
use crate::format;
use crate::icons::FileIcons;
use crate::view::{CopyName, CopyPath, OpenSelected, RevealSelected, ShowProperties};

/// Rows fetched per request
const PAGE: usize = 256;
/// Compact rows, smaller than the table's smallest preset (26px)
pub const ROW_SIZE: gpui_kit::component::Size = gpui_kit::component::Size::Size(px(24.));
/// Cells get 4px vertical padding at custom sizes, the text has to fit in the rest. 16px is
/// what 12px text needs including descenders.
const CELL_LINE_HEIGHT: Pixels = px(16.);
/// How far the mouse has to move with the button down before a row is dragged out
const DRAG_THRESHOLD: Pixels = px(6.);

#[derive(Debug, Clone, Copy)]
pub struct SearchTiming {
    /// Time the service spent searching
    pub service: Duration,
    /// Time until the response arrived, including the pipe round trip
    pub round_trip: Duration,
}

pub struct Results {
    client: Arc<ServiceClient>,
    columns: Vec<Column>,
    query: String,
    sort: Sort,
    /// Incremented for every search we start, to drop responses of outdated ones
    seq: u64,
    /// Id of the result set on the service side
    search: u64,
    total: usize,
    pages: HashMap<usize, Vec<Row>>,
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
    pub fn new(client: Arc<ServiceClient>) -> Self {
        let column = |key: &'static str, name: &'static str, width: f32| {
            Column::new(key, name).width(px(width)).sortable()
        };
        Self {
            client,
            columns: vec![
                column("name", "Name", 300.),
                column("folder", "Folder", 520.),
                column("size", "Size", 90.).text_right(),
                column("modified", "Date Modified", 140.),
                column("created", "Date Created", 140.),
                column("attributes", "Attributes", 96.),
            ],
            query: String::new(),
            sort: Sort::default(),
            seq: 0,
            search: 0,
            total: 0,
            pages: HashMap::new(),
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
        self.pages
            .get(&(ix / PAGE))
            .and_then(|page| page.get(ix % PAGE))
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
            sort: self.sort,
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
                        results.pages.clear();
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
                    results.pages.insert(page, rows);
                    results.last_page = Some(took);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn cell_text(row: &Row, col_ix: usize) -> String {
        match col_ix {
            1 => row.folder.clone(),
            2 => format::size(row.size),
            3 => format::time(row.modified),
            4 => format::time(row.created),
            5 => format::attributes(row.attributes),
            _ => row.name.clone(),
        }
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
        self.columns[col_ix].clone()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let column = match col_ix {
            1 => SortColumn::Path,
            2 => SortColumn::Size,
            3 => SortColumn::Modified,
            4 => SortColumn::Created,
            5 => SortColumn::Attributes,
            _ => SortColumn::Name,
        };
        self.sort = match sort {
            ColumnSort::Ascending => Sort {
                column,
                ascending: true,
            },
            ColumnSort::Descending => Sort {
                column,
                ascending: false,
            },
            ColumnSort::Default => Sort::default(),
        };
        self.search(self.query.clone(), true, cx);
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
        let text = Self::cell_text(row, col_ix);
        if col_ix != 0 {
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

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .text_xs()
            .line_height(CELL_LINE_HEIGHT)
            .truncate()
            .child(self.column(col_ix, cx).name.clone())
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
            .menu_with_icon("Copy full path", IconName::Copy, Box::new(CopyPath))
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
