// The main window shell: header, tabs, and startup.
// navigation.rs moves between folders. commands.rs acts on the selection.
// places.rs is the sidebar. view.rs is the grid and list.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{archive, bookmarks, format, preview, settings, trash, undo};

mod commands;
mod navigation;
mod places;
mod view;


const CSS: &str = "
.fs-grid { padding: 8px; }
.fs-grid > child { border-radius: 12px; padding: 8px; margin: 2px; transition: background 120ms; }
.fs-grid > child:hover { background: alpha(@window_fg_color, 0.06); }
.fs-grid > child:selected { background: alpha(@accent_bg_color, 0.22); }
.fs-drop { background: alpha(@accent_bg_color, 0.35); border-radius: 8px; outline: 2px solid @accent_color; outline-offset: -2px; }
.fs-name { margin-top: 4px; }
.fs-hidden { opacity: 0.45; }
columnview > listview > row { border-radius: 8px; }
.crumbs button { padding: 3px 8px; min-height: 0; }
.crumbs button.current { font-weight: bold; }
.drive-card { padding: 14px; border-radius: 12px; }
.drive-bar { min-height: 8px; }
.drive-bar > trough, .drive-bar > trough > progress { min-height: 8px; border-radius: 8px; }
.bookmark-remove { min-height: 0; min-width: 0; padding: 2px; }
.bookmark-x { opacity: 0; transition: opacity 120ms; }
row:hover .bookmark-x,
.bookmark-x:focus-visible { opacity: 1; }
.bookmark-drop { background: alpha(@accent_bg_color, 0.22); }
";

const ATTRS: &str = "standard::*,time::modified,unix::mode";

/// Zoom bounds for the grid icon size (pixels); list scales at a quarter.
const ZOOM_MIN: i32 = 48;
const ZOOM_MAX: i32 = 160;
const ZOOM_STEP: i32 = 16;

/// Clipboard mime types for file cut/copy: GNOME's copy/cut format (carries the
/// cut-vs-copy intent and interoperates with Nautilus/Files) and the generic
/// URI list (understood by most apps).
const GNOME_COPIED: &str = "x-special/gnome-copied-files";
const URI_LIST: &str = "text/uri-list";

/// One folder tab: its own listing, views, history, and breadcrumb.
struct Tab {
    page: RefCell<Option<adw::TabPage>>,
    dir_list: gtk::DirectoryList,
    filter: gtk::CustomFilter,
    selection: gtk::MultiSelection,
    view_stack: gtk::Stack,
    grid_view: gtk::GridView,
    column_view: gtk::ColumnView,
    breadcrumb: gtk::Box,
    computer_box: gtk::Box,
    trash_box: gtk::Box,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    back: RefCell<Vec<gio::File>>,
    fwd: RefCell<Vec<gio::File>>,
}

/// The whole window and its shared state.
struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    tab_view: adw::TabView,
    sidebar_list: gtk::ListBox,
    back_btn: gtk::Button,
    fwd_btn: gtk::Button,
    up_btn: gtk::Button,
    status: gtk::Label,
    tabs: RefCell<Vec<Rc<Tab>>>,
    /// Grid icon size in pixels (list scales from it); shared view mode.
    zoom: Cell<i32>,
    is_list: Cell<bool>,
    show_hidden: Rc<Cell<bool>>,
    bookmarks: RefCell<Vec<PathBuf>>,
    preview: RefCell<Option<adw::Window>>,
    /// Shared sort: which column (0 = name, 1 = size, 2 = modified) and whether
    /// descending. Applied to the active tab (both grid and list share one sort
    /// model), and seeded into new tabs.
    sort_key: Cell<u8>,
    sort_desc: Cell<bool>,
    /// Bottom area that reveals a row per in-flight background operation.
    progress_box: gtk::Box,
    progress_revealer: gtk::Revealer,
    /// Reversible history. Doing something new clears the redo side.
    undo_stack: RefCell<Vec<undo::Action>>,
    redo_stack: RefCell<Vec<undo::Action>>,
}

const STATE_KEY: &str = "filescope-state";

fn app_state(app: &adw::Application) -> Option<Rc<App>> {
    let data = unsafe { app.data::<Rc<App>>(STATE_KEY) }?;
    Some(unsafe { data.as_ref() }.clone())
}

