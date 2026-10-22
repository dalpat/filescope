// The main window shell: header, tabs, and startup.
// navigation.rs moves between folders. commands.rs acts on the selection.
// places.rs is the sidebar. view.rs is the grid and list.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::{archive, bookmarks, format, preview, settings, undo};

mod commands;
mod navigation;
mod places;
mod search;
mod terminal;
mod view;


const CSS: &str = "
.fs-grid { padding: 8px; }
.fs-grid > child { border-radius: 12px; padding: 8px; margin: 2px; transition: background 120ms; }
.fs-grid > child:hover { background: alpha(@window_fg_color, 0.06); }
.fs-grid > child:selected { background: alpha(@accent_bg_color, 0.22); }
.fs-drop { background: alpha(@accent_bg_color, 0.35); border-radius: 8px; outline: 2px solid @accent_color; outline-offset: -2px; }
.fs-name { margin-top: 4px; }
.fs-preview-pane { background: @window_bg_color; border-radius: 0; box-shadow: none; border-left: 1px solid alpha(@window_fg_color, 0.12); }
.fs-hidden { opacity: 0.45; }
columnview > header > button { padding: 2px 12px 8px; min-height: 0; }
columnview > listview > row { border-radius: 8px; margin: 1px 8px; }
columnview > listview > row:hover { background: alpha(@window_fg_color, 0.06); }
columnview > listview > row:selected,
columnview > listview > row:selected:hover { background: alpha(@accent_bg_color, 0.22); }
columnview > listview > row > cell { padding: 3px 10px; }
.crumbs button { padding: 3px 8px; min-height: 0; }
.crumbs button.current { font-weight: bold; }
button.place-card, button.drive-card {
  background: @card_bg_color;
  box-shadow: none;
  border: 1px solid alpha(@window_fg_color, 0.08);
  border-radius: 12px;
  font-weight: normal;
}
button.place-card { padding: 16px 10px 12px; }
button.drive-card { padding: 14px 16px; }
button.place-card:hover, button.drive-card:hover {
  background: alpha(@window_fg_color, 0.04);
  box-shadow: none;
}
flowbox.pc-tiles > flowboxchild,
flowbox.pc-tiles > flowboxchild:hover {
  background: none;
  box-shadow: none;
  padding: 0;
}
flowbox.trash-items > flowboxchild:selected > button {
  background: alpha(@accent_bg_color, 0.22);
  border-color: @accent_bg_color;
}
.drive-bar { min-height: 8px; }
.drive-bar > trough, .drive-bar > trough > progress { min-height: 8px; border-radius: 8px; }
expander.pc-section > title { font-size: 13px; font-weight: normal; color: alpha(@window_fg_color, 0.55); padding: 2px 2px 12px; }
.bookmark-remove { min-height: 0; min-width: 0; padding: 2px; }
.bookmark-x { opacity: 0; transition: opacity 120ms; }
row:hover .bookmark-x,
.bookmark-x:focus-visible { opacity: 1; }
.bookmark-drop { background: alpha(@accent_bg_color, 0.22); }
.fs-terminal { background: @window_bg_color; }
.fs-terminal-handle { min-height: 5px; background: alpha(@window_fg_color, 0.10); }
.fs-terminal-handle:hover { background: @accent_bg_color; }
.fs-terminal-bar { padding: 0 8px; min-height: 28px; }
.fs-terminal-bar label { font-size: 12px; color: alpha(@window_fg_color, 0.65); }
.fs-terminal-bar button { min-height: 0; min-width: 0; padding: 4px; }
";

const ATTRS: &str = "standard::*,time::modified,unix::mode,thumbnail::path,thumbnail::is-valid";

/// Zoom bounds for the grid icon size (pixels); list scales at a quarter.
const ZOOM_MIN: i32 = 48;
const ZOOM_MAX: i32 = 160;
const ZOOM_STEP: i32 = 16;

/// Clipboard mime types for file cut/copy: GNOME's copy/cut format (carries the
/// cut-vs-copy intent and interoperates with Nautilus/Files) and the generic
/// URI list (understood by most apps).
const GNOME_COPIED: &str = "x-special/gnome-copied-files";
const URI_LIST: &str = "text/uri-list";

