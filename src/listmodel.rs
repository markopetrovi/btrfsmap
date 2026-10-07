//! Lazy list model over one directory's children.
//!
//! A directory can hold a million entries, so rows are materialised only when
//! the view asks for them (and cached, as `ListModel::item` must be stable).

use crate::model::*;
use gtk4 as gtk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

/// Everything a row needs, snapshotted for the currently selected metric.
pub struct RowData {
    pub id: u32,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub share: f64,
    pub files: u64,
    pub mtime: u32,
    pub flags: u8,
    pub freeable: u64,
    pub owned: u64,
    pub color: Option<(f64, f64, f64)>,
}

impl RowData {
    fn build(tree: &Tree, id: u32, metric: Metric, parent_total: u64, color: Option<(f64, f64, f64)>) -> RowData {
        let n = tree.node(id);
        let size = metric.value(tree, id);
        let (files, freeable) = if n.kind == KIND_DIR {
            let d = tree.dir(id);
            (d.nfiles, d.freeable)
        } else {
            (1, n.owned)
        };
        RowData {
            id,
            name: String::from_utf8_lossy(tree.name(id)).into_owned(),
            is_dir: n.kind == KIND_DIR,
            size,
            share: if parent_total > 0 {
                size as f64 * 100.0 / parent_total as f64
            } else {
                0.0
            },
            files,
            mtime: n.mtime,
            flags: n.flags,
            freeable,
            owned: n.owned,
            color,
        }
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct DirListModel {
        pub tree: RefCell<Option<Arc<Tree>>>,
        pub ids: RefCell<Vec<u32>>,
        pub rows: RefCell<HashMap<u32, glib::Object>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DirListModel {
        const NAME: &'static str = "CstDirListModel";
        type Type = super::DirListModel;
        type Interfaces = (gio::ListModel,);
    }

    impl ObjectImpl for DirListModel {}

    impl ListModelImpl for DirListModel {
        fn item_type(&self) -> glib::Type {
            glib::BoxedAnyObject::static_type()
        }
        fn n_items(&self) -> u32 {
            self.ids.borrow().len() as u32
        }
        fn item(&self, position: u32) -> Option<glib::Object> {
            let id = *self.ids.borrow().get(position as usize)?;
            if let Some(o) = self.rows.borrow().get(&id) {
                return Some(o.clone());
            }
            let tree = self.tree.borrow().as_ref().cloned()?;
            let row = self.rows.borrow_mut().remove(&id);
            let _ = row;
            let obj = glib::BoxedAnyObject::new(RowData::build(
                &tree,
                id,
                METRIC.with(|m| m.get()),
                PARENT_TOTAL.with(|t| t.get()),
                COLOR.with(|c| c.borrow().get(&id).copied()),
            ));
            self.rows.borrow_mut().insert(id, obj.clone().upcast());
            Some(obj.upcast())
        }
    }
}

glib::wrapper! {
    pub struct DirListModel(ObjectSubclass<imp::DirListModel>) @implements gio::ListModel;
}

impl Default for DirListModel {
    fn default() -> Self {
        glib::Object::new()
    }
}

thread_local! {
    /// The metric / parent total / colour map the rows are built against.
    /// Thread-local because every row is created on the GTK main thread.
    static METRIC: Cell<Metric> = const { Cell::new(Metric::Owned) };
    static PARENT_TOTAL: Cell<u64> = const { Cell::new(0) };
    static COLOR: RefCell<HashMap<u32, (f64, f64, f64)>> = RefCell::new(HashMap::new());
}

impl DirListModel {
    /// Node id shown at a list position.
    pub fn id_at(&self, position: u32) -> Option<u32> {
        self.imp().ids.borrow().get(position as usize).copied()
    }

    /// Point the model at a directory and rebuild the row order.
    pub fn set_dir(
        &self,
        tree: Arc<Tree>,
        node: u32,
        metric: Metric,
        colors: HashMap<u32, (f64, f64, f64)>,
    ) {
        let mut ids: Vec<u32> = tree.child_ids(node).collect();
        ids.sort_by_key(|&c| {
            let is_dir = tree.node(c).kind == KIND_DIR;
            (!is_dir, std::cmp::Reverse(metric.value(&tree, c)))
        });
        let parent_total = metric.value(&tree, node);
        METRIC.with(|m| m.set(metric));
        PARENT_TOTAL.with(|t| t.set(parent_total));
        COLOR.with(|c| *c.borrow_mut() = colors);
        let old_len = self.imp().ids.borrow().len() as u32;
        *self.imp().ids.borrow_mut() = ids;
        self.imp().rows.borrow_mut().clear();
        *self.imp().tree.borrow_mut() = Some(tree);
        let new_len = self.imp().ids.borrow().len() as u32;
        self.items_changed(0, old_len, new_len);
    }
}