/// Open `files` in the running window, creating it if this is the first launch.
/// An empty list shows This PC. A second launch with a folder adds a tab.
pub fn open_files(app: &adw::Application, files: &[gio::File]) {
    let fresh = app_state(app).is_none();
    if fresh {
        build(app);
    }
    let Some(state) = app_state(app) else { return };
    if fresh && files.is_empty() {
        navigation::new_tab(&state, gio::File::for_path(glib::home_dir()), true);
        places::show_computer(&state, &navigation::active_tab(&state));
    } else {
        for file in files {
            open_gfile(&state, file);
        }
    }
    state.window.present();
    if fresh {
        maybe_capture(app, &state);
    }
}

/// Open folders from the file-manager bus (`ShowFolders` / `ShowItems`).
pub fn open_targets(app: &adw::Application, targets: &[crate::file_manager::Target]) {
    if app_state(app).is_none() {
        open_files(app, &[]);
    }
    let Some(state) = app_state(app) else { return };
    for target in targets {
        navigation::new_tab(&state, gio::File::for_path(&target.dir), true);
        if target.names.is_empty() {
            continue;
        }
        let tab = navigation::active_tab(&state);
        let props = target.properties && target.names.len() == 1;
        let state = state.clone();
        navigation::select_names(&tab, &target.names, move || {
            if props {
                commands::show_properties(&state);
            }
        });
    }
    state.window.present();
}

fn open_gfile(state: &Rc<App>, file: &gio::File) {
    let Some(path) = file.path() else { return };
    if path.is_dir() {
        navigation::new_tab(state, gio::File::for_path(path), true);
        return;
    }
    let Some(parent) = path.parent() else { return };
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if name.is_empty() {
        return;
    }
    navigation::new_tab(state, gio::File::for_path(parent), true);
    navigation::select_names(&navigation::active_tab(state), &[name], || {});
}

