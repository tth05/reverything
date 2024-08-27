use std::cell::RefCell;
use slint::{Model, ModelNotify, ModelRc, ModelTracker, SharedString, StandardListViewItem, VecModel};
use std::default::Default;
use std::rc::Rc;
use std::time::Instant;
use crate::ntfs::index::NtfsVolumeIndex;

slint::include_modules!();

pub fn run_ui(index: NtfsVolumeIndex) -> Result<(), slint::PlatformError> {
    let ui = App::new()?;

    let model = Rc::new(InfiniteModel {
        ntfs_index: index,
        filtered_files: RefCell::new(None),
        notify: Default::default(),
    });

    ui.set_data(model.clone().into());
    
    ui.on_search_input_change(move |search: SharedString| {
        model.set_filter(search.to_string());
    });

    ui.run()
}

pub struct InfiniteModel {
    ntfs_index: NtfsVolumeIndex,
    filtered_files: RefCell<Option<Vec<u64>>>,
    // the ModelNotify will allow to notify the UI that the model changes
    notify: ModelNotify,
}

impl InfiniteModel {
    fn set_filter(&self, search: String) {
        let t = Instant::now();
        if search.is_empty() {
            self.filtered_files.replace(None);
        } else {
            let mut vec = self.filtered_files.take().unwrap_or_default();
            vec.clear();
            self.ntfs_index
                .iter()
                .enumerate()
                .filter(|(_, info)| info.name.contains(&search))
                .map(|(i, _)| i as u64)
                .for_each(|i| vec.push(i));
            self.filtered_files.replace(Some(vec));
        }
        
        self.notify.reset();
    }
}

impl Model for InfiniteModel {
    type Data = ModelRc<StandardListViewItem>;

    fn row_count(&self) -> usize {
        if let Some(filtered_files) = self.filtered_files.borrow().as_ref() {
            return filtered_files.len();
        }
        
        self.ntfs_index.file_count()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        let file_info = if let Some(filtered_files) = self.filtered_files.borrow().as_ref() {
            self.ntfs_index.find_by_index(filtered_files[row])?
        } else {
            self.ntfs_index.find_by_index(row as u64).unwrap()
        };
        
        Some(
            Rc::new(VecModel::from(vec![StandardListViewItem::from(
                slint::format!("{}", self.ntfs_index.compute_full_path(&file_info)),
            ),StandardListViewItem::from(
                slint::format!("{}", file_info.size()),
            )]))
            .into(),
        )
    }

    fn set_row_data(&self, row: usize, data: Self::Data) {
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