/// A step in a tab's back/forward history. This PC is not a folder.
pub(super) enum Place {
    Folder(gio::File),
    Search { root: gio::File, query: String },
    Computer,
    Trash,
}

/// Where an in-window shell can start. This PC and Trash are not directories.
pub(super) fn terminal_cwd(place: &Place) -> Option<PathBuf> {
    match place {
        Place::Folder(file) | Place::Search { root: file, .. } => file.path(),
        Place::Computer | Place::Trash => None,
    }
}

/// One folder tab: its own listing, views, history, and breadcrumb.
struct Tab {
    page: RefCell<Option<adw::TabPage>>,
    dir_list: gtk::DirectoryList,
    filter: gtk::CustomFilter,
    selection: gtk::MultiSelection,
    folder_model: gtk::SortListModel,
    search: search::Pane,
    view_stack: gtk::Stack,
    grid_view: gtk::GridView,
    column_view: gtk::ColumnView,
    breadcrumb: gtk::Box,
    computer_box: gtk::Box,
    trash_box: gtk::Box,
    /// Count from the rendered Trash listing, independent of the folder model.
    trash_count: Cell<u32>,
    preview_pane: Rc<preview::pane::Pane>,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    back: RefCell<Vec<Place>>,
    fwd: RefCell<Vec<Place>>,
    /// Zoom last applied to each folder view. The hidden one stays stale until shown.
    grid_zoom: Cell<i32>,
    list_zoom: Cell<i32>,
    /// Bottom shell for this tab. Hidden until the user opens it.
    terminal: terminal::Pane,
}