fn build(app: &adw::Application) {
    install_css();

    let back_btn = flat_icon("go-previous-symbolic", "Back (Alt+Left)");
    let fwd_btn = flat_icon("go-next-symbolic", "Forward (Alt+Right)");
    let up_btn = flat_icon("go-up-symbolic", "Up (Alt+Up)");
    let sidebar_toggle =
        gtk::ToggleButton::builder().icon_name("sidebar-show-symbolic").active(true).tooltip_text("Toggle sidebar").build();
    sidebar_toggle.add_css_class("flat");
    let new_tab_btn = flat_icon("tab-new-symbolic", "New tab (Ctrl+T)");
    let view_toggle =
        gtk::ToggleButton::builder().icon_name("view-list-symbolic").tooltip_text("List view").build();
    view_toggle.add_css_class("flat");
    let sort_btn =
        gtk::MenuButton::builder().icon_name("view-sort-ascending-symbolic").tooltip_text("Sort by").build();
    sort_btn.add_css_class("flat");
    sort_btn.set_menu_model(Some(&sort_menu()));
    let zoom_out = flat_icon("zoom-out-symbolic", "Zoom out (Ctrl+-)");
    let zoom_in = flat_icon("zoom-in-symbolic", "Zoom in (Ctrl++)");
    let menu_btn = gtk::MenuButton::builder().icon_name("open-menu-symbolic").build();
    menu_btn.set_menu_model(Some(&primary_menu()));

    let header = adw::HeaderBar::new();
    header.pack_start(&sidebar_toggle);
    header.pack_start(&back_btn);
    header.pack_start(&fwd_btn);
    header.pack_start(&up_btn);
    header.pack_start(&new_tab_btn);
    header.pack_end(&menu_btn);
    header.pack_end(&sort_btn);
    header.pack_end(&view_toggle);
    header.pack_end(&zoom_in);
    header.pack_end(&zoom_out);

    let tab_view = adw::TabView::new();
    // Autohide: the tab bar disappears when only one tab is open (no point
    // showing a single-tab strip) and returns as soon as a second tab exists.
    let tab_bar = adw::TabBar::builder().view(&tab_view).autohide(true).expand_tabs(false).build();

    let status = gtk::Label::builder().xalign(0.0).build();
    status.add_css_class("dim-label");
    status.add_css_class("caption");
    status.set_margin_start(14);
    status.set_margin_top(5);
    status.set_margin_bottom(5);
    let status_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    status_bar.append(&status);

    // Background-operation progress: a revealer holding one row per running job.
    let progress_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    progress_box.set_margin_start(12);
    progress_box.set_margin_end(12);
    progress_box.set_margin_top(6);
    progress_box.set_margin_bottom(6);
    let progress_revealer =
        gtk::Revealer::builder().child(&progress_box).reveal_child(false).build();

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&tab_view));
    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.add_top_bar(&tab_bar);
    content.set_content(Some(&toasts));
    content.add_bottom_bar(&progress_revealer);
    content.add_bottom_bar(&status_bar);

    let sidebar_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    sidebar_list.add_css_class("navigation-sidebar");
    let sidebar_scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&sidebar_list).build();
    let sidebar_header = adw::HeaderBar::builder().show_end_title_buttons(false).build();
    sidebar_header.set_title_widget(Some(&adw::WindowTitle::new("filescope", "")));
    let sidebar = adw::ToolbarView::new();
    sidebar.add_top_bar(&sidebar_header);
    sidebar.set_content(Some(&sidebar_scroller));

    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .min_sidebar_width(190.0)
        .max_sidebar_width(300.0)
        .sidebar_width_fraction(0.22)
        .build();
    sidebar_toggle.bind_property("active", &split, "show-sidebar").bidirectional().sync_create().build();

    // Preferences from the last session (view, zoom, sort, hidden, geometry).
    let cfg = settings::load();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("filescope")
        .default_width(cfg.width)
        .default_height(cfg.height)
        .content(&split)
        .build();
    if cfg.maximized {
        window.maximize();
    }

    let state = Rc::new(App {
        window: window.clone(),
        toasts,
        tab_view: tab_view.clone(),
        sidebar_list: sidebar_list.clone(),
        back_btn: back_btn.clone(),
        fwd_btn: fwd_btn.clone(),
        up_btn: up_btn.clone(),
        status,
        tabs: RefCell::new(Vec::new()),
        zoom: Cell::new(cfg.zoom),
        is_list: Cell::new(cfg.is_list),
        show_hidden: Rc::new(Cell::new(cfg.show_hidden)),
        bookmarks: RefCell::new(bookmarks::load()),
        preview: RefCell::new(None),
        sort_key: Cell::new(cfg.sort_key),
        sort_desc: Cell::new(cfg.sort_desc),
        progress_box,
        progress_revealer,
        undo_stack: RefCell::new(Vec::new()),
        redo_stack: RefCell::new(Vec::new()),
    });

    install_actions(app, &state);
    places::populate_sidebar(&state);

    // Reflect the loaded preferences in the widgets/actions that display them.
    view_toggle.set_active(cfg.is_list);
    commands::set_action_state(&state, "toggle-hidden", &cfg.show_hidden.to_variant());
    commands::set_action_state(&state, "sort-descending", &cfg.sort_desc.to_variant());
    let sort_name = match cfg.sort_key {
        1 => "size",
        2 => "modified",
        _ => "name",
    };
    commands::set_action_state(&state, "sort", &sort_name.to_variant());

    // Remember preferences for next launch.
    {
        let state = state.clone();
        window.connect_close_request(move |window| {
            preview::close_sushi(window);
            settings::save(&settings::Settings {
                is_list: state.is_list.get(),
                zoom: state.zoom.get(),
                sort_key: state.sort_key.get(),
                sort_desc: state.sort_desc.get(),
                show_hidden: state.show_hidden.get(),
                width: window.default_width(),
                height: window.default_height(),
                maximized: window.is_maximized(),
            });
            glib::Propagation::Proceed
        });
    }

    // Chrome follows the active tab.
    {
        let state = state.clone();
        tab_view.connect_selected_page_notify(move |_| update_chrome(&state));
    }
    // Close a tab (keep at least one).
    {
        let state = state.clone();
        tab_view.connect_close_page(move |tv, page| {
            if tv.n_pages() <= 1 {
                tv.close_page_finish(page, false);
            } else {
                state.tabs.borrow_mut().retain(|t| t.page.borrow().as_ref() != Some(page));
                tv.close_page_finish(page, true);
            }
            glib::Propagation::Stop
        });
    }
    // Header controls.
    {
        let s = state.clone();
        back_btn.connect_clicked(move |_| { let t = navigation::active_tab(&s); navigation::go_back(&s, &t); });
    }
    {
        let s = state.clone();
        fwd_btn.connect_clicked(move |_| { let t = navigation::active_tab(&s); navigation::go_forward(&s, &t); });
    }
    {
        let s = state.clone();
        up_btn.connect_clicked(move |_| { let t = navigation::active_tab(&s); navigation::go_up(&s, &t); });
    }
    {
        let s = state.clone();
        new_tab_btn.connect_clicked(move |_| { navigation::new_tab(&s, gio::File::for_path(glib::home_dir()), true); });
    }
    {
        let s = state.clone();
        view_toggle.connect_toggled(move |b| {
            let list = b.is_active();
            s.is_list.set(list);
            b.set_icon_name(if list { "view-grid-symbolic" } else { "view-list-symbolic" });
            b.set_tooltip_text(Some(if list { "Grid view" } else { "List view" }));
            for t in s.tabs.borrow().iter() {
                navigation::refresh_view_mode(&s, t, false);
            }
        });
    }
    {
        let s = state.clone();
        zoom_in.connect_clicked(move |_| zoom(&s, ZOOM_STEP));
    }
    {
        let s = state.clone();
        zoom_out.connect_clicked(move |_| zoom(&s, -ZOOM_STEP));
    }
    {
        let s = state.clone();
        sidebar_list.connect_row_activated(move |_, row| {
            let t = navigation::active_tab(&s);
            if unsafe { row.data::<bool>("computer") }.is_some() {
                places::show_computer(&s, &t);
            } else if unsafe { row.data::<bool>("trash") }.is_some() {
                places::show_trash(&s, &t);
            } else if let Some(volume) = unsafe { row.data::<gio::Volume>("volume") } {
                places::mount_and_open(&s, &t, unsafe { volume.as_ref() });
            } else if let Some(path) = unsafe { row.data::<PathBuf>("path") } {
                navigation::navigate(&s, &t, gio::File::for_path(unsafe { path.as_ref() }.clone()));
            }
        });
    }

    // Keep the Paste action's enabled state in step with the system clipboard,
    // including copies made in other apps.
    {
        let s = state.clone();
        state.window.clipboard().connect_changed(move |_| update_chrome(&s));
    }

    // Keep the sidebar's device list live as drives are plugged in, mounted, or
    // removed. The handlers are owned by the (process-lifetime) monitor singleton.
    {
        let monitor = gio::VolumeMonitor::get();
        let s = state.clone();
        monitor.connect_mount_added(move |_, _| places::populate_sidebar(&s));
        let s = state.clone();
        monitor.connect_mount_removed(move |_, _| places::populate_sidebar(&s));
        let s = state.clone();
        monitor.connect_volume_added(move |_, _| places::populate_sidebar(&s));
        let s = state.clone();
        monitor.connect_volume_removed(move |_, _| places::populate_sidebar(&s));
    }

    unsafe { app.set_data(STATE_KEY, state.clone()) };
}

