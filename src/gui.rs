// SPDX-License-Identifier: GPL-2.0-or-later

//! The GTK4 application: scan a path or open a saved scan, watch the scan run,
//! then browse the result as a ring chart, treemap and file list.

use crate::chart::{Chart, View};
use crate::listmodel::{DirListModel, RowData};
use crate::model::*;
use crate::walk::{self, Progress, ScanOptions, ScanStats};
use gtk4 as gtk;
use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{mpsc, Arc};
use std::time::Duration;

enum Msg {
    Scanned(Result<(Tree, ScanStats), String>),
    Loaded(Result<(Tree, PathBuf), String>),
    Saved(Result<PathBuf, String>),
}

struct Ui {
    window: gtk::ApplicationWindow,
    stack: gtk::Stack,
    // scan / load page
    phase: gtk::Label,
    scan_path: gtk::Label,
    scan_bar: gtk::ProgressBar,
    scan_counts: gtk::Label,
    scan_current: gtk::Label,
    cancel_btn: gtk::Button,
    progress: RefCell<Option<Arc<Progress>>>,
    // browse
    chart: Chart,
    model: DirListModel,
    list: gtk::ListView,
    crumbs: gtk::Box,
    metric_dd: gtk::DropDown,
    metric_help: gtk::Label,
    details: gtk::Label,
    status: gtk::Label,
    view_btn: gtk::Button,
    save_btn: gtk::Button,
    up_btn: gtk::Button,
    // state
    tree: RefCell<Option<Arc<Tree>>>,
    node: Cell<u32>,
    metric: Cell<Metric>,
    rx: RefCell<Option<mpsc::Receiver<Msg>>>,
    loading: Cell<bool>,
}

pub fn run(open: Option<PathBuf>) -> ExitCode {
    let app = gtk::Application::builder()
        .application_id("io.github.compsizetree")
        .build();
    let open = RefCell::new(open);
    app.connect_activate(move |app| {
        let ui = build_ui(app);
        let start = open.borrow_mut().take();
        match start {
            Some(path) => ui.open_scan(path),
            None => ui.show_start(),
        }
        ui.window.present();
    });
    app.run_with_args::<&str>(&[]);
    ExitCode::SUCCESS
}

fn is_dark() -> bool {
    match std::env::var("COMPSIZE_TREE_DARK").ok().as_deref() {
        Some("1") => return true,
        Some("0") => return false,
        _ => {}
    }
    {
        let scheme = gio::Settings::new("org.gnome.desktop.interface").string("color-scheme");
        if scheme == "prefer-dark" {
            return true;
        }
        if scheme == "default" {
            return false;
        }
    }
    gtk::Settings::default()
        .map(|s| s.is_gtk_application_prefer_dark_theme())
        .unwrap_or(false)
}

