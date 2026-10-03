use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{
    Model, ModelNotify, ModelRc, ModelTracker, SharedString, StandardListViewItem, Timer,
    TimerMode, VecModel,
};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use crate::index::VolumeIndex;
use crate::search::{hit_parts, search_all, Hit, Sort, SortColumn};
use crate::service::IndexSet;

slint::include_modules!();

struct SearchRequest {
    text: String,
    sort: Sort,
}

pub fn run_ui(set: Arc<IndexSet>) -> Result<(), slint::PlatformError> {
    let app = App::new()?;

    let model = Rc::new(ResultsModel {
        set: set.clone(),
        hits: RefCell::new(Vec::new()),
        notify: Default::default(),
    });
    app.set_rows(model.clone().into());

    let (tx, rx) = mpsc::channel::<SearchRequest>();
    spawn_search_worker(set.clone(), rx, app.as_weak());

    let query = Rc::new(RefCell::new(String::new()));
    let sort = Rc::new(Cell::new(Sort::default()));
    let searched_generation = Rc::new(Cell::new(u64::MAX));

    let submit = {
        let (set, query, sort, searched_generation) =
            (set.clone(), query.clone(), sort.clone(), searched_generation.clone());
        Rc::new(move || {
            searched_generation.set(set.generation());
            let _ = tx.send(SearchRequest {
                text: query.borrow().clone(),
                sort: sort.get(),
            });
        })
    };

    app.on_search_changed({
        let (query, submit) = (query.clone(), submit.clone());
        move |text: SharedString| {
            *query.borrow_mut() = text.to_string();
            submit();
        }
    });

    app.on_sort_changed({
        let (sort, submit) = (sort.clone(), submit.clone());
        move |column, ascending| {
            let column = match column {
                1 => SortColumn::Path,
                2 => SortColumn::Size,
                3 => SortColumn::Modified,
                4 => SortColumn::Created,
                5 => SortColumn::Attributes,
                _ => SortColumn::Name,
            };
            sort.set(Sort { column, ascending });
            submit();
        }
    });

    // Double click opens the containing folder with the file selected
    app.on_row_clicked({
        let model = model.clone();
        let last_click = Cell::new((usize::MAX, Instant::now()));
        move |row| {
            let row = row as usize;
            let (last_row, at) = last_click.get();
            if last_row == row && at.elapsed() < Duration::from_millis(500) {
                if let Some(path) = model.full_path(row) {
                    let _ = std::process::Command::new("explorer.exe")
                        .arg(format!("/select,{}", path))
                        .spawn();
                }
                last_click.set((usize::MAX, Instant::now()));
            } else {
                last_click.set((row, Instant::now()));
            }
        }
    });

    // Picks up journal updates and status changes
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(250), {
        let (app, set) = (app.as_weak(), set.clone());
        move || {
            if let Some(app) = app.upgrade() {
                app.set_status(set.status().into());
            }
            if set.generation() != searched_generation.get() {
                submit();
            }
        }
    });

    app.run()
}

/// Runs searches off the UI thread. Requests that queue up while a search runs are collapsed
/// into the newest one.
fn spawn_search_worker(
    set: Arc<IndexSet>,
    rx: mpsc::Receiver<SearchRequest>,
    app: slint::Weak<App>,
) {
    std::thread::Builder::new()
        .name("search".into())
        .spawn(move || {
            while let Ok(mut request) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    request = newer;
                }

                let t = Instant::now();
                let hits = search_all(&set, &request.text, request.sort);
                let took = t.elapsed();

                let _ = app.upgrade_in_event_loop(move |app| {
                    let rows = app.get_rows();
                    let model = rows.as_any().downcast_ref::<ResultsModel>().unwrap();
                    app.set_result_info(format_result_info(hits.len(), took, request.sort).into());
                    model.set_hits(hits);
                });
            }
        })
        .expect("Failed to spawn search thread");
}

fn format_result_info(count: usize, took: Duration, sort: Sort) -> String {
    let mut info = format!("{} objects in {:.1} ms", group_digits(count as u64), took.as_secs_f64() * 1000.0);
    if sort.column == SortColumn::Path && count > crate::search::MAX_PATH_SORT {
        info.push_str(" (too many results to sort by folder)");
    }
    info
}

pub struct ResultsModel {
    set: Arc<IndexSet>,
    hits: RefCell<Vec<Hit>>,
    notify: ModelNotify,
}

impl ResultsModel {
    fn set_hits(&self, hits: Vec<Hit>) {
        *self.hits.borrow_mut() = hits;
        self.notify.reset();
    }

    fn with_entry<T>(&self, row: usize, f: impl FnOnce(&VolumeIndex, u32) -> T) -> Option<T> {
        let (v, id) = hit_parts(*self.hits.borrow().get(row)?);
        let index = self.set.volumes.get(v)?.index.read().unwrap();
        // The index may have changed since the search ran
        index.is_in_use(id).then(|| f(&index, id))
    }

    fn full_path(&self, row: usize) -> Option<String> {
        self.with_entry(row, |index, id| index.full_path(id))
    }
}

impl Model for ResultsModel {
    type Data = ModelRc<StandardListViewItem>;

    fn row_count(&self) -> usize {
        self.hits.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let cells = self
            .with_entry(row, |index, id| {
                [
                    SharedString::from(index.name_str(id)),
                    SharedString::from(index.folder_path(id)),
                    format_size(index.size(id)).into(),
                    format_time(index.modified(id)).into(),
                    format_time(index.created(id)).into(),
                    format_attributes(index.flags(id)).into(),
                ]
            })
            .unwrap_or_default();

        Some(ModelRc::new(VecModel::from(
            cells.into_iter().map(StandardListViewItem::from).collect::<Vec<_>>(),
        )))
    }

    fn set_row_data(&self, _row: usize, _data: Self::Data) {}

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{} B", bytes);
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.0} {}", value, UNITS[unit])
    }
}

/// Explorer style attribute letters.
pub fn format_attributes(flags: u32) -> String {
    const LETTERS: [(u32, char); 9] = [
        (0x1, 'R'),     // read only
        (0x2, 'H'),     // hidden
        (0x4, 'S'),     // system
        (0x20, 'A'),    // archive
        (0x100, 'T'),   // temporary
        (0x200, 'P'),   // sparse
        (0x400, 'L'),   // reparse point
        (0x800, 'C'),   // compressed
        (0x4000, 'E'),  // encrypted
    ];
    LETTERS
        .iter()
        .filter(|(bit, _)| flags & bit != 0)
        .map(|&(_, c)| c)
        .collect()
}

/// Formats a unix timestamp in local time.
pub fn format_time(unix: u32) -> String {
    if unix == 0 {
        return String::new();
    }
    let ticks = (unix as u64 + 11_644_473_600) * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return String::new();
        }
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute
    )
}