/// Dev/docs hook: if `FILESCOPE_SHOT` is set, render the window to that PNG in
/// process once it has settled, then quit — for generating README screenshots on
/// systems where the compositor blocks normal grabs. `FILESCOPE_SHOT_VIEW`
/// (`grid` | `list` | `computer`) selects which view to capture. No effect on
/// normal runs.
fn maybe_capture(app: &adw::Application, state: &Rc<App>) {
    let Ok(path) = std::env::var("FILESCOPE_SHOT") else {
        return;
    };
    if let Ok(view) = std::env::var("FILESCOPE_SHOT_VIEW") {
        let tab = navigation::active_tab(state);
        match view.as_str() {
            "list" => {
                state.is_list.set(true);
                tab.view_stack.set_visible_child_name("list");
            }
            "grid" => {
                state.is_list.set(false);
                tab.view_stack.set_visible_child_name("grid");
            }
            "computer" => places::show_computer(state, &tab),
            "trash" => places::show_trash(state, &tab),
            _ => {}
        }
    }

    let window = state.window.clone();
    let app = app.clone();
    // Give the listing and async thumbnails a moment to render before grabbing.
    glib::timeout_add_local_once(std::time::Duration::from_millis(1800), move || {
        let (w, h) = (window.width().max(1), window.height().max(1));
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, w as f64, h as f64);
        if let (Some(node), Some(renderer)) =
            (snapshot.to_node(), window.native().and_then(|n| n.renderer()))
        {
            let texture = renderer.render_texture(&node, None);
            match texture.save_to_png(&path) {
                Ok(()) => eprintln!("saved screenshot to {path}"),
                Err(err) => eprintln!("screenshot failed: {err}"),
            }
        }
        app.quit();
    });
}