fn install_css() {
    let css = gtk::CssProvider::new();
    css.load_from_data(
        "
        .sharebar trough, .sharebar progress { min-height: 3px; }
        .sharebar { min-height: 3px; margin-top: 1px; }
        .dim { opacity: 0.65; }
        .h1 { font-size: 20px; font-weight: bold; }
        .warn { color: #c01c28; }
        .chip { padding: 1px 6px; border-radius: 6px; background: alpha(currentColor, 0.10); font-size: 11px; }
        .num { font-feature-settings: \"tnum\"; }
        ",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn build_ui(app: &gtk::Application) -> Rc<Ui> {
    install_css();

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();

    // ---- start page -------------------------------------------------------
    let root_warning = gtk::Label::builder()
        .use_markup(true)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .visible(!crate::is_root())
        .build();
    root_warning.set_markup(
        "<span foreground='#c01c28'>Scanning needs root (the btrfs extent ioctl): run \
         <tt>sudo compsize-tree</tt>. Opening a saved scan works as you are.</span>",
    );

    let start_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    let title = gtk::Label::builder()
        .label("compsize-tree")
        .css_classes(["h1"])
        .build();
    let subtitle = gtk::Label::builder()
        .use_markup(true)
        .justify(gtk::Justification::Center)
        .label(
            "Disk usage that counts each physical extent once.\n\
             <span size='small'>Reflinks, snapshots and compression are accounted for, so a \
             btrfs disk no longer reads as 27 TB.</span>",
        )
        .build();
    let open_btn = gtk::Button::with_label("Open a saved scan…");
    let scan_btn = gtk::Button::with_label("Scan a path…");
    scan_btn.add_css_class("suggested-action");
    start_box.append(&title);
    start_box.append(&subtitle);
    start_box.append(&root_warning);
    start_box.append(&open_btn);
    start_box.append(&scan_btn);
    stack.add_named(&start_box, Some("start"));

    // ---- scan / load page -------------------------------------------------
    let phase = gtk::Label::builder().css_classes(["h1"]).build();
    let scan_path = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    let scan_bar = gtk::ProgressBar::builder().show_text(false).build();
    let scan_counts = gtk::Label::builder().css_classes(["num"]).build();
    let scan_current = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(["dim"])
        .build();
    let cancel_btn = gtk::Button::with_label("Cancel");
    let scan_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .width_request(560)
        .build();
    scan_box.append(&phase);
    scan_box.append(&scan_path);
    scan_box.append(&scan_bar);
    scan_box.append(&scan_counts);
    scan_box.append(&scan_current);
    scan_box.append(&cancel_btn);
    stack.add_named(&scan_box, Some("scan"));

    // ---- browse page ------------------------------------------------------
    let chart = Chart::new();
    chart.set_dark(is_dark());
    let details = gtk::Label::builder()
        .use_markup(true)
        .wrap(true)
        .xalign(0.0)
        .valign(gtk::Align::Start)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(12)
        .margin_end(12)
        .build();
    let right = gtk::Box::new(gtk::Orientation::Vertical, 0);
    right.append(&chart.area);
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&details);

    let model = DirListModel::default();
    let selection = gtk::SingleSelection::new(Some(model.clone()));
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.set_margin_top(2);
        row.set_margin_bottom(2);
        row.set_margin_start(6);
        row.set_margin_end(6);
        let swatch = gtk::Label::builder().width_request(14).build();
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_hexpand(true);
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let bar = gtk::ProgressBar::builder()
            .css_classes(["sharebar"])
            .show_text(false)
            .build();
        text.append(&name);
        text.append(&bar);
        let size = gtk::Label::builder().css_classes(["num"]).width_request(86).xalign(1.0).build();
        let share = gtk::Label::builder().css_classes(["num", "dim"]).width_request(58).xalign(1.0).build();
        let items = gtk::Label::builder().css_classes(["num", "dim"]).width_request(74).xalign(1.0).build();
        let when = gtk::Label::builder().css_classes(["dim"]).width_request(84).xalign(1.0).build();
        let tags = gtk::Label::builder().css_classes(["chip"]).build();
        row.append(&swatch);
        row.append(&text);
        row.append(&tags);
        row.append(&size);
        row.append(&share);
        row.append(&items);
        row.append(&when);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, obj| {
        let item = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
        let row = item.child().and_downcast::<gtk::Box>().expect("Box");
        let data = item.item().and_downcast::<glib::BoxedAnyObject>().expect("row data");
        let d = data.borrow::<RowData>();
        let mut cursor = row.first_child();
        let mut next = move || {
            let cur = cursor.clone();
            cursor = cur.as_ref().and_then(|c| c.next_sibling());
            cur
        };
        let swatch = next().and_downcast::<gtk::Label>().unwrap();
        match d.color {
            Some(c) => {
                swatch.set_markup(&format!("<span foreground='{}'>■</span>", hex(c)));
                swatch.set_visible(true);
            }
            None => swatch.set_visible(false),
        }
        let text = next().and_downcast::<gtk::Box>().unwrap();
        let name = text.first_child().and_downcast::<gtk::Label>().unwrap();
        let bar = name.next_sibling().and_downcast::<gtk::ProgressBar>().unwrap();
        name.set_label(&d.name);
        bar.set_fraction((d.share / 100.0).clamp(0.0, 1.0));
        let tags = next().and_downcast::<gtk::Label>().unwrap();
        let mut t = String::new();
        if d.flags & FLAG_SNAPSHOT != 0 {
            t.push_str("snapshot");
        } else if d.flags & FLAG_SUBVOL != 0 {
            t.push_str("subvolume");
        }
        if d.flags & FLAG_MOUNT != 0 {
            t.push_str(" · mount");
        }
        if d.flags & FLAG_MOUNT_OTHER != 0 {
            t.push_str(" · mount (not entered)");
        }
        if d.is_dir && d.freeable < d.owned {
            if !t.is_empty() {
                t.push(' ');
            }
            t.push_str("shared");
        }
        tags.set_label(&t);
        tags.set_visible(!t.is_empty());
        next().and_downcast::<gtk::Label>().unwrap().set_label(&human(d.size));
        next()
            .and_downcast::<gtk::Label>()
            .unwrap()
            .set_label(&format!("{:.1}%", d.share));
        next().and_downcast::<gtk::Label>().unwrap().set_label(&if d.is_dir {
            thousands(d.files)
        } else {
            String::new()
        });
        next().and_downcast::<gtk::Label>().unwrap().set_label(&fmt_when(d.mtime));
    });
    let list = gtk::ListView::builder()
        .model(&selection)
        .factory(&factory)
        .vexpand(true)
        .build();
    list.set_show_separators(true);
    let list_scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let up_btn = gtk::Button::builder().icon_name("go-up-symbolic").tooltip_text("go up one level").build();
    let crumbs = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    let metric_dd = gtk::DropDown::from_strings(&Metric::ALL.map(|m| m.label()));
    let metric_help = gtk::Label::builder()
        .css_classes(["dim"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .tooltip_text(Metric::Owned.help())
        .build();
    let view_btn = gtk::Button::builder().label("Treemap").tooltip_text("switch chart").build();
    let save_btn = gtk::Button::builder().label("Save scan…").build();
    let open_btn2 = gtk::Button::builder().label("Open…").build();
    let head = gtk::CenterBox::new();
    head.set_start_widget(Some(&up_btn));
    head.set_center_widget(Some(&crumbs));
    let head_end = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head_end.append(&metric_help);
    head_end.append(&metric_dd);
    head_end.append(&view_btn);
    head_end.append(&open_btn2);
    head_end.append(&save_btn);
    head.set_end_widget(Some(&head_end));

    let status = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim", "num"])
        .margin_start(8)
        .margin_end(8)
        .margin_top(3)
        .margin_bottom(3)
        .build();

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&list_scroll)
        .end_child(&right)
        .position(620)
        .resize_start_child(true)
        .shrink_start_child(false)
        .vexpand(true)
        .build();
    let browse = gtk::Box::new(gtk::Orientation::Vertical, 0);
    browse.append(&head);
    browse.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    browse.append(&paned);
    browse.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    browse.append(&status);
    stack.add_named(&browse, Some("browse"));

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("compsize-tree")
        .default_width(1400)
        .default_height(900)
        .child(&stack)
        .build();

    let ui = Rc::new(Ui {
        window,
        stack,
        phase,
        scan_path,
        scan_bar,
        scan_counts,
        scan_current,
        cancel_btn,
        progress: RefCell::new(None),
        chart,
        model,
        list,
        crumbs,
        metric_dd,
        metric_help,
        details,
        status,
        view_btn,
        save_btn,
        up_btn,
        tree: RefCell::new(None),
        node: Cell::new(0),
        metric: Cell::new(Metric::Owned),
        rx: RefCell::new(None),
        loading: Cell::new(false),
    });

    // ---- wiring -----------------------------------------------------------
    {
        let ui2 = Rc::clone(&ui);
        open_btn.connect_clicked(move |_| ui2.pick_open());
    }
    {
        let ui2 = Rc::clone(&ui);
        open_btn2.connect_clicked(move |_| ui2.pick_open());
    }
    {
        let ui2 = Rc::clone(&ui);
        scan_btn.connect_clicked(move |_| ui2.pick_scan());
    }
    {
        let ui2 = Rc::clone(&ui);
        ui.save_btn.connect_clicked(move |_| ui2.pick_save());
    }
    {
        let ui2 = Rc::clone(&ui);
        ui.cancel_btn.connect_clicked(move |_| {
            if let Some(p) = ui2.progress.borrow().as_ref() {
                p.cancel();
            }
            ui2.status("cancelling…");
        });
    }
    {
        let ui2 = Rc::clone(&ui);
        ui.up_btn.connect_clicked(move |_| {
            if let Some(tree) = ui2.tree.borrow().as_ref().cloned() {
                let cur = ui2.node.get();
                let parent = tree.node(cur).parent;
                if parent != NO_NODE {
                    ui2.navigate(parent);
                }
            }
        });
    }
    {
        let ui2 = Rc::clone(&ui);
        ui.view_btn.connect_clicked(move |btn| {
            let next = if btn.label().as_deref() == Some("Treemap") {
                ui2.chart.set_view(View::Treemap);
                "Ring chart"
            } else {
                ui2.chart.set_view(View::Ring);
                "Treemap"
            };
            btn.set_label(next);
        });
    }
    {
        let ui2 = Rc::clone(&ui);
        ui.metric_dd.connect_selected_notify(move |dd| {
            let m = Metric::ALL[dd.selected().min(4) as usize];
            ui2.metric.set(m);
            ui2.metric_help.set_label(m.help());
            ui2.metric_help.set_tooltip_text(Some(m.help()));
            ui2.chart.set_metric(m);
            if let Some(tree) = ui2.tree.borrow().as_ref().cloned() {
                let node = ui2.node.get();
                let colors = ui2.chart.state.borrow().colors.clone();
                ui2.model.set_dir(tree, node, m, colors);
            }
        });
    }
    {
        let ui2 = Rc::clone(&ui);
        ui.list.connect_activate(move |list, pos| {
            let Some(tree) = ui2.tree.borrow().as_ref().cloned() else { return };
            let id = ui2.model.id_at(pos).or_else(|| {
                list.model()
                    .and_then(|m| m.item(pos))
                    .and_downcast::<glib::BoxedAnyObject>()
                    .map(|o| o.borrow::<RowData>().id)
            });
            let Some(id) = id else { return };
            if tree.node(id).kind == KIND_DIR {
                ui2.navigate(id);
            }
        });
    }
    {
        let ui2 = Rc::clone(&ui);
        let sel = selection.clone();
        sel.connect_selected_notify(move |s| {
            let pos = s.selected();
            let id = if pos == u32::MAX { None } else { ui2.model.id_at(pos) };
            ui2.chart.state.borrow_mut().selected = id;
            if let Some(id) = id {
                ui2.describe(id);
            }
            ui2.chart.area.queue_draw();
        });
    }
    {
        let inner = Rc::clone(&ui);
        ui.chart.state.borrow_mut().on_click = Some(Rc::new(move |id: u32| {
            let Some(tree) = inner.tree.borrow().as_ref().cloned() else { return };
            if tree.node(id).kind == KIND_DIR {
                inner.navigate(id);
            }
        }));
    }
    {
        let inner = Rc::clone(&ui);
        ui.chart.state.borrow_mut().on_hover = Some(Rc::new(move |id: Option<u32>| {
            if let Some(id) = id {
                inner.describe(id);
            }
        }));
    }
    {
        let ui2 = Rc::clone(&ui);
        glib::timeout_add_local(Duration::from_millis(200), move || {
            ui2.poll();
            glib::ControlFlow::Continue
        });
    }
    ui
}

fn hex(c: (f64, f64, f64)) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.0 * 255.0).round().clamp(0.0, 255.0) as u8,
        (c.1 * 255.0).round().clamp(0.0, 255.0) as u8,
        (c.2 * 255.0).round().clamp(0.0, 255.0) as u8
    )
}

