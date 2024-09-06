use crate::ntfs::index::{FileInfo, NtfsVolumeIndex};
use rayon::prelude::*;
use slint::{
    Model, ModelNotify, ModelRc, ModelTracker, SharedString, StandardListViewItem, VecModel,
};
use std::cell::RefCell;
use std::default::Default;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

slint::include_modules!();

pub fn run_ui(index: Arc<Mutex<NtfsVolumeIndex>>) -> Result<(), slint::PlatformError> {
    let app = App::new()?;

    let model = Rc::new(NtfsIndexTableModel {
        ntfs_index: index,
        filter: RefCell::new("".to_string()),
        filtered_files: RefCell::new(Vec::new()),
        notify: Default::default(),
    });
    model.set_filter("".to_string());

    let app_weak = app.as_weak();
    std::thread::spawn(move || loop {
        // While this is a bit lazy (we simply match the journal update loop found in the main file),
        // it gets the job done and is easier than using a channel which notifies the UI.
        std::thread::sleep(std::time::Duration::from_secs(1));

        let app_weak = app_weak.clone();
        slint::invoke_from_event_loop(move || {
            app_weak
                .unwrap()
                .get_data()
                .as_any()
                .downcast_ref::<NtfsIndexTableModel>()
                .unwrap()
                .refresh()
        })
        .expect("Failed to refresh model");
    });

    app.set_data(model.clone().into());

    app.on_search_input_change(move |search: SharedString| {
        model.set_filter(search.to_string());
    });

    app.run()
}

pub struct NtfsIndexTableModel {
    ntfs_index: Arc<Mutex<NtfsVolumeIndex>>,
    filter: RefCell<String>,
    filtered_files: RefCell<Vec<u64>>,
    notify: ModelNotify,
}

unsafe impl Send for NtfsIndexTableModel {}
unsafe impl Sync for NtfsIndexTableModel {}

impl NtfsIndexTableModel {
    fn refresh(&self) {
        self.set_filter(self.filter.take());
    }

    fn set_filter(&self, search: String) {
        self.filter.replace(search.to_string());

        let mut vec = self.filtered_files.take();
        vec.clear();

        let search = search
            .split(|c| c == '\\' || c == '/')
            .filter(|s| !s.is_empty())
            .rev()
            .collect::<Vec<_>>();
        let ntfs_index = self.ntfs_index.lock().unwrap();

        let match_fn: Box<dyn Fn(&(usize, Option<&FileInfo>)) -> bool + Send + Sync> =
            if search.is_empty() {
                Box::new(|(_, info)| info.is_some())
            } else {
                Box::new(|(_, info)| {
                    let Some(info) = info else {
                        return false;
                    };

                    ntfs_index
                        .iter_with_parents(info)
                        .zip(search.iter())
                        .all(|(info, search)| info.name.contains(search))
                })
            };

        let vec = ntfs_index
            .par_iter()
            .enumerate()
            .filter(match_fn)
            .map(|(i, _)| i as u64)
            .collect();
        self.filtered_files.replace(vec);

        self.notify.reset();
    }
}

impl Model for NtfsIndexTableModel {
    type Data = ModelRc<StandardListViewItem>;

    fn row_count(&self) -> usize {
        self.filtered_files.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let ntfs_index = self.ntfs_index.lock().unwrap();
        let file_info = ntfs_index.find_by_index(self.filtered_files.borrow()[row])?;

        Some(
            Rc::new(VecModel::from(vec![
                StandardListViewItem::from(slint::format!(
                    "{}",
                    ntfs_index.compute_full_path(file_info)
                )),
                StandardListViewItem::from(slint::format!("{}", file_info.size())),
            ]))
            .into(),
        )
    }

    fn set_row_data(&self, _row: usize, _data: Self::Data) {
        panic!("InfiniteModel is read-only");
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