/// Whether `name` is an executable found on `PATH`.
fn has_binary(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

// --- Zoom -------------------------------------------------------------------

fn zoom(app: &Rc<App>, delta: i32) {
    let new = (app.zoom.get() + delta).clamp(ZOOM_MIN, ZOOM_MAX);
    if new == app.zoom.get() {
        return;
    }
    app.zoom.set(new);
    for tab in app.tabs.borrow().iter() {
        tab.grid_view.set_factory(Some(&view::grid_factory(new)));
        view::fill_columns(&tab.column_view, view::list_icon(new));
        if let Some(sorter) = tab.column_view.sorter()
            && let Some(sm) = tab.selection.model().and_downcast::<gtk::SortListModel>()
        {
            sm.set_sorter(Some(&view::with_hidden_last(&sorter)));
        }
        // Rebuilding the columns clears the active sort column; restore it.
        apply_sort_to(app, tab);
    }
}


/// Keyboard accelerators, as (action, keys).
///
/// These are window-wide: GTK dispatches them **before** the focused widget, so a
/// bare (unmodified) key here would be stolen from every text entry — you could
/// not type it while renaming, searching, or editing the path. Only function keys
/// are safe bare; Space / Backspace / Delete / Enter act on the view instead
/// (see `attach_view_keys`). `no_accelerator_steals_a_typing_key` enforces this.
const ACCELS: &[(&str, &[&str])] = &[
    ("back", &["<alt>Left"]),
    ("forward", &["<alt>Right"]),
    ("up", &["<alt>Up"]),
    ("home", &["<alt>Home"]),
    ("copy", &["<ctrl>c"]),
    ("cut", &["<ctrl>x"]),
    ("paste", &["<ctrl>v"]),
    ("rename", &["F2"]),
    ("new-folder", &["<ctrl><shift>n"]),
    ("new-tab", &["<ctrl>t"]),
    ("close-tab", &["<ctrl>w"]),
    ("select-all", &["<ctrl>a"]),
    ("refresh", &["<ctrl>r", "F5"]),
    ("find", &["<ctrl>f"]),
    ("toggle-hidden", &["<ctrl>h"]),
    ("bookmark", &["<ctrl>d"]),
    ("undo", &["<ctrl>z"]),
    ("redo", &["<ctrl><shift>z", "<ctrl>y"]),
    ("open-terminal", &["<ctrl>period"]),
    ("zoom-in", &["<ctrl>plus", "<ctrl>equal"]),
    ("zoom-out", &["<ctrl>minus"]),
];

// --- Actions ----------------------------------------------------------------

fn install_actions(app: &adw::Application, state: &Rc<App>) {
    macro_rules! action {
        ($name:literal, $body:expr) => {{
            let act = gio::SimpleAction::new($name, None);
            let s = state.clone();
            act.connect_activate(move |_, _| $body(&s));
            state.window.add_action(&act);
        }};
    }
    action!("back", |s: &Rc<App>| { let t = navigation::active_tab(s); navigation::go_back(s, &t); });
    action!("forward", |s: &Rc<App>| { let t = navigation::active_tab(s); navigation::go_forward(s, &t); });
    action!("up", |s: &Rc<App>| { let t = navigation::active_tab(s); navigation::go_up(s, &t); });
    action!("home", |s: &Rc<App>| { let t = navigation::active_tab(s); navigation::navigate(s, &t, gio::File::for_path(glib::home_dir())); });
    action!("open", commands::open_selected);
    action!("open-with", commands::open_with);
    action!("preview", commands::toggle_preview);
    action!("copy", |s: &Rc<App>| commands::set_clipboard(s, false));
    action!("cut", |s: &Rc<App>| commands::set_clipboard(s, true));
    action!("paste", commands::paste);
    action!("rename", commands::rename_selected);
    action!("trash", commands::trash_selected);
    action!("delete", commands::delete_selected);
    action!("new-folder", commands::new_folder);
    action!("new-tab", |s: &Rc<App>| { navigation::new_tab(s, gio::File::for_path(glib::home_dir()), true); });
    action!("close-tab", |s: &Rc<App>| { let t = navigation::active_tab(s); if let Some(p) = t.page.borrow().as_ref() { s.tab_view.close_page(p); } });
    action!("select-all", |s: &Rc<App>| { navigation::active_tab(s).selection.select_all(); });
    action!("refresh", |s: &Rc<App>| { navigation::refresh(&navigation::active_tab(s)); });
    action!("find", |s: &Rc<App>| {
        let t = navigation::active_tab(s);
        let show = !t.search_bar.is_search_mode();
        t.search_bar.set_search_mode(show);
        if show {
            t.search_entry.grab_focus();
        }
    });
    action!("properties", commands::show_properties);
    action!("undo", commands::undo);
    action!("redo", commands::redo);
    action!("open-terminal", commands::open_terminal);
    action!("extract", commands::extract_selected);
    action!("compress", commands::compress_selected);
    action!("bookmark", places::bookmark_current);
    action!("zoom-in", |s: &Rc<App>| zoom(s, ZOOM_STEP));
    action!("zoom-out", |s: &Rc<App>| zoom(s, -ZOOM_STEP));
    action!("about", about);

    let default_fm = gio::SimpleAction::new_stateful(
        "default-file-manager",
        None,
        &crate::file_manager::is_enabled().to_variant(),
    );
    {
        let state = state.clone();
        let app = app.clone();
        default_fm.connect_change_state(move |act, value| {
            let Some(on) = value.and_then(|v| v.get::<bool>()) else { return };
            match crate::file_manager::set_enabled(on) {
                Ok(note) => {
                    act.set_state(&on.to_variant());
                    if on {
                        crate::file_manager::own(&app);
                    } else {
                        crate::file_manager::release(&app);
                    }
                    toast(&state, &note);
                }
                Err(err) => toast(&state, &err),
            }
        });
    }
    state.window.add_action(&default_fm);

    let toggle = gio::SimpleAction::new_stateful("toggle-hidden", None, &false.to_variant());
    {
        let s = state.clone();
        toggle.connect_activate(move |act, _| {
            let now = !act.state().and_then(|v| v.get::<bool>()).unwrap_or(false);
            act.set_state(&now.to_variant());
            s.show_hidden.set(now);
            for t in s.tabs.borrow().iter() {
                t.filter.changed(gtk::FilterChange::Different);
            }
            update_chrome(&s);
        });
    }
    state.window.add_action(&toggle);

    // "Sort by" — a radio over the three columns plus a descending toggle. Both
    // drive the active tab's shared sort model, so grid and list reorder together.
    let sort = gio::SimpleAction::new_stateful("sort", Some(glib::VariantTy::STRING), &"name".to_variant());
    {
        let s = state.clone();
        sort.connect_activate(move |act, param| {
            let Some(key) = param.and_then(|p| p.get::<String>()) else { return };
            act.set_state(&key.to_variant());
            s.sort_key.set(match key.as_str() {
                "size" => 1,
                "modified" => 2,
                _ => 0,
            });
            apply_sort(&s);
        });
    }
    state.window.add_action(&sort);

    let sort_desc = gio::SimpleAction::new_stateful("sort-descending", None, &false.to_variant());
    {
        let s = state.clone();
        sort_desc.connect_activate(move |act, _| {
            let now = !act.state().and_then(|v| v.get::<bool>()).unwrap_or(false);
            act.set_state(&now.to_variant());
            s.sort_desc.set(now);
            apply_sort(&s);
        });
    }
    state.window.add_action(&sort_desc);

    for (name, accels) in ACCELS {
        app.set_accels_for_action(&format!("win.{name}"), accels);
    }
}



fn about(app: &Rc<App>) {
    let dialog = adw::AboutDialog::builder()
        .application_name("filescope")
        .application_icon("system-file-manager")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("filescope")
        .comments("A fast, nice-looking file manager for Linux.")
        .build();
    dialog.present(Some(&app.window));
}

// --- Chrome -----------------------------------------------------------------

fn update_chrome(app: &Rc<App>) {
    let tab = navigation::active_tab(app);
    app.back_btn.set_sensitive(!tab.back.borrow().is_empty());
    app.fwd_btn.set_sensitive(!tab.fwd.borrow().is_empty());
    app.up_btn.set_sensitive(tab.dir_list.file().and_then(|f| f.parent()).is_some());

    // The window (and, via it, the header) title follows the active tab's
    // location, so it always names the folder you're in — or the special view.
    let showing = tab.view_stack.visible_child_name();
    let special = match showing.as_deref() {
        Some("computer") => Some("This PC"),
        Some("trash") => Some("Trash"),
        _ => None,
    };
    let win_title = match special {
        Some(name) => name.to_string(),
        None => tab
            .dir_list
            .file()
            .map(|f| f.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into()))
            .unwrap_or_else(|| "filescope".into()),
    };
    app.window.set_title(Some(&win_title));

    let total = tab.selection.n_items();
    let sel = commands::selected(&tab);
    let text = if let Some(name) = special {
        // The special views have their own contents; the tab's listing isn't it.
        match name {
            "Trash" => {
                let n = trash::entries_in(&trash::home_trash()).len();
                format!("Trash — {n} item{}", plural(n))
            }
            other => other.to_string(),
        }
    } else if sel.is_empty() {
        format!("{total} item{}", plural(total as usize))
    } else if let [(_, info)] = sel.as_slice() {
        // A single selection: show its full name (folders) or name and size.
        let name = info.display_name();
        if info.file_type() == gio::FileType::Directory {
            name.to_string()
        } else {
            format!("{name} — {}", format::human_size(info.size().max(0) as u64))
        }
    } else {
        let bytes: u64 = sel
            .iter()
            .filter(|(_, i)| i.file_type() != gio::FileType::Directory)
            .map(|(_, i)| i.size().max(0) as u64)
            .sum();
        format!("{} of {total} selected — {}", sel.len(), format::human_size(bytes))
    };
    app.status.set_label(&text);
    update_actions(app, &tab);
}

