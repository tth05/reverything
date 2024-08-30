use crate::ntfs::index::{FileInfo, NtfsVolumeIndex};
use slint::{
    Model, ModelNotify, ModelRc, ModelTracker, SharedString, StandardListViewItem, VecModel,
};
use std::cell::RefCell;
use std::default::Default;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use rayon::prelude::*;

slint::include_modules!();

pub fn run_ui(index: Arc<Mutex<NtfsVolumeIndex>>) -> Result<(), slint::PlatformError> {
    let ui = App::new()?;

    let model = Rc::new(InfiniteModel {
        ntfs_index: index,
        filtered_files: RefCell::new(Vec::new()),
        notify: Default::default(),
    });
    model.set_filter("".to_string());

    ui.set_data(model.clone().into());

    ui.on_search_input_change(move |search: SharedString| {
        model.set_filter(search.to_string());
    });

    ui.run()
}

pub struct InfiniteModel {
    ntfs_index: Arc<Mutex<NtfsVolumeIndex>>,
    filtered_files: RefCell<Vec<u64>>,
    // the ModelNotify will allow to notify the UI that the model changes
    notify: ModelNotify,
}

impl InfiniteModel {
    fn set_filter(&self, search: String) {
        let mut vec = self.filtered_files.take();
        vec.clear();

        let match_fn: Box<dyn Fn(&(usize, Option<&FileInfo>)) -> bool  + Send+ Sync> = if search.is_empty() {
            Box::new(|(_, info)| info.is_some())
        } else {
            Box::new(|(_, info)| matches!(info, Some(info) if info.name.contains(&search)))
        };

        let vec = self.ntfs_index
            .lock()
            .unwrap()
            .par_iter()
            .enumerate()
            .filter(match_fn)
            .map(|(i, _)| i as u64)
            .collect();
        self.filtered_files.replace(vec);

        self.notify.reset();
    }
}

impl Model for InfiniteModel {
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
        // a typical implementation just return `self`
        self
    }
}