fn fmt_when(t: u32) -> String {
    if t == 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let age = now.saturating_sub(t as u64);
    if age < 3600 {
        format!("{} min ago", (age / 60).max(1))
    } else if age < 86400 {
        format!("{} h ago", age / 3600)
    } else if age < 86400 * 60 {
        format!("{} d ago", age / 86400)
    } else {
        format!("{} mo ago", age / (86400 * 30))
    }
}

impl Ui {
    fn status(&self, msg: &str) {
        self.status.set_label(msg);
    }

    fn show_start(&self) {
        self.stack.set_visible_child_name("start");
        self.status("");
    }

    // ---- scanning ---------------------------------------------------------
    fn pick_scan(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Choose a directory to scan")
            .modal(true)
            .build();
        let ui = Rc::clone(self);
        glib::spawn_future_local(async move {
            if let Ok(file) = dialog.select_folder_future(Some(&ui.window)).await {
                if let Some(path) = file.path() {
                    ui.start_scan(path);
                }
            }
        });
    }

    fn start_scan(self: &Rc<Self>, path: PathBuf) {
        if !crate::is_root() {
            self.show_start();
            self.status(
                "scanning reads extent data through an ioctl that needs root \
                 - restart with: sudo compsize-tree",
            );
            return;
        }
        self.phase.set_label("Scanning");
        self.scan_path.set_label(&path.to_string_lossy());
        self.scan_bar.set_text(Some("starting…"));
        self.scan_bar.pulse();
        self.scan_counts.set_label("");
        self.scan_current.set_label("");
        self.cancel_btn.set_visible(true);
        self.stack.set_visible_child_name("scan");
        self.status("");

        let progress = Arc::new(Progress::default());
        *self.progress.borrow_mut() = Some(progress.clone());
        self.loading.set(false);
        let (tx, rx) = mpsc::channel();
        *self.rx.borrow_mut() = Some(rx);
        let path2 = path.clone();
        std::thread::spawn(move || {
            let opts = ScanOptions {
                roots: vec![path2.as_os_str().as_encoded_bytes().to_vec()],
                live: Some(progress.clone()),
                ..Default::default()
            };
            let msg = match walk::scan_tree(&opts) {
                Ok((tree, stats)) => Msg::Scanned(Ok((tree, stats))),
                Err(e) => Msg::Scanned(Err(e.to_string())),
            };
            let _ = tx.send(msg);
        });
    }