fn update_actions(app: &Rc<App>, tab: &Rc<Tab>) {
    let count = tab.selection.selection().size();
    let has_sel = count > 0;
    let one = count == 1;
    // Paste is available whenever the system clipboard holds file URIs.
    let has_clip = {
        let formats = app.window.clipboard().formats();
        formats.contain_mime_type(GNOME_COPIED) || formats.contain_mime_type(URI_LIST)
    };
    // "Extract Here" only makes sense when everything selected is an archive.
    let sel = commands::selected(tab);
    let all_archives = has_sel
        && sel.iter().all(|(_, i)| archive::Format::from_name(&i.display_name()).is_some());

    for (name, enabled) in [
        ("open", has_sel), ("open-with", one), ("preview", one), ("cut", has_sel),
        ("copy", has_sel), ("paste", has_clip), ("rename", one), ("trash", has_sel),
        ("delete", has_sel), ("properties", one),
        ("extract", all_archives), ("compress", has_sel),
        ("undo", !app.undo_stack.borrow().is_empty()),
        ("redo", !app.redo_stack.borrow().is_empty()),
    ] {
        if let Some(act) = app.window.lookup_action(name) {
            act.downcast::<gio::SimpleAction>().unwrap().set_enabled(enabled);
        }
    }
}


// --- Context menu -----------------------------------------------------------