impl Tab {
    fn shutdown(&self) {
        self.terminal.shutdown();
        self.search.cancel();
        self.preview_pane.set_visible(false);
        // Special-view cards hold navigation callbacks back to this tab.
        for container in [&self.computer_box, &self.trash_box] {
            while let Some(child) = container.first_child() {
                container.remove(&child);
            }
        }
    }
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
    /// A zoom is already waiting for the next frame, so more wheel ticks only
    /// update `zoom` and do not rebuild again.
    zoom_queued: Cell<bool>,
    is_list: Cell<bool>,
    show_hidden: Rc<Cell<bool>>,
    bookmarks: RefCell<Vec<PathBuf>>,
    preview: RefCell<Option<adw::Window>>,
    preview_pane_default: Cell<bool>,
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

/// Continue an Overview query in a tab with the same indexed result ordering.
pub(crate) fn open_search(app: &adw::Application, root: &gio::File, query: &str) {
    if app_state(app).is_none() { build(app); }
    let Some(state) = app_state(app) else { return; };
    navigation::new_tab(&state, root.clone(), true);
    let tab = navigation::active_tab(&state);
    tab.search_bar.set_search_mode(true);
    tab.search_entry.set_text(query);
    search::changed(&state, &tab, true);
    state.window.present();
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

    let back_btn = crate::icon::flat_button("go-previous-symbolic", "Back (Alt+Left)");
    let fwd_btn = crate::icon::flat_button("go-next-symbolic", "Forward (Alt+Right)");
    let up_btn = crate::icon::flat_button("go-up-symbolic", "Up (Alt+Up)");
    let sidebar_toggle = gtk::ToggleButton::builder()
        .child(&crate::icon::image_named("sidebar-show-symbolic", 16))
        .active(true)
        .tooltip_text("Toggle sidebar")
        .build();
    sidebar_toggle.add_css_class("flat");
    let new_tab_btn = crate::icon::flat_button("tab-new-symbolic", "New tab (Ctrl+T)");
    let view_toggle = gtk::ToggleButton::builder()
        .child(&crate::icon::image_named("view-list-symbolic", 16))
        .tooltip_text("List view")
        .build();
    view_toggle.add_css_class("flat");
    let sort_btn = gtk::MenuButton::builder()
        .child(&crate::icon::image_named("view-sort-ascending-symbolic", 16))
        .tooltip_text("Sort by")
        .build();
    sort_btn.add_css_class("flat");
    sort_btn.set_menu_model(Some(&sort_menu()));
    let zoom_out = crate::icon::flat_button("zoom-out-symbolic", "Zoom out (Ctrl+-)");
    let zoom_in = crate::icon::flat_button("zoom-in-symbolic", "Zoom in (Ctrl++)");
    let menu_btn = gtk::MenuButton::builder().child(&crate::icon::image_named("open-menu-symbolic", 16)).build();
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
        zoom_queued: Cell::new(false),
        is_list: Cell::new(cfg.is_list),
        show_hidden: Rc::new(Cell::new(cfg.show_hidden)),
        bookmarks: RefCell::new(bookmarks::load()),
        preview: RefCell::new(None),
        preview_pane_default: Cell::new(cfg.preview_pane),
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
        let application = app.downgrade();
        window.connect_close_request(move |window| {
            preview::close_sushi(window);
            for tab in state.tabs.borrow().iter() {
                tab.shutdown();
            }
            save_settings(&state);
            // The search service can outlive this window. Reopening must build
            // a new window instead of presenting the destroyed one.
            if let Some(application) = application.upgrade()
                && app_state(&application).as_ref().is_some_and(|current| Rc::ptr_eq(current, &state))
            {
                unsafe { application.steal_data::<Rc<App>>(STATE_KEY) };
            }
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
                for tab in state.tabs.borrow().iter().filter(|t| t.page.borrow().as_ref() == Some(page)) {
                    tab.shutdown();
                }
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
            if let Some(image) = b.child().and_downcast::<gtk::Image>() {
                crate::icon::set_named(
                    &image,
                    if list { "view-grid-symbolic" } else { "view-list-symbolic" },
                    16,
                );
            }
            b.set_tooltip_text(Some(if list { "Grid view" } else { "List view" }));
            for t in s.tabs.borrow().iter() {
                match t.view_stack.visible_child_name().as_deref() {
                    Some("computer") => places::show_computer(&s, t),
                    Some("trash") => places::show_trash(&s, t),
                    _ => navigation::refresh_view_mode(&s, t, false),
                }
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
        // Explorer zooms with Ctrl+wheel. Plain scrolling still moves the page.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let s = state.clone();
        scroll.connect_scroll(move |controller, _, dy| {
            if !controller.current_event_state().contains(gtk::gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            if dy < 0.0 {
                zoom(&s, ZOOM_STEP);
            } else if dy > 0.0 {
                zoom(&s, -ZOOM_STEP);
            }
            glib::Propagation::Stop
        });
        state.window.add_controller(scroll);
    }
    {
        let s = state.clone();
        sidebar_list.connect_row_activated(move |_, row| {
            let t = navigation::active_tab(&s);
            if unsafe { row.data::<bool>("computer") }.is_some() {
                places::show_computer(&s, &t);
            } else if unsafe { row.data::<bool>("trash") }.is_some() {
                navigation::go_trash(&s, &t);
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
    // A wheel flick is many events. Apply the latest size once, on the next frame.
    if app.zoom_queued.replace(true) {
        return;
    }
    let app = app.clone();
    glib::idle_add_local_once(move || {
        app.zoom_queued.set(false);
        let tabs: Vec<_> = app.tabs.borrow().iter().cloned().collect();
        for tab in tabs {
            apply_visible_zoom(&app, &tab);
        }
    });
}

/// Resize only the view the tab is actually showing.
fn apply_visible_zoom(app: &Rc<App>, tab: &Rc<Tab>) {
    let zoom = app.zoom.get();
    let showing = tab.view_stack.visible_child_name();
    let stamped = match showing.as_deref() {
        Some("list") => tab.list_zoom.get(),
        _ => tab.grid_zoom.get(),
    };
    match view::zoom_work(showing.as_deref(), stamped, zoom) {
        view::ZoomWork::None => {}
        view::ZoomWork::Tiles => match showing.as_deref() {
            Some("computer") => places::rescale_tiles(app, &tab.computer_box),
            _ => places::rescale_tiles(app, &tab.trash_box),
        },
        view::ZoomWork::Resize(icon) => {
            // Stamp first. A later bind reads this size. An unstamped view
            // would keep asking for the old one.
            if showing.as_deref() == Some("list") {
                tab.list_zoom.set(zoom);
                view::resize_names(tab.column_view.upcast_ref(), icon);
            } else {
                tab.grid_zoom.set(zoom);
                view::resize_names(tab.grid_view.upcast_ref(), icon);
            }
        }
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
    ("preview-pane", &["<ctrl><shift>p"]),
    ("toggle-hidden", &["<ctrl>h"]),
    ("bookmark", &["<ctrl>d"]),
    ("undo", &["<ctrl>z"]),
    ("redo", &["<ctrl><shift>z", "<ctrl>y"]),
    ("open-terminal", &["<ctrl>period"]),
    ("terminal-pane", &["<ctrl>grave"]),
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
    action!("open-containing-folder", commands::open_containing_folder);
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
    action!("select-all", |s: &Rc<App>| {
        let tab = navigation::active_tab(s);
        if tab.view_stack.visible_child_name().as_deref() == Some("trash") {
            if let Some(flow) = places::trash_flow(&tab) { flow.select_all(); }
        } else {
            tab.selection.select_all();
        }
    });
    action!("refresh", |s: &Rc<App>| {
        let tab = navigation::active_tab(s);
        if tab.view_stack.visible_child_name().as_deref() == Some("trash") {
            places::show_trash(s, &tab);
        } else {
            navigation::refresh(&tab);
        }
    });
    action!("find", |s: &Rc<App>| {
        let t = navigation::active_tab(s);
        if matches!(t.view_stack.visible_child_name().as_deref(), Some("computer" | "trash")) {
            navigation::navigate(s, &t, gio::File::for_path(glib::home_dir()));
        }
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
    action!("terminal-pane", toggle_terminal_pane);
    action!("extract", commands::extract_selected);
    action!("compress", commands::compress_selected);
    action!("bookmark", places::bookmark_current);
    action!("zoom-in", |s: &Rc<App>| zoom(s, ZOOM_STEP));
    action!("zoom-out", |s: &Rc<App>| zoom(s, -ZOOM_STEP));
    action!("about", about);

    let pane = gio::SimpleAction::new_stateful("preview-pane", None, &false.to_variant());
    {
        let state = state.clone();
        pane.connect_activate(move |action, _| {
            let tab = navigation::active_tab(&state);
            let visible = !tab.preview_pane.widget.is_visible();
            tab.preview_pane.set_visible(visible);
            update_preview(&tab);
            action.set_state(&visible.to_variant());
            state.preview_pane_default.set(visible);
            save_settings(&state);
        });
    }
    state.window.add_action(&pane);

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

fn status_line(view: &str, total: u32, selected: u64, single: Option<&gio::FileInfo>) -> String {
    match (view, selected, single) {
        ("computer", _, _) => String::new(),
        ("trash", _, _) | (_, 0, _) => format!("{total} item{}", plural(total as usize)),
        (_, 1, Some(info)) => {
            let name = info.display_name();
            if info.file_type() == gio::FileType::Directory {
                name.to_string()
            } else {
                format!("{name} — {}", format::human_size(info.size().max(0) as u64))
            }
        }
        _ => format!("{selected} selected"),
    }
}

fn update_chrome(app: &Rc<App>) {
    let tab = navigation::active_tab(app);
    commands::set_action_state(app, "preview-pane", &tab.preview_pane.widget.is_visible().to_variant());
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
    let win_title = if tab.search.active.get() { format!("Search: {}", tab.search.query.borrow()) } else { match special {
        Some(name) => name.to_string(),
        None => tab
            .dir_list
            .file()
            .map(|f| f.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into()))
            .unwrap_or_else(|| "filescope".into()),
    }};
    app.window.set_title(Some(&win_title));

    let total = if showing.as_deref() == Some("trash") {
        tab.trash_count.get()
    } else {
        tab.selection.n_items()
    };
    let selection = tab.selection.selection();
    let selected = selection.size();
    let single = if special.is_none() && selected == 1 {
        tab.selection.item(selection.minimum()).and_downcast::<gio::FileInfo>()
    } else {
        None
    };
    app.status.set_label(&status_line(showing.as_deref().unwrap_or("grid"), total, selected, single.as_ref()));
    update_actions(app, &tab);
}

fn update_actions(app: &Rc<App>, tab: &Rc<Tab>) {
    let trash = tab.view_stack.visible_child_name().as_deref() == Some("trash");
    let count = tab.selection.selection().size();
    let has_sel = count > 0 && !trash;
    let one = count == 1 && !trash;
    let has_clip = clipboard_has_files(app);
    let folder = !tab.search.active.get() && !trash;
    // "Extract Here" only makes sense when everything selected is an archive.
    // Open With and Preview only apply to a single file, not a folder.
    let sel = commands::selected(tab);
    let single_file = matches!(
        sel.as_slice(),
        [(_, info)] if info.file_type() != gio::FileType::Directory
    );
    let all_archives = has_sel
        && sel.iter().all(|(_, i)| archive::Format::from_name(&i.display_name()).is_some());

    for (name, enabled) in [
        ("open", has_sel), ("open-with", single_file), ("preview", single_file), ("cut", has_sel),
        ("copy", has_sel), ("paste", has_clip && folder), ("rename", one), ("trash", has_sel),
        ("delete", has_sel), ("properties", one),
        ("extract", all_archives && folder), ("compress", has_sel && folder),
        ("new-folder", folder), ("open-containing-folder", one && !folder),
        ("sort", folder), ("sort-descending", folder),
        ("undo", !app.undo_stack.borrow().is_empty()),
        ("redo", !app.redo_stack.borrow().is_empty()),
    ] {
        if let Some(act) = app.window.lookup_action(name) {
            act.downcast::<gio::SimpleAction>().unwrap().set_enabled(enabled);
        }
    }
}


// --- Context menu -----------------------------------------------------------

fn clipboard_has_files(app: &App) -> bool {
    let formats = app.window.clipboard().formats();
    formats.contain_mime_type(GNOME_COPIED) || formats.contain_mime_type(URI_LIST)
}

/// Right-click on a file or folder: only actions for that selection.
/// Right-click on empty space: only actions for the folder you are in.
fn context_menu_for(app: &Rc<App>, on_item: bool) -> gio::Menu {
    let tab = navigation::active_tab(app);
    let sel = commands::selected(&tab);
    let one = sel.len() == 1;
    let single_file = matches!(
        sel.as_slice(),
        [(_, info)] if info.file_type() != gio::FileType::Directory
    );
    let all_archives = !sel.is_empty()
        && sel.iter().all(|(_, i)| archive::Format::from_name(&i.display_name()).is_some());
    let has_clip = clipboard_has_files(app);

    let menu = gio::Menu::new();
    if on_item && !sel.is_empty() {
        let primary = gio::Menu::new();
        primary.append(Some("Open"), Some("win.open"));
        if tab.search.active.get() && one {
            primary.append(Some("Open Containing Folder"), Some("win.open-containing-folder"));
        }
        if single_file {
            primary.append(Some("Open With…"), Some("win.open-with"));
            // Space previews from the file view. It is not a window accelerator,
            // so the menu has to say so itself.
            append_shown_accel(&primary, "Preview", "win.preview", "space");
        }
        primary.append(Some("Cut"), Some("win.cut"));
        primary.append(Some("Copy"), Some("win.copy"));
        if has_clip {
            primary.append(Some("Paste"), Some("win.paste"));
        }
        menu.append_section(None, &primary);

        let edit = gio::Menu::new();
        if one {
            edit.append(Some("Rename…"), Some("win.rename"));
        }
        if all_archives {
            edit.append(Some("Extract Here"), Some("win.extract"));
        }
        edit.append(Some("Compress…"), Some("win.compress"));
        append_shown_accel(&edit, "Move to Trash", "win.trash", "Delete");
        append_shown_accel(&edit, "Delete Permanently…", "win.delete", "<shift>Delete");
        if one {
            edit.append(Some("Properties"), Some("win.properties"));
        }
        menu.append_section(None, &edit);
    } else {
        let folder = gio::Menu::new();
        folder.append(Some("New Folder"), Some("win.new-folder"));
        if has_clip {
            folder.append(Some("Paste"), Some("win.paste"));
        }
        folder.append(Some("Select All"), Some("win.select-all"));
        menu.append_section(None, &folder);

        let place = gio::Menu::new();
        place.append(Some("Open Terminal Here"), Some("win.open-terminal"));
        place.append(Some("Bookmark This Folder"), Some("win.bookmark"));
        menu.append_section(None, &place);

        let history = gio::Menu::new();
        if !app.undo_stack.borrow().is_empty() {
            history.append(Some("Undo"), Some("win.undo"));
        }
        if !app.redo_stack.borrow().is_empty() {
            history.append(Some("Redo"), Some("win.redo"));
        }
        if history.n_items() > 0 {
            menu.append_section(None, &history);
        }
    }
    menu
}

/// Show a shortcut on a menu row without registering a window accelerator.
/// Delete and Space are handled by the file view so they are not stolen from text fields.
fn append_shown_accel(menu: &gio::Menu, label: &str, action: &str, accel: &str) {
    let item = gio::MenuItem::new(Some(label), Some(action));
    item.set_attribute_value("accel", Some(&accel.to_variant()));
    menu.append_item(&item);
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

fn toggle_terminal_pane(app: &Rc<App>) {
    let tab = navigation::active_tab(app);
    let cwd = navigation::here(&tab).as_ref().and_then(terminal_cwd);
    tab.terminal.toggle(cwd.as_deref());
}

fn save_settings(app: &App) {
    settings::save(&settings::Settings {
        is_list: app.is_list.get(),
        preview_pane: app.preview_pane_default.get(),
        zoom: app.zoom.get(),
        sort_key: app.sort_key.get(),
        sort_desc: app.sort_desc.get(),
        show_hidden: app.show_hidden.get(),
        width: app.window.default_width(),
        height: app.window.default_height(),
        maximized: app.window.is_maximized(),
    });
}

fn update_preview(tab: &Tab) {
    let selection = tab.selection.selection();
    let target = if selection.size() == 1
        && !matches!(tab.view_stack.visible_child_name().as_deref(), Some("computer" | "trash"))
    {
        tab.dir_list.file().and_then(|dir| {
            let info = tab.selection.item(selection.minimum()).and_downcast::<gio::FileInfo>()?;
            let directory = if tab.search.active.get() { None } else { Some(&dir) };
            Some((commands::file_from_info(&info, directory)?, info))
        })
    } else {
        None
    };
    tab.preview_pane.set_target(target);
}

fn primary_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let a = gio::Menu::new();
    a.append(Some("New Tab"), Some("win.new-tab"));
    a.append(Some("New Folder"), Some("win.new-folder"));
    a.append(Some("Open Terminal Here"), Some("win.open-terminal"));
    a.append(Some("Terminal Pane"), Some("win.terminal-pane"));
    a.append(Some("Bookmark This Folder"), Some("win.bookmark"));
    menu.append_section(None, &a);
    let b = gio::Menu::new();
    b.append(Some("Search…"), Some("win.find"));
    b.append(Some("Show Hidden Files"), Some("win.toggle-hidden"));
    b.append(Some("Preview Pane"), Some("win.preview-pane"));
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

fn toast(app: &Rc<App>, message: &str) {
    app.toasts.add_toast(adw::Toast::new(message));
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

pub(crate) fn permission_string(mode: u32) -> String {
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
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn closed_tabs_release_folder_and_special_views() {
        use super::*;
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        gtk::Settings::default().unwrap().set_gtk_enable_animations(false);
        let application = adw::Application::builder()
            .application_id("dev.filescope.TabCleanupTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let context = glib::MainContext::default();
        context.block_on(async {
            open_files(&application, &[gio::File::for_path(std::env::temp_dir())]);
            let app = app_state(&application).unwrap();
            for view in ["grid", "computer", "trash"] {
                navigation::new_tab(&app, gio::File::for_path(std::env::temp_dir()), true);
                let tab = navigation::active_tab(&app);
                match view {
                    "computer" => places::show_computer(&app, &tab),
                    "trash" => places::show_trash(&app, &tab),
                    _ => {},
                }
                glib::timeout_future(std::time::Duration::from_millis(200)).await;
                let weak = Rc::downgrade(&tab);
                let page = tab.page.borrow().clone().unwrap();
                drop(tab);
                app.tab_view.close_page(&page);
                drop(page);
                glib::timeout_future(std::time::Duration::from_millis(600)).await;
                assert!(weak.upgrade().is_none(), "closed {view} tab is retained");
                assert_eq!(app.tabs.borrow().len(), 1);
            }
            app.window.close();
        });
    }

    fn status_item(name: &str, kind: gtk::gio::FileType, size: i64) -> gtk::gio::FileInfo {
        let info = gtk::gio::FileInfo::new();
        info.set_display_name(name);
        info.set_file_type(kind);
        info.set_size(size);
        info
    }

    #[test]
    fn status_line_counts_large_selections_without_item_metadata() {
        assert_eq!(super::status_line("grid", 10_000, 10_000, None), "10000 selected");
        assert_eq!(super::status_line("list", 10_000, 10_000, None), "10000 selected");
    }

    #[test]
    fn status_line_for_a_folder() {
        use gtk::gio::FileType;
        for view in ["grid", "list", "empty"] {
            assert_eq!(super::status_line(view, 12, 0, None), "12 items");
            let file = status_item("notes.txt", FileType::Regular, 1000);
            let folder = status_item("Photos", FileType::Directory, 4096);
            assert_eq!(super::status_line(view, 12, 1, Some(&file)), "notes.txt — 1.0 kB");
            assert_eq!(super::status_line(view, 12, 1, Some(&folder)), "Photos");
            assert_eq!(super::status_line(view, 12, 3, None), "3 selected");
        }
    }

    #[test]
    fn status_line_for_this_pc_is_empty() {
        let file = status_item("notes.txt", gtk::gio::FileType::Regular, 1000);
        assert_eq!(super::status_line("computer", 12, 0, None), "");
        assert_eq!(super::status_line("computer", 12, 1, Some(&file)), "");
        assert_eq!(super::status_line("computer", 12, 10_000, None), "");
    }

    #[test]
    fn status_line_for_trash_uses_the_given_count() {
        let file = status_item("old-folder-selection.txt", gtk::gio::FileType::Regular, 1000);
        assert_eq!(super::status_line("trash", 4, 0, None), "4 items");
        assert_eq!(super::status_line("trash", 4, 1, Some(&file)), "4 items");
        assert_eq!(super::status_line("trash", 4, 10_000, None), "4 items");
        for (count, expected) in [(0, "0 items"), (1, "1 item"), (2, "2 items")] {
            assert_eq!(super::status_line("trash", count, 0, None), expected);
        }
    }

    #[test]
    fn status_line_pluralizes_item_count() {
        for (count, expected) in [(0, "0 items"), (1, "1 item"), (2, "2 items")] {
            assert_eq!(super::status_line("grid", count, 0, None), expected);
        }
    }

    #[test]
    fn status_line_clamps_unknown_file_size() {
        let file = status_item("notes.txt", gtk::gio::FileType::Regular, -1);
        assert_eq!(super::status_line("grid", 1, 1, Some(&file)), "notes.txt — 0 B");
    }


    #[test]
    fn terminal_cwd_is_the_folder_and_absent_on_special_views() {
        let folder = super::Place::Folder(gtk::gio::File::for_path("/tmp"));
        assert_eq!(super::terminal_cwd(&folder).as_deref(), Some(std::path::Path::new("/tmp")));
        assert!(super::terminal_cwd(&super::Place::Computer).is_none());
        assert!(super::terminal_cwd(&super::Place::Trash).is_none());
    }

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