    // ---- opening a saved scan --------------------------------------------
    fn pick_open(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Open a saved scan")
            .modal(true)
            .build();
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("compsize-tree scans"));
        filter.add_pattern("*.cst");
        filter.add_pattern("*.cst.zst");
        filter.add_pattern("*");
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        let ui = Rc::clone(self);
        glib::spawn_future_local(async move {
            if let Ok(file) = dialog.open_future(Some(&ui.window)).await {
                if let Some(path) = file.path() {
                    ui.open_scan(path);
                }
            }
        });
    }

    fn open_scan(self: &Rc<Self>, path: PathBuf) {
        self.phase.set_label("Opening saved scan");
        self.scan_path.set_label(&path.to_string_lossy());
        self.scan_bar.set_text(Some("reading…"));
        self.scan_bar.pulse();
        self.scan_counts.set_label("decompressing and loading; a large scan takes a few seconds");
        self.scan_current.set_label("");
        self.scan_bar.set_show_text(false);
        self.loading.set(true);
        self.cancel_btn.set_visible(false);
        self.stack.set_visible_child_name("scan");
        self.status("");
        self.loading.set(true);

        let (tx, rx) = mpsc::channel();
        *self.rx.borrow_mut() = Some(rx);
        let path2 = path.clone();
        std::thread::spawn(move || {
            let msg = match crate::cache::Cache::load_auto(&path2) {
                Ok(c) => Msg::Loaded(Ok((c.into_tree(), path2))),
                Err(e) => Msg::Loaded(Err(e.to_string())),
            };
            let _ = tx.send(msg);
        });
    }

    // ---- saving -----------------------------------------------------------
    fn pick_save(self: &Rc<Self>) {
        let Some(tree) = self.tree.borrow().as_ref().cloned() else { return };
        let name = tree
            .scanned_roots
            .first()
            .map(|r| {
                let s = String::from_utf8_lossy(r);
                let slug = s.trim_matches('/').replace('/', "_");
                format!("{}.cst", if slug.is_empty() { "scan".into() } else { slug })
            })
            .unwrap_or_else(|| "scan.cst".into());
        let dialog = gtk::FileDialog::builder()
            .title("Save scan")
            .modal(true)
            .initial_name(&name)
            .build();
        let ui = Rc::clone(self);
        glib::spawn_future_local(async move {
            if let Ok(file) = dialog.save_future(Some(&ui.window)).await {
                if let Some(path) = file.path() {
                    ui.save_scan(tree, path);
                }
            }
        });
    }

    fn save_scan(self: &Rc<Self>, tree: Arc<Tree>, path: PathBuf) {
        self.loading.set(true);
        self.phase.set_label("Saving scan");
        self.scan_path.set_label(&path.to_string_lossy());
        self.scan_bar.set_text(Some("writing…"));
        self.scan_bar.pulse();
        self.scan_counts.set_label("compressing with zstd if it is available");
        self.scan_current.set_label("");
        self.cancel_btn.set_visible(false);
        self.stack.set_visible_child_name("scan");
        let (tx, rx) = mpsc::channel();
        *self.rx.borrow_mut() = Some(rx);
        *self.progress.borrow_mut() = None;
        std::thread::spawn(move || {
            let c = crate::cache::Cache::from_tree(&tree);
            let msg = match c
                .write_compressed(&path)
                .or_else(|_| c.write(&path))
            {
                Ok(()) => {
                    crate::hand_over(&path);
                    Msg::Saved(Ok(path))
                }
                Err(e) => Msg::Saved(Err(e.to_string())),
            };
            let _ = tx.send(msg);
        });
    }

    // ---- polling the worker ----------------------------------------------
    fn poll(self: &Rc<Self>) {
        let loading = self.loading.get();
        let msg = {
            let rx = self.rx.borrow();
            match rx.as_ref().and_then(|r| r.try_recv().ok()) {
                Some(m) => m,
                None => {
                    if let Some(p) = self.progress.borrow().as_ref() {
                        if loading {
                            self.scan_bar.pulse();
                            return;
                        }
                        let (files, dirs, _refs, running, cur) = p.snapshot();
                        self.scan_bar.pulse();
                        self.scan_counts.set_label(&format!(
                            "{} files · {} dirs",
                            thousands(files),
                            thousands(dirs)
                        ));
                        if !cur.is_empty() {
                            self.scan_current.set_label(&cur);
                        }
                        if !running && files == 0 {
                            self.scan_current.set_label("waiting for the first directory…");
                        }
                    }
                    return;
                }
            }
        };
        *self.rx.borrow_mut() = None;
        *self.progress.borrow_mut() = None;
        self.loading.set(false);
        match msg {
            Msg::Scanned(Ok((tree, stats))) => {
                self.loaded(tree, None, Some(stats));
            }
            Msg::Scanned(Err(e)) => {
                self.show_start();
                self.status(&format!("scan failed: {e}"));
            }
            Msg::Loaded(Ok((tree, path))) => {
                self.loaded(tree, Some(path), None);
            }
            Msg::Loaded(Err(e)) => {
                self.show_start();
                self.status(&format!("could not open: {e}"));
            }
            Msg::Saved(Ok(path)) => {
                self.stack.set_visible_child_name("browse");
                self.status(&format!(
                    "saved {} ({})",
                    path.display(),
                    human(crate::cache::size_of(&path))
                ));
            }
            Msg::Saved(Err(e)) => {
                self.stack.set_visible_child_name("browse");
                self.status(&format!("could not save: {e}"));
            }
        }
    }

    fn loaded(self: &Rc<Self>, tree: Tree, path: Option<PathBuf>, stats: Option<ScanStats>) {
        let stats = stats.unwrap_or_else(|| tree.stats.clone());
        let tree = Arc::new(tree);
        *self.tree.borrow_mut() = Some(tree.clone());
        let root = tree.roots.first().copied().unwrap_or(tree.root);
        self.node.set(root);
        self.stack.set_visible_child_name("browse");
        self.save_btn.set_sensitive(true);
        let snap_note = {
            let mut snap_owned = 0u64;
            let mut snap_free = 0u64;
            let mut n = 0u64;
            for i in 0..tree.nodes.len() as u32 {
                let nd = tree.node(i);
                if nd.kind == KIND_DIR && nd.flags & FLAG_SNAPSHOT != 0 {
                    snap_owned += nd.owned;
                    snap_free += tree.dir(i).freeable;
                    n += 1;
                }
            }
            if n > 0 {
                format!(" · snapshots: {n} hold {}, {} freeable", human(snap_owned), human(snap_free))
            } else {
                String::new()
            }
        };
        let extra = if stats.skipped_otherfs > 0 || stats.skipped_nonbtrfs > 0 {
            format!(
                " · {} non-btrfs and {} other btrfs filesystems skipped",
                stats.skipped_nonbtrfs, stats.skipped_otherfs
            )
        } else {
            String::new()
        };
        let src = match &path {
            Some(p) => format!("opened {}", p.display()),
            None => format!(
                "scanned {}",
                tree.scanned_roots
                    .iter()
                    .map(|r| String::from_utf8_lossy(r).into_owned())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        self.status(&format!(
            "{src} · {} files · {} extents · {} references · {} dirs · {:.0}s{}{}",
            thousands(stats.files),
            thousands(stats.extents),
            thousands(stats.refs),
            thousands(stats.dirs),
            stats.elapsed.as_secs_f64(),
            snap_note,
            extra
        ));
        self.navigate(root);
    }

    // ---- browsing ---------------------------------------------------------
    fn navigate(self: &Rc<Self>, node: u32) {
        let Some(tree) = self.tree.borrow().as_ref().cloned() else { return };
        self.node.set(node);
        let metric = self.metric.get();
        self.chart.show(tree.clone(), node);
        let colors = self.chart.state.borrow().colors.clone();
        self.model.set_dir(tree.clone(), node, metric, colors);
        self.rebuild_crumbs(&tree, node);
        self.describe(node);
        self.up_btn.set_sensitive(tree.node(node).parent != NO_NODE);
    }

    fn rebuild_crumbs(self: &Rc<Self>, tree: &Arc<Tree>, node: u32) {
        while let Some(child) = self.crumbs.first_child() {
            self.crumbs.remove(&child);
        }
        let mut chain = Vec::new();
        let mut cur = node;
        while cur != NO_NODE {
            chain.push(cur);
            if cur == tree.root {
                break;
            }
            cur = tree.node(cur).parent;
        }
        chain.reverse();
        let ui = Rc::clone(self);
        for (i, id) in chain.iter().enumerate() {
            if i > 0 {
                let sep = gtk::Label::new(Some("/"));
                sep.add_css_class("dim");
                self.crumbs.append(&sep);
            }
            let name = if *id == tree.root {
                "/".to_string()
            } else {
                String::from_utf8_lossy(tree.name(*id)).into_owned()
            };
            let btn = gtk::Button::builder()
                .label(&name)
                .has_frame(false)
                .tooltip_text(format!("{}", human(tree.node(*id).owned)))
                .build();
            let id = *id;
            let ui2 = Rc::clone(&ui);
            btn.connect_clicked(move |_| ui2.navigate(id));
            self.crumbs.append(&btn);
        }
    }

    fn describe(&self, id: u32) {
        let Some(tree) = self.tree.borrow().as_ref().cloned() else { return };
        let n = tree.node(id);
        let name = String::from_utf8_lossy(tree.name(id)).into_owned();
        let mut text = format!(
            "<b>{}</b>\n<span size='small'>owned {} · compsize {} · freeable {} · referenced {}</span>",
            glib::markup_escape_text(if name.is_empty() { "/" } else { &name }),
            human(n.owned),
            human(Metric::Compsize.value(&tree, id)),
            human(Metric::Freeable.value(&tree, id)),
            human(n.referenced),
        );
        if n.kind == KIND_DIR {
            let d = tree.dir(id);
            text.push_str(&format!(
                "\n<span size='small'>{} files · {} dirs · uncompressed {} · {}% of referenced</span>",
                thousands(d.nfiles),
                thousands(d.ndirs),
                human(Metric::Uncompressed.value(&tree, id)),
                if n.referenced > 0 {
                    (n.owned as f64 * 100.0 / n.referenced as f64).round() as u64
                } else {
                    0
                }
            ));
            if d.freeable < n.owned {
                text.push_str(&format!(
                    "\n<span size='small'>shared: only {} would be freed by deleting it</span>",
                    human(d.freeable)
                ));
            }
        }
        if n.flags & FLAG_MOUNT != 0 {
            text.push_str(
                "\n<span size='small'>a subvolume is mounted here: its tree begins at this \
                 node, and the directory the containing tree holds is only where it is \
                 mounted over</span>",
            );
        }
        if n.flags & FLAG_MOUNT_OTHER != 0 {
            text.push_str(
                "\n<span size='small'>a mount point this scan did not read: the same \
                 subvolume is counted at another of its mounts, or the mounted filesystem \
                 is not this one and is never entered</span>",
            );
        }
        self.details.set_markup(&text);
    }

}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('.');
        }
        out.push(c);
    }
    out
}