fn context_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let a = gio::Menu::new();
    a.append(Some("Open"), Some("win.open"));
    a.append(Some("Open With…"), Some("win.open-with"));
    a.append(Some("Preview (Space)"), Some("win.preview"));
    menu.append_section(None, &a);
    let b = gio::Menu::new();
    b.append(Some("Cut"), Some("win.cut"));
    b.append(Some("Copy"), Some("win.copy"));
    b.append(Some("Paste"), Some("win.paste"));
    menu.append_section(None, &b);
    let undo_section = gio::Menu::new();
    undo_section.append(Some("Undo"), Some("win.undo"));
    undo_section.append(Some("Redo"), Some("win.redo"));
    menu.append_section(None, &undo_section);
    let archive_section = gio::Menu::new();
    archive_section.append(Some("Extract Here"), Some("win.extract"));
    archive_section.append(Some("Compress…"), Some("win.compress"));
    menu.append_section(None, &archive_section);
    let c = gio::Menu::new();
    c.append(Some("Rename…"), Some("win.rename"));
    c.append(Some("Move to Trash"), Some("win.trash"));
    c.append(Some("Delete Permanently…"), Some("win.delete"));
    menu.append_section(None, &c);
    let d = gio::Menu::new();
    d.append(Some("New Folder…"), Some("win.new-folder"));
    d.append(Some("Open Terminal Here (Ctrl+.)"), Some("win.open-terminal"));
    d.append(Some("Bookmark This Folder"), Some("win.bookmark"));
    d.append(Some("Select All"), Some("win.select-all"));
    d.append(Some("Properties"), Some("win.properties"));
    menu.append_section(None, &d);
    menu
}

fn sort_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let by = gio::Menu::new();
    by.append(Some("Name"), Some("win.sort::name"));
    by.append(Some("Size"), Some("win.sort::size"));
    by.append(Some("Modified"), Some("win.sort::modified"));
    menu.append_section(Some("Sort by"), &by);
    let dir = gio::Menu::new();
    dir.append(Some("Descending"), Some("win.sort-descending"));
    menu.append_section(None, &dir);
    menu
}

/// Apply the shared sort selection to the active tab.
fn apply_sort(app: &Rc<App>) {
    let tab = navigation::active_tab(app);
    apply_sort_to(app, &tab);
}

/// Point `tab`'s column view at the currently selected sort column and direction.
/// Because the column view's sorter backs the tab's sort model, this reorders
/// both the grid and the list at once.
fn apply_sort_to(app: &Rc<App>, tab: &Rc<Tab>) {
    let idx = app.sort_key.get() as u32;
    let Some(col) = tab.column_view.columns().item(idx).and_downcast::<gtk::ColumnViewColumn>()
    else {
        return;
    };
    let dir = if app.sort_desc.get() { gtk::SortType::Descending } else { gtk::SortType::Ascending };
    tab.column_view.sort_by_column(Some(&col), dir);
}

fn primary_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let a = gio::Menu::new();
    a.append(Some("New Tab"), Some("win.new-tab"));
    a.append(Some("New Folder…"), Some("win.new-folder"));
    a.append(Some("Open Terminal Here"), Some("win.open-terminal"));
    a.append(Some("Bookmark This Folder"), Some("win.bookmark"));
    menu.append_section(None, &a);
    let b = gio::Menu::new();
    b.append(Some("Search…"), Some("win.find"));
    b.append(Some("Show Hidden Files"), Some("win.toggle-hidden"));
    b.append(Some("Zoom In"), Some("win.zoom-in"));
    b.append(Some("Zoom Out"), Some("win.zoom-out"));
    b.append(Some("Refresh"), Some("win.refresh"));
    menu.append_section(None, &b);
    let defaults = gio::Menu::new();
    defaults.append(Some("Default File Manager"), Some("win.default-file-manager"));
    menu.append_section(None, &defaults);
    let c = gio::Menu::new();
    c.append(Some("About filescope"), Some("win.about"));
    menu.append_section(None, &c);
    menu
}


fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

fn flat_icon(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).build();
    b.add_css_class("flat");
    b
}

fn toast(app: &Rc<App>, message: &str) {
    app.toasts.add_toast(adw::Toast::new(message));
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn permission_string(mode: u32) -> String {
    let bit = |shift: u32, ch: char| if mode & (1 << shift) != 0 { ch } else { '-' };
    [
        bit(8, 'r'), bit(7, 'w'), bit(6, 'x'),
        bit(5, 'r'), bit(4, 'w'), bit(3, 'x'),
        bit(2, 'r'), bit(1, 'w'), bit(0, 'x'),
    ]
    .iter()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::commands::parse_clipboard;
    use super::{ACCELS, GNOME_COPIED, URI_LIST};


    #[test]
    fn no_accelerator_steals_a_typing_key() {
        // Window accelerators are dispatched before the focused widget, so a bare
        // key bound here becomes untypable in the rename/search/path entries.
        // (This caught Space and Backspace being stolen while renaming.)
        for (action, accels) in ACCELS {
            for accel in *accels {
                let has_modifier = accel.contains('<');
                let function_key = accel.len() >= 2
                    && accel.starts_with('F')
                    && accel[1..].chars().all(|c| c.is_ascii_digit());
                assert!(
                    has_modifier || function_key,
                    "win.{action} binds bare `{accel}`: it would be stolen from text entries"
                );
            }
        }
    }


    #[test]
    fn parses_gnome_copy_and_cut() {
        // GNOME format: first line is the action, then one URI per line.
        assert_eq!(
            parse_clipboard(GNOME_COPIED, "copy\nfile:///a\nfile:///b"),
            (false, vec!["file:///a".to_string(), "file:///b".to_string()])
        );
        assert_eq!(
            parse_clipboard(GNOME_COPIED, "cut\nfile:///x"),
            (true, vec!["file:///x".to_string()])
        );
    }

    #[test]
    fn parses_uri_list_skipping_comments() {
        // A plain URI list is always a copy; comment and blank lines are ignored.
        assert_eq!(
            parse_clipboard(URI_LIST, "# comment\r\nfile:///a\r\n\r\nfile:///b\r\n"),
            (false, vec!["file:///a".to_string(), "file:///b".to_string()])
        );
    }
}
