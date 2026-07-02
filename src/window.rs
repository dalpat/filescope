// The main window: a places/bookmarks sidebar beside a tabbed content area.
// Each tab is an independent folder view (grid or list, zoomable) with its own
// history and breadcrumb. A special "This PC" view lists drives with capacity.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{bookmarks, fileops, format, ops, preview};

const CSS: &str = "
.fs-grid { padding: 8px; }
.fs-grid > child { border-radius: 12px; padding: 8px; margin: 2px; transition: background 120ms; }
.fs-grid > child:hover { background: alpha(@window_fg_color, 0.06); }
.fs-grid > child:selected { background: alpha(@accent_bg_color, 0.22); }
.fs-drop { background: alpha(@accent_bg_color, 0.35); border-radius: 8px; outline: 2px solid @accent_color; outline-offset: -2px; }
.fs-name { margin-top: 4px; }
columnview > listview > row { border-radius: 8px; }
.crumbs button { padding: 3px 8px; min-height: 0; }
.crumbs button.current { font-weight: bold; }
.drive-card { padding: 14px; border-radius: 12px; }
.drive-bar { min-height: 8px; }
.drive-bar > trough, .drive-bar > trough > progress { min-height: 8px; border-radius: 8px; }
.bookmark-remove { min-height: 0; min-width: 0; padding: 2px; }
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
}

pub fn build(app: &adw::Application, initial: Option<String>) {
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

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("filescope")
        .default_width(1120)
        .default_height(740)
        .content(&split)
        .build();

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
        zoom: Cell::new(80),
        is_list: Cell::new(false),
        show_hidden: Rc::new(Cell::new(false)),
        bookmarks: RefCell::new(bookmarks::load()),
        preview: RefCell::new(None),
        sort_key: Cell::new(0),
        sort_desc: Cell::new(false),
        progress_box,
        progress_revealer,
    });

    install_actions(app, &state);
    populate_sidebar(&state);

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
        back_btn.connect_clicked(move |_| { let t = active_tab(&s); go_back(&s, &t); });
    }
    {
        let s = state.clone();
        fwd_btn.connect_clicked(move |_| { let t = active_tab(&s); go_forward(&s, &t); });
    }
    {
        let s = state.clone();
        up_btn.connect_clicked(move |_| { let t = active_tab(&s); go_up(&s, &t); });
    }
    {
        let s = state.clone();
        new_tab_btn.connect_clicked(move |_| { new_tab(&s, gio::File::for_path(glib::home_dir()), true); });
    }
    {
        let s = state.clone();
        view_toggle.connect_toggled(move |b| {
            let list = b.is_active();
            s.is_list.set(list);
            b.set_icon_name(if list { "view-grid-symbolic" } else { "view-list-symbolic" });
            b.set_tooltip_text(Some(if list { "Grid view" } else { "List view" }));
            for t in s.tabs.borrow().iter() {
                if t.view_stack.visible_child_name().as_deref() != Some("computer") {
                    t.view_stack.set_visible_child_name(if list { "list" } else { "grid" });
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
        let s = state.clone();
        sidebar_list.connect_row_activated(move |_, row| {
            let t = active_tab(&s);
            if unsafe { row.data::<bool>("computer") }.is_some() {
                show_computer(&s, &t);
            } else if let Some(volume) = unsafe { row.data::<gio::Volume>("volume") } {
                mount_and_open(&s, &t, unsafe { volume.as_ref() });
            } else if let Some(path) = unsafe { row.data::<PathBuf>("path") } {
                navigate(&s, &t, gio::File::for_path(unsafe { path.as_ref() }.clone()));
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
        monitor.connect_mount_added(move |_, _| populate_sidebar(&s));
        let s = state.clone();
        monitor.connect_mount_removed(move |_, _| populate_sidebar(&s));
        let s = state.clone();
        monitor.connect_volume_added(move |_, _| populate_sidebar(&s));
        let s = state.clone();
        monitor.connect_volume_removed(move |_, _| populate_sidebar(&s));
    }

    window.present();

    // Startup: if a folder was passed on the command line, open it; otherwise
    // land on "This PC" (the drives overview) rather than Home. The tab is still
    // seeded at Home so Back/Up have a sensible base to fall to.
    match initial.map(PathBuf::from).filter(|p| p.is_dir()) {
        Some(dir) => new_tab(&state, gio::File::for_path(dir), true),
        None => {
            new_tab(&state, gio::File::for_path(glib::home_dir()), true);
            let tab = active_tab(&state);
            show_computer(&state, &tab);
        }
    }

    maybe_capture(app, &state);
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
        let tab = active_tab(state);
        match view.as_str() {
            "list" => {
                state.is_list.set(true);
                tab.view_stack.set_visible_child_name("list");
            }
            "grid" => {
                state.is_list.set(false);
                tab.view_stack.set_visible_child_name("grid");
            }
            "computer" => show_computer(state, &tab),
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

// --- Tabs -------------------------------------------------------------------

/// Create a tab showing `dir`, optionally selecting it.
fn new_tab(app: &Rc<App>, dir: gio::File, select: bool) {
    let dir_list =
        gtk::DirectoryList::builder().attributes(ATTRS).io_priority(glib::Priority::DEFAULT).build();

    let show_hidden = app.show_hidden.clone();
    // The live search term (lowercased), shared with the filter. Empty ⇒ no
    // search filtering.
    let search_term = Rc::new(RefCell::new(String::new()));
    let filter = {
        let show_hidden = show_hidden.clone();
        let search_term = search_term.clone();
        gtk::CustomFilter::new(move |obj| {
            let info = obj.downcast_ref::<gio::FileInfo>().unwrap();
            // Hidden files, unless the user opted to show them.
            if !show_hidden.get()
                && (info.is_hidden() || info.name().to_string_lossy().starts_with('.'))
            {
                return false;
            }
            // Name search (case-insensitive substring).
            let term = search_term.borrow();
            term.is_empty() || info.display_name().to_lowercase().contains(term.as_str())
        })
    };
    let filtered = gtk::FilterListModel::new(Some(dir_list.clone()), Some(filter.clone()));
    let sort_model = gtk::SortListModel::new(Some(filtered), None::<gtk::Sorter>);
    let selection = gtk::MultiSelection::new(Some(sort_model.clone()));

    let grid_size = app.zoom.get();
    let grid_view = gtk::GridView::builder().model(&selection).max_columns(24).build();
    grid_view.set_factory(Some(&grid_factory(grid_size)));
    grid_view.add_css_class("fs-grid");
    let grid_scroller =
        gtk::ScrolledWindow::builder().vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).child(&grid_view).build();

    let column_view =
        gtk::ColumnView::builder().model(&selection).show_column_separators(false).build();
    fill_columns(&column_view, list_icon(grid_size));
    if let Some(sorter) = column_view.sorter() {
        sort_model.set_sorter(Some(&sorter));
    }
    // Seed this tab with the shared sort selection (defaults to Name ascending).
    if let Some(col) =
        column_view.columns().item(app.sort_key.get() as u32).and_downcast::<gtk::ColumnViewColumn>()
    {
        let dir =
            if app.sort_desc.get() { gtk::SortType::Descending } else { gtk::SortType::Ascending };
        column_view.sort_by_column(Some(&col), dir);
    }
    let list_scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&column_view).build();

    let computer_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let computer_scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&computer_box).build();

    let view_stack = gtk::Stack::new();
    view_stack.add_named(&grid_scroller, Some("grid"));
    view_stack.add_named(&list_scroller, Some("list"));
    view_stack.add_named(&computer_scroller, Some("computer"));

    let breadcrumb = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
    breadcrumb.add_css_class("crumbs");
    breadcrumb.add_css_class("linked");
    breadcrumb.set_margin_start(8);
    breadcrumb.set_margin_top(6);
    breadcrumb.set_margin_bottom(6);
    let crumb_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .child(&breadcrumb)
        .build();

    // Per-tab search bar (hidden until Ctrl+F). Typing filters the current folder
    // by name via the shared `search_term`.
    let search_entry = gtk::SearchEntry::builder().placeholder_text("Search this folder").build();
    let search_bar = gtk::SearchBar::builder().build();
    search_bar.set_child(Some(&search_entry));
    search_bar.connect_entry(&search_entry);
    {
        let search_term = search_term.clone();
        let filter = filter.clone();
        search_entry.connect_search_changed(move |e| {
            *search_term.borrow_mut() = e.text().to_lowercase();
            filter.changed(gtk::FilterChange::Different);
        });
    }
    {
        // Closing the search bar clears the term so the full folder returns.
        let search_term = search_term.clone();
        let filter = filter.clone();
        let entry = search_entry.clone();
        search_bar.connect_search_mode_enabled_notify(move |bar| {
            if !bar.is_search_mode() {
                entry.set_text("");
                search_term.borrow_mut().clear();
                filter.changed(gtk::FilterChange::Different);
            }
        });
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&crumb_scroller);
    content.append(&search_bar);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&view_stack);

    let tab = Rc::new(Tab {
        page: RefCell::new(None),
        dir_list,
        filter,
        selection: selection.clone(),
        view_stack,
        grid_view: grid_view.clone(),
        column_view: column_view.clone(),
        breadcrumb,
        computer_box,
        search_bar: search_bar.clone(),
        search_entry: search_entry.clone(),
        back: RefCell::new(Vec::new()),
        fwd: RefCell::new(Vec::new()),
    });

    // Register the tab before appending: appending the first page auto-selects
    // it, which fires `selected-page-notify` → `active_tab()`; the tab must
    // already be in `app.tabs` (its page is filled in a moment later).
    app.tabs.borrow_mut().push(tab.clone());
    let page = app.tab_view.append(&content);
    page.set_title("Home");
    *tab.page.borrow_mut() = Some(page.clone());

    // Activation on both views.
    {
        let app = app.clone();
        let tab = tab.clone();
        grid_view.connect_activate(move |_, pos| activate(&app, &tab, pos));
    }
    {
        let app = app.clone();
        let tab = tab.clone();
        column_view.connect_activate(move |_, pos| activate(&app, &tab, pos));
    }
    // Selection → chrome.
    {
        let app = app.clone();
        selection.connect_selection_changed(move |_, _, _| update_chrome(&app));
    }
    {
        let app = app.clone();
        selection.connect_items_changed(move |_, _, _, _| update_chrome(&app));
    }
    // Context menu.
    attach_context_menu(app, &grid_view);
    attach_context_menu(app, &column_view);

    // Drag & drop: drag the selection out of either view, and drop files onto a
    // folder (into it) or empty space (into the current folder), Nautilus-style.
    attach_drag_source(&tab, &grid_view);
    attach_drag_source(&tab, &column_view);
    attach_drop_target(app, &tab, &grid_view);
    attach_drop_target(app, &tab, &column_view);

    set_dir(app, &tab, &dir);
    if select {
        app.tab_view.set_selected_page(&page);
    }
}

/// The currently active tab (there is always at least one).
fn active_tab(app: &Rc<App>) -> Rc<Tab> {
    if let Some(page) = app.tab_view.selected_page()
        && let Some(t) =
            app.tabs.borrow().iter().find(|t| t.page.borrow().as_ref() == Some(&page)).cloned()
    {
        return t;
    }
    app.tabs.borrow()[0].clone()
}

// --- Navigation -------------------------------------------------------------

fn navigate(app: &Rc<App>, tab: &Rc<Tab>, file: gio::File) {
    if let Some(current) = tab.dir_list.file() {
        tab.back.borrow_mut().push(current);
    }
    tab.fwd.borrow_mut().clear();
    set_dir(app, tab, &file);
}

fn go_back(app: &Rc<App>, tab: &Rc<Tab>) {
    let Some(prev) = tab.back.borrow_mut().pop() else { return };
    if let Some(current) = tab.dir_list.file() {
        tab.fwd.borrow_mut().push(current);
    }
    set_dir(app, tab, &prev);
}

fn go_forward(app: &Rc<App>, tab: &Rc<Tab>) {
    let Some(next) = tab.fwd.borrow_mut().pop() else { return };
    if let Some(current) = tab.dir_list.file() {
        tab.back.borrow_mut().push(current);
    }
    set_dir(app, tab, &next);
}

fn go_up(app: &Rc<App>, tab: &Rc<Tab>) {
    if let Some(parent) = tab.dir_list.file().and_then(|f| f.parent()) {
        navigate(app, tab, parent);
    }
}

/// Point `tab` at `file`, leaving any "This PC" view, and refresh chrome.
fn set_dir(app: &Rc<App>, tab: &Rc<Tab>, file: &gio::File) {
    // A new folder starts unfiltered — close any open search from the last one.
    tab.search_bar.set_search_mode(false);
    tab.dir_list.set_file(Some(file));
    tab.view_stack.set_visible_child_name(if app.is_list.get() { "list" } else { "grid" });
    rebuild_breadcrumb(app, tab, file);
    if let Some(page) = tab.page.borrow().as_ref() {
        let title =
            file.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        page.set_title(&title);
    }
    update_chrome(app);
}

fn refresh(tab: &Rc<Tab>) {
    if let Some(file) = tab.dir_list.file() {
        tab.dir_list.set_file(gio::File::NONE);
        tab.dir_list.set_file(Some(&file));
    }
}

fn rebuild_breadcrumb(app: &Rc<App>, tab: &Rc<Tab>, file: &gio::File) {
    while let Some(child) = tab.breadcrumb.first_child() {
        tab.breadcrumb.remove(&child);
    }
    let mut chain = Vec::new();
    let mut cursor = Some(file.clone());
    while let Some(f) = cursor {
        cursor = f.parent();
        chain.push(f);
    }
    chain.reverse();
    let last = chain.len().saturating_sub(1);
    for (i, f) in chain.into_iter().enumerate() {
        let label =
            f.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        let button = gtk::Button::builder().label(&label).build();
        button.add_css_class("flat");
        if i == last {
            button.add_css_class("current");
        }
        {
            let app = app.clone();
            let tab = tab.clone();
            let target = f.clone();
            button.connect_clicked(move |_| navigate(&app, &tab, target.clone()));
        }
        tab.breadcrumb.append(&button);
    }
}

// --- This PC (drives) -------------------------------------------------------

/// Populate and show the "This PC" drives view in `tab`.
fn show_computer(app: &Rc<App>, tab: &Rc<Tab>) {
    let box_ = &tab.computer_box;
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
    box_.set_margin_top(16);
    box_.set_margin_bottom(16);
    box_.set_margin_start(16);
    box_.set_margin_end(16);
    box_.set_spacing(12);

    let heading = gtk::Label::builder().label("This PC").xalign(0.0).build();
    heading.add_css_class("title-2");
    box_.append(&heading);

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(12)
        .row_spacing(12)
        .max_children_per_line(4)
        .min_children_per_line(1)
        .homogeneous(true)
        .build();

    for item in drives() {
        flow.insert(&drive_card(app, tab, &item), -1);
    }
    box_.append(&flow);

    if let Some(page) = tab.page.borrow().as_ref() {
        page.set_title("This PC");
    }
    tab.view_stack.set_visible_child_name("computer");
    update_chrome(app);
}

/// A drive tile's backing data: either a mounted location we can open directly,
/// or a connected-but-unmounted volume we mount on click.
enum DriveItem {
    /// A mounted location. `mount` is `Some` for a real volume (so it can be
    /// unmounted) and `None` for the always-present filesystem root. `usage` is
    /// `Some((used, total))` only for synthetic demo drives; real drives leave it
    /// `None` and their capacity is queried live.
    Mounted {
        name: String,
        icon: gio::Icon,
        file: gio::File,
        mount: Option<gio::Mount>,
        usage: Option<(u64, u64)>,
    },
    Unmounted { name: String, icon: gio::Icon, volume: gio::Volume },
}

/// The drives to show: the filesystem root, every mounted volume, and every
/// connected volume that isn't mounted yet (USB sticks, other partitions, …).
fn drives() -> Vec<DriveItem> {
    // Docs/screenshot mode: show synthetic drives instead of the machine's real
    // volumes, so generated screenshots never expose real drive labels.
    if std::env::var_os("FILESCOPE_DEMO").is_some() {
        return demo_drives();
    }

    let mut out: Vec<DriveItem> = Vec::new();
    out.push(DriveItem::Mounted {
        name: "Filesystem".to_string(),
        icon: fallback_icon(),
        file: gio::File::for_path("/"),
        mount: None,
        usage: None,
    });

    let monitor = gio::VolumeMonitor::get();
    for mount in monitor.mounts() {
        let file = mount.default_location();
        // Skip the root (already added) and anything without a real path.
        if file.path().as_deref() == Some(std::path::Path::new("/")) {
            continue;
        }
        out.push(DriveItem::Mounted {
            name: mount.name().to_string(),
            icon: mount.icon(),
            file,
            mount: Some(mount),
            usage: None,
        });
    }
    // Connected volumes with no mount yet — shown so the user can see and mount
    // them from here rather than hunting in another app.
    for volume in monitor.volumes() {
        if volume.get_mount().is_some() {
            continue; // already surfaced via its mount above
        }
        out.push(DriveItem::Unmounted {
            name: volume.name().to_string(),
            icon: volume.icon(),
            volume,
        });
    }
    out
}

fn fallback_icon() -> gio::Icon {
    gio::Icon::for_string("drive-harddisk-symbolic").expect("built-in icon name is valid")
}

/// Synthetic drives for screenshots (see `FILESCOPE_DEMO`). All point at the
/// demo folder so clicking works, but carry made-up names — no real volumes.
fn demo_drives() -> Vec<DriveItem> {
    let icon = |n: &str| gio::Icon::for_string(n).unwrap_or_else(|_| fallback_icon());
    let demo = std::env::var("FILESCOPE_SHOT_DIR").unwrap_or_else(|_| "/".to_string());
    let gb = 1_000_000_000u64;
    vec![
        DriveItem::Mounted {
            name: "Filesystem".to_string(),
            icon: fallback_icon(),
            file: gio::File::for_path("/"),
            mount: None,
            usage: Some((512 * gb, 1000 * gb)),
        },
        DriveItem::Mounted {
            name: "Local Disk".to_string(),
            icon: icon("drive-harddisk-symbolic"),
            file: gio::File::for_path(&demo),
            mount: None,
            usage: Some((340 * gb, 500 * gb)),
        },
        DriveItem::Mounted {
            name: "USB Drive".to_string(),
            icon: icon("drive-removable-media-symbolic"),
            file: gio::File::for_path(&demo),
            mount: None,
            usage: Some((13 * gb, 64 * gb)),
        },
    ]
}

/// One drive tile. A mounted drive shows its capacity ("X free of Y") and opens
/// on click; an unmounted volume shows a "Not mounted" hint and mounts (then
/// opens) on click.
fn drive_card(app: &Rc<App>, tab: &Rc<Tab>, item: &DriveItem) -> gtk::Widget {
    let (label, icon) = match item {
        DriveItem::Mounted { name, icon, .. } => (name.as_str(), icon),
        DriveItem::Unmounted { name, icon, .. } => (name.as_str(), icon),
    };

    let image = gtk::Image::from_gicon(icon);
    image.set_pixel_size(32);
    let name = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    name.add_css_class("heading");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    head.append(&image);
    head.append(&name);

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("drive-bar");
    let caption = gtk::Label::builder().xalign(0.0).build();
    caption.add_css_class("dim-label");
    caption.add_css_class("caption");

    match item {
        DriveItem::Mounted { file, usage, .. } => {
            // Demo drives carry canned figures; real drives are queried live.
            let capacity = usage.or_else(|| {
                let info = file.query_filesystem_info("filesystem::*", gio::Cancellable::NONE).ok()?;
                let size = info.attribute_uint64("filesystem::size");
                let free = info.attribute_uint64("filesystem::free");
                (size > 0).then_some((size.saturating_sub(free), size))
            });
            if let Some((used, size)) = capacity
                && size > 0
            {
                bar.set_fraction(used as f64 / size as f64);
                caption.set_label(&format!(
                    "{} free of {}",
                    format::human_size(size - used),
                    format::human_size(size)
                ));
            }
        }
        DriveItem::Unmounted { .. } => {
            bar.set_visible(false);
            caption.set_label("Not mounted — click to mount");
        }
    }

    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    card.add_css_class("drive-card");
    card.add_css_class("card");
    card.append(&head);
    card.append(&bar);
    card.append(&caption);

    let button = gtk::Button::builder().child(&card).build();
    button.add_css_class("flat");
    {
        let app = app.clone();
        let tab = tab.clone();
        match item {
            DriveItem::Mounted { file, .. } => {
                let file = file.clone();
                button.connect_clicked(move |_| navigate(&app, &tab, file.clone()));
            }
            DriveItem::Unmounted { volume, .. } => {
                let volume = volume.clone();
                button.connect_clicked(move |_| mount_and_open(&app, &tab, &volume));
            }
        }
    }
    button.upcast()
}

/// Unmount `mount`. If a tab is currently inside it, send that tab Home first so
/// it isn't left showing a directory that's about to disappear. Runs async.
fn unmount(app: &Rc<App>, mount: &gio::Mount) {
    // Any tab sitting inside this mount gets sent Home before it vanishes.
    if let Some(root) = mount.default_location().path() {
        for tab in app.tabs.borrow().iter() {
            if tab.dir_list.file().and_then(|f| f.path()).is_some_and(|p| p.starts_with(&root)) {
                let tab = tab.clone();
                let app = app.clone();
                navigate(&app, &tab, gio::File::for_path(glib::home_dir()));
            }
        }
    }

    let op = gio::MountOperation::new();
    let name = mount.name().to_string();
    let app = app.clone();
    mount.unmount_with_operation(
        gio::MountUnmountFlags::NONE,
        Some(&op),
        gio::Cancellable::NONE,
        move |res| match res {
            Ok(()) => {
                populate_sidebar(&app);
                toast(&app, &format!("Unmounted {name}"));
            }
            Err(err) => {
                if !err.matches(gio::IOErrorEnum::FailedHandled) {
                    toast(&app, &format!("Couldn't unmount {name}: {err}"));
                }
            }
        },
    );
}

/// Mount `volume`, then open it in `tab`. Runs asynchronously; the platform
/// shows a prompt (for passwords etc.) when the volume needs one.
fn mount_and_open(app: &Rc<App>, tab: &Rc<Tab>, volume: &gio::Volume) {
    let op = gio::MountOperation::new();
    let app = app.clone();
    let tab = tab.clone();
    let volume = volume.clone();
    volume.clone().mount(
        gio::MountMountFlags::NONE,
        Some(&op),
        gio::Cancellable::NONE,
        move |res| match res {
            Ok(()) => {
                populate_sidebar(&app);
                match volume.get_mount() {
                    Some(mount) => navigate(&app, &tab, mount.default_location()),
                    None => toast(&app, "Mounted, but couldn't locate the drive"),
                }
            }
            Err(err) => {
                // The user dismissing the mount prompt reports FailedHandled.
                if err.matches(gio::IOErrorEnum::FailedHandled) {
                    return;
                }
                // GIO couldn't mount it (commonly an NTFS volume on a system
                // without gvfs/udisks NTFS support) — fall back to ntfs-3g.
                ntfs3g_fallback(&app, &tab, &volume, &err);
            }
        },
    );
}

/// Mount `volume` with ntfs-3g directly, then open it. Used when GIO's own mount
/// fails — chiefly for NTFS partitions. Prefers `pkexec` so no terminal is
/// needed; the mount is owned by the current user (uid/gid options) and mounted
/// under a user-writable point in the runtime dir. Runs asynchronously.
fn ntfs3g_fallback(app: &Rc<App>, tab: &Rc<Tab>, volume: &gio::Volume, err: &glib::Error) {
    let Some(device) = volume.identifier("unix-device") else {
        toast(app, &format!("Couldn't mount: {err}"));
        return;
    };
    if !has_binary("ntfs-3g") {
        toast(app, "This drive needs NTFS support — install the ntfs-3g package.");
        return;
    }

    // A user-owned mount point: <runtime>/filescope-mounts/<sanitized name>.
    let safe: String = volume
        .name()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let mut mount_point = glib::user_runtime_dir();
    mount_point.push("filescope-mounts");
    mount_point.push(if safe.is_empty() { "drive".to_string() } else { safe });
    if let Err(e) = std::fs::create_dir_all(&mount_point) {
        toast(app, &format!("Couldn't prepare a mount point: {e}"));
        return;
    }

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let options = format!("uid={uid},gid={gid},umask=022");

    // pkexec raises a graphical auth prompt; without it, try ntfs-3g directly
    // (works when it is configured to let the user mount).
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    if has_binary("pkexec") {
        argv.push("pkexec".into());
    }
    argv.push("ntfs-3g".into());
    argv.push("-o".into());
    argv.push(options.into());
    argv.push(device.as_str().into());
    argv.push(mount_point.clone().into_os_string());
    let argv_refs: Vec<&std::ffi::OsStr> = argv.iter().map(|s| s.as_os_str()).collect();

    match gio::Subprocess::newv(&argv_refs, gio::SubprocessFlags::NONE) {
        Ok(process) => {
            let app = app.clone();
            let tab = tab.clone();
            process.wait_check_async(gio::Cancellable::NONE, move |res| match res {
                Ok(()) => navigate(&app, &tab, gio::File::for_path(&mount_point)),
                Err(e) => toast(&app, &format!("ntfs-3g couldn't mount the drive: {e}")),
            });
        }
        Err(e) => toast(app, &format!("Couldn't run ntfs-3g: {e}")),
    }
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
        tab.grid_view.set_factory(Some(&grid_factory(new)));
        fill_columns(&tab.column_view, list_icon(new));
        if let Some(sorter) = tab.column_view.sorter()
            && let Some(sm) = tab.selection.model().and_downcast::<gtk::SortListModel>()
        {
            sm.set_sorter(Some(&sorter));
        }
        // Rebuilding the columns clears the active sort column; restore it.
        apply_sort_to(app, tab);
    }
}

/// List-view icon size derived from the grid zoom.
fn list_icon(grid_size: i32) -> i32 {
    (grid_size / 4).clamp(16, 40)
}

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
    action!("back", |s: &Rc<App>| { let t = active_tab(s); go_back(s, &t); });
    action!("forward", |s: &Rc<App>| { let t = active_tab(s); go_forward(s, &t); });
    action!("up", |s: &Rc<App>| { let t = active_tab(s); go_up(s, &t); });
    action!("home", |s: &Rc<App>| { let t = active_tab(s); navigate(s, &t, gio::File::for_path(glib::home_dir())); });
    action!("open", open_selected);
    action!("open-with", open_with);
    action!("preview", toggle_preview);
    action!("copy", |s: &Rc<App>| set_clipboard(s, false));
    action!("cut", |s: &Rc<App>| set_clipboard(s, true));
    action!("paste", paste);
    action!("rename", rename_selected);
    action!("trash", trash_selected);
    action!("delete", delete_selected);
    action!("new-folder", new_folder);
    action!("new-tab", |s: &Rc<App>| { new_tab(s, gio::File::for_path(glib::home_dir()), true); });
    action!("close-tab", |s: &Rc<App>| { let t = active_tab(s); if let Some(p) = t.page.borrow().as_ref() { s.tab_view.close_page(p); } });
    action!("select-all", |s: &Rc<App>| { active_tab(s).selection.select_all(); });
    action!("refresh", |s: &Rc<App>| { refresh(&active_tab(s)); });
    action!("find", |s: &Rc<App>| {
        let t = active_tab(s);
        let show = !t.search_bar.is_search_mode();
        t.search_bar.set_search_mode(show);
        if show {
            t.search_entry.grab_focus();
        }
    });
    action!("properties", show_properties);
    action!("bookmark", bookmark_current);
    action!("zoom-in", |s: &Rc<App>| zoom(s, ZOOM_STEP));
    action!("zoom-out", |s: &Rc<App>| zoom(s, -ZOOM_STEP));
    action!("about", about);

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

    for (name, accels) in [
        ("back", &["<alt>Left"][..]),
        ("forward", &["<alt>Right"]),
        ("up", &["<alt>Up", "BackSpace"]),
        ("home", &["<alt>Home"]),
        ("open", &["Return"]),
        ("preview", &["space"]),
        ("copy", &["<ctrl>c"]),
        ("cut", &["<ctrl>x"]),
        ("paste", &["<ctrl>v"]),
        ("rename", &["F2"]),
        ("trash", &["Delete"]),
        ("delete", &["<shift>Delete"]),
        ("new-folder", &["<ctrl><shift>n"]),
        ("new-tab", &["<ctrl>t"]),
        ("close-tab", &["<ctrl>w"]),
        ("select-all", &["<ctrl>a"]),
        ("refresh", &["<ctrl>r", "F5"]),
        ("find", &["<ctrl>f"]),
        ("toggle-hidden", &["<ctrl>h"]),
        ("bookmark", &["<ctrl>d"]),
        ("zoom-in", &["<ctrl>plus", "<ctrl>equal"]),
        ("zoom-out", &["<ctrl>minus"]),
    ] {
        app.set_accels_for_action(&format!("win.{name}"), accels);
    }
}

fn selected(tab: &Rc<Tab>) -> Vec<(gio::File, gio::FileInfo)> {
    let Some(dir) = tab.dir_list.file() else { return Vec::new() };
    let bitset = tab.selection.selection();
    let mut out = Vec::new();
    for i in 0..bitset.size() {
        let pos = bitset.nth(i as u32);
        if let Some(obj) = tab.selection.item(pos) {
            let info = obj.downcast::<gio::FileInfo>().unwrap();
            out.push((dir.child(info.name()), info));
        }
    }
    out
}

fn activate(app: &Rc<App>, tab: &Rc<Tab>, position: u32) {
    let Some(item) = tab.selection.item(position) else { return };
    let info = item.downcast::<gio::FileInfo>().unwrap();
    let Some(dir) = tab.dir_list.file() else { return };
    let child = dir.child(info.name());
    if info.file_type() == gio::FileType::Directory {
        navigate(app, tab, child);
    } else {
        launch(app, &child);
    }
}

fn open_selected(app: &Rc<App>) {
    let tab = active_tab(app);
    let items = selected(&tab);
    if let [(file, info)] = items.as_slice()
        && info.file_type() == gio::FileType::Directory
    {
        navigate(app, &tab, file.clone());
        return;
    }
    for (file, info) in items {
        if info.file_type() != gio::FileType::Directory {
            launch(app, &file);
        }
    }
}

/// "Open With…": let the user pick which application opens the selected file.
/// Uses the platform app chooser (via `FileLauncher::always_ask`), so it lists
/// every app that handles the type plus an "Other Application…" escape hatch.
fn open_with(app: &Rc<App>) {
    let tab = active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else {
        return;
    };
    if info.file_type() == gio::FileType::Directory {
        return; // "Open With" is only meaningful for files.
    }
    let launcher = gtk::FileLauncher::new(Some(file));
    launcher.set_always_ask(true);
    let app = app.clone();
    launcher.launch(Some(&app.window.clone()), gio::Cancellable::NONE, move |res| {
        if let Err(err) = res {
            // Dismissing the chooser isn't an error worth a toast.
            if !err.matches(gtk::DialogError::Dismissed) {
                toast(&app, &format!("Couldn't open: {err}"));
            }
        }
    });
}

/// Space toggles a Quick-Look style preview of the single selected file.
fn toggle_preview(app: &Rc<App>) {
    if let Some(win) = app.preview.borrow_mut().take() {
        win.close();
        return;
    }
    let tab = active_tab(app);
    let items = selected(&tab);
    if let [(file, info)] = items.as_slice()
        && info.file_type() != gio::FileType::Directory
    {
        let win = preview::show(&app.window, file, info);
        {
            let app = app.clone();
            win.connect_close_request(move |_| {
                *app.preview.borrow_mut() = None;
                glib::Propagation::Proceed
            });
        }
        *app.preview.borrow_mut() = Some(win);
    }
}

fn launch(app: &Rc<App>, file: &gio::File) {
    let launcher = gtk::FileLauncher::new(Some(file));
    let app = app.clone();
    launcher.launch(Some(&app.window.clone()), gio::Cancellable::NONE, move |res| {
        if let Err(err) = res {
            toast(&app, &format!("Couldn't open: {err}"));
        }
    });
}

/// Put the selection on the **system** clipboard (so it pastes into other apps
/// too), tagged copy or cut.
fn set_clipboard(app: &Rc<App>, cut: bool) {
    let files: Vec<gio::File> = selected(&active_tab(app)).into_iter().map(|(f, _)| f).collect();
    if files.is_empty() {
        return;
    }
    let n = files.len();
    let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();

    // GNOME copy/cut format: "<action>\n<uri>\n<uri>…".
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let gnome_p = gdk::ContentProvider::for_bytes(GNOME_COPIED, &glib::Bytes::from(gnome.as_bytes()));
    // Generic URI list, CRLF-separated.
    let uri_list = format!("{}\r\n", uris.join("\r\n"));
    let uri_p = gdk::ContentProvider::for_bytes(URI_LIST, &glib::Bytes::from(uri_list.as_bytes()));

    let union = gdk::ContentProvider::new_union(&[gnome_p, uri_p]);
    let _ = app.window.clipboard().set_content(Some(&union));
    update_chrome(app);
    toast(app, &format!("{} {n} item{}", if cut { "Cut" } else { "Copied" }, plural(n)));
}

/// Read file URIs (and the cut/copy intent) currently on the clipboard, then
/// invoke `done`. Reads GNOME's format if present (for the intent), else the URI
/// list. Everything is async — the clipboard read never blocks the UI.
fn read_clipboard_files(clipboard: &gdk::Clipboard, done: impl Fn(bool, Vec<PathBuf>) + 'static) {
    clipboard.read_async(
        &[GNOME_COPIED, URI_LIST],
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |res| {
            let Ok((stream, mime)) = res else { return };
            stream.read_bytes_async(
                1 << 16,
                glib::Priority::DEFAULT,
                gio::Cancellable::NONE,
                move |bytes| {
                    let Ok(bytes) = bytes else { return };
                    let text = String::from_utf8_lossy(&bytes);
                    let (cut, uris) = parse_clipboard(mime.as_str(), &text);
                    let paths: Vec<PathBuf> =
                        uris.iter().filter_map(|u| gio::File::for_uri(u).path()).collect();
                    done(cut, paths);
                },
            );
        },
    );
}

/// Parse clipboard text into (is_cut, uris) for the GNOME copy/cut format or a
/// plain URI list.
fn parse_clipboard(mime: &str, text: &str) -> (bool, Vec<String>) {
    if mime == GNOME_COPIED {
        let mut lines = text.lines();
        let cut = lines.next().map(|a| a.trim() == "cut").unwrap_or(false);
        let uris = lines.filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
        (cut, uris)
    } else {
        let uris = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();
        (false, uris)
    }
}

/// Launch a background copy/move/delete job: spawn the worker thread, show a
/// progress row, and pump its events on the main loop (updating the bar, popping
/// conflict dialogs, refreshing when done).
fn start_job(app: &Rc<App>, kind: ops::Kind, sources: Vec<PathBuf>, dest: PathBuf) {
    if sources.is_empty() {
        return;
    }
    let (verb, past) = match kind {
        ops::Kind::Copy => ("Copying", "Copied"),
        ops::Kind::Move => ("Moving", "Moved"),
        ops::Kind::Delete => ("Deleting", "Deleted"),
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let (ev_tx, ev_rx) = async_channel::unbounded::<ops::Event>();
    let (dec_tx, dec_rx) = std::sync::mpsc::channel::<ops::Decision>();

    // Progress row: a title line and a bar with a stop button.
    let title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
    title.add_css_class("caption");
    title.set_label(&format!("{verb}…"));
    let bar = gtk::ProgressBar::builder().hexpand(true).valign(gtk::Align::Center).build();
    let stop = gtk::Button::from_icon_name("process-stop-symbolic");
    stop.add_css_class("flat");
    stop.set_tooltip_text(Some("Cancel"));
    {
        let cancel = cancel.clone();
        stop.connect_clicked(move |_| cancel.store(true, AtomicOrdering::Relaxed));
    }
    let bar_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar_row.append(&bar);
    bar_row.append(&stop);
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.append(&title);
    row.append(&bar_row);
    app.progress_box.append(&row);
    app.progress_revealer.set_reveal_child(true);

    // Worker thread.
    {
        let cancel = cancel.clone();
        std::thread::spawn(move || ops::run(kind, sources, dest, cancel, ev_tx, dec_rx));
    }

    // Pump events on the main loop.
    let app = app.clone();
    glib::spawn_future_local(async move {
        let (mut total_bytes, mut total_items) = (0u64, 0u64);
        while let Ok(event) = ev_rx.recv().await {
            match event {
                ops::Event::Total { bytes, items } => {
                    total_bytes = bytes;
                    total_items = items;
                }
                ops::Event::Advance { bytes_done, items_done, current } => {
                    let frac = if total_bytes > 0 {
                        bytes_done as f64 / total_bytes as f64
                    } else if total_items > 0 {
                        items_done as f64 / total_items as f64
                    } else {
                        0.0
                    };
                    bar.set_fraction(frac.clamp(0.0, 1.0));
                    let sizes = if total_bytes > 0 {
                        format!(" · {} / {}", format::human_size(bytes_done), format::human_size(total_bytes))
                    } else {
                        String::new()
                    };
                    title.set_label(&format!("{verb} {current} — {items_done}/{total_items}{sizes}"));
                }
                ops::Event::Conflict { name } => {
                    let dialog = adw::AlertDialog::new(
                        Some(&format!("“{name}” already exists")),
                        Some("An item with the same name is already in the destination."),
                    );
                    let apply_all = gtk::CheckButton::with_label("Apply to all conflicts");
                    dialog.set_extra_child(Some(&apply_all));
                    dialog.add_responses(&[
                        ("cancel", "Cancel"),
                        ("skip", "Skip"),
                        ("rename", "Keep Both"),
                        ("replace", "Replace"),
                    ]);
                    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
                    dialog.set_default_response(Some("rename"));
                    dialog.set_close_response("cancel");
                    let dec_tx = dec_tx.clone();
                    dialog.choose(&app.window, gio::Cancellable::NONE, move |resp| {
                        let all = apply_all.is_active();
                        let decision = match resp.as_str() {
                            "replace" => if all { ops::Decision::ReplaceAll } else { ops::Decision::Replace },
                            "skip" => if all { ops::Decision::SkipAll } else { ops::Decision::Skip },
                            "rename" => ops::Decision::Rename,
                            _ => ops::Decision::Cancel,
                        };
                        let _ = dec_tx.send(decision);
                    });
                }
                ops::Event::Finished { ok, failed } => {
                    app.progress_box.remove(&row);
                    if app.progress_box.first_child().is_none() {
                        app.progress_revealer.set_reveal_child(false);
                    }
                    for tab in app.tabs.borrow().iter() {
                        refresh(tab);
                    }
                    update_chrome(&app);
                    let message = if cancel.load(AtomicOrdering::Relaxed) {
                        format!("{verb} cancelled — {ok} done")
                    } else if failed > 0 {
                        format!("{past} {ok}; {failed} failed")
                    } else {
                        format!("{past} {ok} item{}", plural(ok as usize))
                    };
                    toast(&app, &message);
                    break;
                }
            }
        }
    });
}

fn paste(app: &Rc<App>) {
    let tab = active_tab(app);
    let Some(dir) = tab.dir_list.file().and_then(|f| f.path()) else { return };
    let clipboard = app.window.clipboard();
    let app = app.clone();
    read_clipboard_files(&clipboard, move |cut, sources| {
        if sources.is_empty() {
            return;
        }
        start_job(&app, if cut { ops::Kind::Move } else { ops::Kind::Copy }, sources, dir.clone());
        // A cut is consumed once pasted (matching Nautilus).
        if cut {
            let _ = app.window.clipboard().set_content(gdk::ContentProvider::NONE);
        }
    });
}

fn trash_selected(app: &Rc<App>) {
    let tab = active_tab(app);
    let items = selected(&tab);
    if items.is_empty() {
        return;
    }
    let (mut ok, mut failed) = (0u32, 0u32);
    for (file, _) in &items {
        if file.trash(gio::Cancellable::NONE).is_ok() { ok += 1 } else { failed += 1 }
    }
    refresh(&tab);
    toast(app, &format!("Moved {ok} item{} to Trash{}", plural(ok as usize),
        if failed > 0 { format!("; {failed} failed") } else { String::new() }));
}

fn delete_selected(app: &Rc<App>) {
    let tab = active_tab(app);
    let items = selected(&tab);
    if items.is_empty() {
        return;
    }
    let n = items.len();
    let dialog = adw::AlertDialog::new(
        Some(&format!("Permanently delete {n} item{}?", plural(n))),
        Some("This cannot be undone."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "delete" {
            return;
        }
        let sources: Vec<PathBuf> =
            selected(&active_tab(&app)).into_iter().filter_map(|(f, _)| f.path()).collect();
        start_job(&app, ops::Kind::Delete, sources, PathBuf::new());
    });
}

fn rename_selected(app: &Rc<App>) {
    let tab = active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else {
        return;
    };
    let file = file.clone();
    let entry =
        gtk::Entry::builder().text(info.display_name().as_str()).activates_default(true).build();
    let dialog = adw::AlertDialog::new(Some("Rename"), None);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "rename" {
            return;
        }
        let name = entry.text().to_string();
        if name.is_empty() || name.contains('/') {
            toast(&app, "Invalid name");
            return;
        }
        match file.path().ok_or(()).and_then(|p| fileops::rename(&p, &name).map_err(|_| ())) {
            Ok(_) => refresh(&active_tab(&app)),
            Err(()) => toast(&app, "Couldn't rename"),
        }
    });
}

fn new_folder(app: &Rc<App>) {
    let tab = active_tab(app);
    let Some(dir) = tab.dir_list.file().and_then(|f| f.path()) else { return };
    let entry = gtk::Entry::builder().text("New Folder").activates_default(true).build();
    let dialog = adw::AlertDialog::new(Some("New Folder"), None);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "create" {
            return;
        }
        let name = entry.text().to_string();
        if name.is_empty() || name.contains('/') {
            toast(&app, "Invalid name");
            return;
        }
        match fileops::make_dir(&dir, &name) {
            Ok(_) => refresh(&active_tab(&app)),
            Err(_) => toast(&app, "Couldn't create folder"),
        }
    });
}

/// A proper Properties dialog: an identity header (icon, name, kind) over
/// grouped rows for size/location, timestamps, and ownership/permissions — the
/// values selectable for copying. Folder sizes are computed off the UI thread.
fn show_properties(app: &Rc<App>) {
    let tab = active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else {
        return;
    };
    let file = file.clone();

    // Re-query for the richer attributes the directory listing doesn't carry
    // (access/created times, owner/group); fall back to the listing's info.
    let full = file
        .query_info(
            "standard::*,time::*,unix::*,owner::*",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok();
    let info: &gio::FileInfo = full.as_ref().unwrap_or(info);

    let name = info.display_name().to_string();
    let is_dir = info.file_type() == gio::FileType::Directory;
    let kind = if is_dir {
        "Folder".to_string()
    } else {
        info.content_type()
            .map(|ct| gio::content_type_get_description(&ct).to_string())
            .unwrap_or_else(|| "File".to_string())
    };
    let location = file
        .parent()
        .and_then(|p| p.path())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Identity header.
    let image = gtk::Image::builder().pixel_size(48).build();
    if let Some(icon) = info.icon() {
        image.set_from_gicon(&icon);
    }
    let title = gtk::Label::builder()
        .label(&name)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    title.add_css_class("title-2");
    let subtitle = gtk::Label::builder().label(&kind).build();
    subtitle.add_css_class("dim-label");
    let head = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    head.set_halign(gtk::Align::Center);
    head.append(&image);
    head.append(&title);
    head.append(&subtitle);

    // General.
    let general = adw::PreferencesGroup::new();
    general.add(&prop_row("Type", &kind));
    let size_row = prop_row("Size", "");
    general.add(&size_row);
    general.add(&prop_row("Location", &location));
    if is_dir {
        size_row.set_subtitle("Calculating…");
        if let Some(path) = file.path() {
            let size_row = size_row.clone();
            glib::spawn_future_local(async move {
                let (bytes, count) =
                    gio::spawn_blocking(move || fileops::dir_size(&path)).await.unwrap_or((0, 0));
                size_row.set_subtitle(&format!(
                    "{} — {count} item{}",
                    format::human_size(bytes),
                    plural(count as usize)
                ));
            });
        }
    } else {
        let bytes = info.size().max(0) as u64;
        size_row.set_subtitle(&format!("{} ({bytes} bytes)", format::human_size(bytes)));
    }

    // Timestamps.
    let time = adw::PreferencesGroup::new();
    time.set_title("Timestamps");
    if let Some(dt) = info.modification_date_time() {
        time.add(&prop_row("Modified", &format::modified(&dt)));
    }
    for (label, attr) in [("Accessed", "time::access"), ("Created", "time::created")] {
        let secs = info.attribute_uint64(attr);
        if secs > 0
            && let Ok(dt) = glib::DateTime::from_unix_local(secs as i64)
        {
            time.add(&prop_row(label, &format::modified(&dt)));
        }
    }

    // Ownership & permissions.
    let perms = adw::PreferencesGroup::new();
    perms.set_title("Permissions");
    if let Some(owner) = info.attribute_string("owner::user") {
        perms.add(&prop_row("Owner", &owner));
    }
    if let Some(group) = info.attribute_string("owner::group") {
        perms.add(&prop_row("Group", &group));
    }
    perms.add(&prop_row("Access", &permission_string(info.attribute_uint32("unix::mode"))));

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&head);
    content.append(&general);
    content.append(&time);
    content.append(&perms);
    let scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&content).build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroller));
    let dialog =
        adw::Dialog::builder().title("Properties").content_width(460).content_height(620).build();
    dialog.set_child(Some(&view));
    dialog.present(Some(&app.window));
}

/// One labelled, copy-selectable row in the Properties dialog.
fn prop_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(value).build();
    row.set_subtitle_selectable(true);
    row
}

fn bookmark_current(app: &Rc<App>) {
    let Some(path) = active_tab(app).dir_list.file().and_then(|f| f.path()) else { return };
    {
        let mut marks = app.bookmarks.borrow_mut();
        if marks.contains(&path) {
            toast(app, "Already bookmarked");
            return;
        }
        marks.push(path.clone());
        bookmarks::save(&marks);
    }
    populate_sidebar(app);
    toast(app, "Bookmark added");
}

fn remove_bookmark(app: &Rc<App>, path: &PathBuf) {
    {
        let mut marks = app.bookmarks.borrow_mut();
        marks.retain(|p| p != path);
        bookmarks::save(&marks);
    }
    populate_sidebar(app);
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
    let tab = active_tab(app);
    app.back_btn.set_sensitive(!tab.back.borrow().is_empty());
    app.fwd_btn.set_sensitive(!tab.fwd.borrow().is_empty());
    app.up_btn.set_sensitive(tab.dir_list.file().and_then(|f| f.parent()).is_some());

    // The window (and, via it, the header) title follows the active tab's
    // location, so it always names the folder you're in — or "This PC".
    let in_computer = tab.view_stack.visible_child_name().as_deref() == Some("computer");
    let win_title = if in_computer {
        "This PC".to_string()
    } else {
        tab.dir_list
            .file()
            .map(|f| f.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into()))
            .unwrap_or_else(|| "filescope".into())
    };
    app.window.set_title(Some(&win_title));

    let total = tab.selection.n_items();
    let sel = selected(&tab);
    let text = if in_computer {
        "This PC".to_string()
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
    for (name, enabled) in [
        ("open", has_sel), ("open-with", one), ("preview", one), ("cut", has_sel),
        ("copy", has_sel), ("paste", has_clip), ("rename", one), ("trash", has_sel),
        ("delete", has_sel), ("properties", one),
    ] {
        if let Some(act) = app.window.lookup_action(name) {
            act.downcast::<gio::SimpleAction>().unwrap().set_enabled(enabled);
        }
    }
}

// --- Sidebar ----------------------------------------------------------------

fn populate_sidebar(app: &Rc<App>) {
    let list = &app.sidebar_list;
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    // Places.
    use glib::UserDirectory::*;
    let mut places: Vec<(&str, String, PathBuf)> =
        vec![("user-home-symbolic", "Home".into(), glib::home_dir())];
    for (icon, label, dir) in [
        ("user-desktop-symbolic", "Desktop", Desktop),
        ("folder-documents-symbolic", "Documents", Documents),
        ("folder-download-symbolic", "Downloads", Downloads),
        ("folder-music-symbolic", "Music", Music),
        ("folder-pictures-symbolic", "Pictures", Pictures),
        ("folder-videos-symbolic", "Videos", Videos),
    ] {
        if let Some(path) = glib::user_special_dir(dir) {
            places.push((icon, label.into(), path));
        }
    }
    for (icon, label, path) in places {
        list.append(&place_row(icon, &label, Some(path), false));
    }

    // This PC (special row → drives view).
    list.append(&place_row("computer-symbolic", "This PC", None, true));

    // Devices: the filesystem root and each drive/partition, mounted or not.
    // Mounted ones open on click and carry an eject button; unmounted ones mount
    // on click.
    let items = drives();
    if items.len() > 1 {
        list.append(&section_header("Devices"));
        for item in items {
            list.append(&device_row(app, item));
        }
    }

    // Bookmarks.
    let marks = app.bookmarks.borrow().clone();
    if !marks.is_empty() {
        list.append(&section_header("Bookmarks"));
        for path in marks {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            list.append(&bookmark_row(app, &name, path));
        }
    }
}

fn section_header(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("dim-label");
    label.add_css_class("caption-heading");
    label.set_margin_top(10);
    label.set_margin_bottom(2);
    label.set_margin_start(12);
    gtk::ListBoxRow::builder().child(&label).selectable(false).activatable(false).build()
}

fn place_row(icon: &str, label: &str, path: Option<PathBuf>, computer: bool) -> gtk::ListBoxRow {
    let image = gtk::Image::from_icon_name(icon);
    let text = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build();
    let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.set_margin_start(6);
    row_box.set_margin_end(6);
    row_box.append(&image);
    row_box.append(&text);
    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    if let Some(path) = path {
        unsafe { row.set_data("path", path) };
    }
    if computer {
        unsafe { row.set_data("computer", true) };
    }
    row
}

/// A sidebar device row. Mounted → opens on click, with an eject button that
/// unmounts. Unmounted → mounts (then opens) on click.
fn device_row(app: &Rc<App>, item: DriveItem) -> gtk::ListBoxRow {
    let (label, icon) = match &item {
        DriveItem::Mounted { name, icon, .. } => (name.clone(), icon.clone()),
        DriveItem::Unmounted { name, icon, .. } => (name.clone(), icon.clone()),
    };
    let image = gtk::Image::from_gicon(&icon);
    let text = gtk::Label::builder()
        .label(&label)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.set_margin_start(6);
    row_box.set_margin_end(6);
    row_box.append(&image);
    row_box.append(&text);

    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    match item {
        DriveItem::Mounted { file, mount, .. } => {
            if let Some(path) = file.path() {
                unsafe { row.set_data("path", path) };
            }
            // Real volumes get an eject/unmount button; the root does not.
            if let Some(mount) = mount {
                text.set_tooltip_text(None);
                let eject = gtk::Button::builder()
                    .icon_name("media-eject-symbolic")
                    .tooltip_text("Unmount")
                    .build();
                eject.add_css_class("flat");
                eject.add_css_class("bookmark-remove");
                let app = app.clone();
                eject.connect_clicked(move |_| unmount(&app, &mount));
                row_box.append(&eject);
            }
        }
        DriveItem::Unmounted { volume, .. } => {
            text.add_css_class("dim-label");
            unsafe { row.set_data("volume", volume) };
        }
    }
    row
}

fn bookmark_row(app: &Rc<App>, label: &str, path: PathBuf) -> gtk::ListBoxRow {
    let image = gtk::Image::from_icon_name("folder-symbolic");
    let text = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let remove = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Remove bookmark").build();
    remove.add_css_class("flat");
    remove.add_css_class("bookmark-remove");
    {
        let app = app.clone();
        let path = path.clone();
        remove.connect_clicked(move |_| remove_bookmark(&app, &path));
    }
    let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.set_margin_start(6);
    row_box.set_margin_end(6);
    row_box.append(&image);
    row_box.append(&text);
    row_box.append(&remove);
    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    unsafe { row.set_data("path", path) };
    row
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
    let c = gio::Menu::new();
    c.append(Some("Rename…"), Some("win.rename"));
    c.append(Some("Move to Trash"), Some("win.trash"));
    c.append(Some("Delete Permanently…"), Some("win.delete"));
    menu.append_section(None, &c);
    let d = gio::Menu::new();
    d.append(Some("New Folder…"), Some("win.new-folder"));
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
    let tab = active_tab(app);
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
    a.append(Some("Bookmark This Folder"), Some("win.bookmark"));
    menu.append_section(None, &a);
    let b = gio::Menu::new();
    b.append(Some("Search…"), Some("win.find"));
    b.append(Some("Show Hidden Files"), Some("win.toggle-hidden"));
    b.append(Some("Zoom In"), Some("win.zoom-in"));
    b.append(Some("Zoom Out"), Some("win.zoom-out"));
    b.append(Some("Refresh"), Some("win.refresh"));
    menu.append_section(None, &b);
    let c = gio::Menu::new();
    c.append(Some("About filescope"), Some("win.about"));
    menu.append_section(None, &c);
    menu
}

fn attach_context_menu(app: &Rc<App>, widget: &impl IsA<gtk::Widget>) {
    let popover = gtk::PopoverMenu::from_model(Some(&context_menu()));
    popover.set_parent(widget);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    let app = app.clone();
    let popover = popover.clone();
    gesture.connect_pressed(move |g, _, x, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        update_chrome(&app);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    });
    widget.add_controller(gesture);
}

// --- Drag & drop ------------------------------------------------------------

/// Make a view draggable: dragging exports the current selection as a URI list,
/// so it can be dropped into another tab, another folder, or another app. Both
/// copy and move are offered (Ctrl forces copy, Shift forces move).
fn attach_drag_source(tab: &Rc<Tab>, view: &impl IsA<gtk::Widget>) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
    let tab = tab.clone();
    source.connect_prepare(move |_, _, _| {
        let files: Vec<gio::File> = selected(&tab).into_iter().map(|(f, _)| f).collect();
        if files.is_empty() {
            return None;
        }
        let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
        let bytes = glib::Bytes::from(format!("{}\r\n", uris.join("\r\n")).as_bytes());
        Some(gdk::ContentProvider::for_bytes(URI_LIST, &bytes))
    });
    view.add_controller(source);
}

/// Accept files dropped onto a view. A drop onto a folder goes *into* that folder;
/// a drop on empty space goes into the current folder. The action follows the
/// drag (move within the same filesystem by default, copy across; Ctrl/Shift
/// override), and the folder under the pointer highlights while hovering.
fn attach_drop_target(app: &Rc<App>, tab: &Rc<Tab>, view: &impl IsA<gtk::Widget>) {
    let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY | gdk::DragAction::MOVE);
    let highlighted: Rc<RefCell<Option<gtk::Widget>>> = Rc::new(RefCell::new(None));

    {
        let view = view.clone();
        let hl = highlighted.clone();
        target.connect_motion(move |_, x, y| {
            set_drop_highlight(&view, x, y, &hl);
            gdk::DragAction::COPY | gdk::DragAction::MOVE
        });
    }
    {
        let hl = highlighted.clone();
        target.connect_leave(move |_| clear_drop_highlight(&hl));
    }
    {
        let app = app.clone();
        let tab = tab.clone();
        let view = view.clone();
        target.connect_drop(move |target, value, x, y| {
            clear_drop_highlight(&highlighted);
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let sources: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            let Some(current) = tab.dir_list.file().and_then(|f| f.path()) else {
                return false;
            };
            // Into the folder under the pointer, or the current folder otherwise.
            let dest = match folder_name_at(&view, x, y) {
                Some(name) => current.join(name),
                None => current,
            };
            // Skip no-op / invalid targets: an item already in `dest`, or a folder
            // dropped into itself or one of its descendants.
            let sources: Vec<PathBuf> = sources
                .into_iter()
                .filter(|s| s.parent() != Some(dest.as_path()) && *s != dest && !dest.starts_with(s))
                .collect();
            if sources.is_empty() {
                return false;
            }
            let offered = target.current_drop().map(|d| d.actions()).unwrap_or(gdk::DragAction::COPY);
            let kind = drop_kind(offered, &sources, &dest);
            start_job(&app, kind, sources, dest);
            true
        });
    }
    view.add_controller(target);
}

/// Choose move vs copy for a drop the way Nautilus does: honour an explicit
/// modifier (Ctrl → copy, Shift → move, seen as a single-action offer), else move
/// when everything is on the destination's filesystem and copy when it crosses.
fn drop_kind(offered: gdk::DragAction, sources: &[PathBuf], dest: &Path) -> ops::Kind {
    let can_copy = offered.contains(gdk::DragAction::COPY);
    let can_move = offered.contains(gdk::DragAction::MOVE);
    if can_move && !can_copy {
        return ops::Kind::Move;
    }
    if can_copy && !can_move {
        return ops::Kind::Copy;
    }
    if same_filesystem(sources, dest) { ops::Kind::Move } else { ops::Kind::Copy }
}

/// Whether every source is on the same filesystem as `dest`.
fn same_filesystem(sources: &[PathBuf], dest: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(dest_dev) = std::fs::metadata(dest).map(|m| m.dev()) else {
        return false;
    };
    sources
        .iter()
        .all(|s| std::fs::symlink_metadata(s).map(|m| m.dev() == dest_dev).unwrap_or(false))
}

/// The name of the folder cell under `(x, y)` in `view`, if any.
fn folder_name_at(view: &impl IsA<gtk::Widget>, x: f64, y: f64) -> Option<String> {
    let mut widget = view.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if let Some(name) = unsafe { widget.data::<String>("fs-dir-name") } {
            let name = unsafe { name.as_ref() };
            if !name.is_empty() {
                return Some(name.clone());
            }
        }
        widget = widget.parent()?;
    }
}

/// Highlight the folder cell under `(x, y)`, un-highlighting any previous one.
fn set_drop_highlight(view: &impl IsA<gtk::Widget>, x: f64, y: f64, hl: &Rc<RefCell<Option<gtk::Widget>>>) {
    let cell = folder_cell_at(view, x, y);
    let mut current = hl.borrow_mut();
    if current.as_ref() == cell.as_ref() {
        return;
    }
    if let Some(prev) = current.take() {
        prev.remove_css_class("fs-drop");
    }
    if let Some(cell) = &cell {
        cell.add_css_class("fs-drop");
    }
    *current = cell;
}

fn clear_drop_highlight(hl: &Rc<RefCell<Option<gtk::Widget>>>) {
    if let Some(prev) = hl.borrow_mut().take() {
        prev.remove_css_class("fs-drop");
    }
}

/// Tag a cell with the folder name it drops into (empty for non-folders, so a
/// recycled cell never keeps a stale target).
fn mark_drop_folder(cell: &impl IsA<gtk::Widget>, info: &gio::FileInfo) {
    let name = if info.file_type() == gio::FileType::Directory {
        info.name().to_string_lossy().into_owned()
    } else {
        String::new()
    };
    unsafe { cell.set_data("fs-dir-name", name) };
}

/// The folder cell widget under `(x, y)`, walking up from the picked descendant.
fn folder_cell_at(view: &impl IsA<gtk::Widget>, x: f64, y: f64) -> Option<gtk::Widget> {
    let mut widget = view.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if let Some(name) = unsafe { widget.data::<String>("fs-dir-name") }
            && !unsafe { name.as_ref() }.is_empty()
        {
            return Some(widget);
        }
        widget = widget.parent()?;
    }
}

// --- Views ------------------------------------------------------------------

/// Rebuild `column_view`'s columns with the given leading-icon size.
fn fill_columns(column_view: &gtk::ColumnView, icon_size: i32) {
    while let Some(col) = column_view.columns().item(0).and_downcast::<gtk::ColumnViewColumn>() {
        column_view.remove_column(&col);
    }
    column_view.append_column(&name_column(icon_size));
    column_view.append_column(&size_column());
    column_view.append_column(&modified_column());
}

fn name_column(icon_size: i32) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let image = gtk::Image::new();
        let label =
            gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();
        row.append(&image);
        row.append(&label);
        item.set_child(Some(&row));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let row = item.child().and_downcast::<gtk::Box>().unwrap();
        let image = row.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = image.next_sibling().and_downcast::<gtk::Label>().unwrap();
        image.set_pixel_size(icon_size);
        set_themed_icon(&image, &info);
        if icon_size >= 32 {
            set_thumbnail(&image, &info, icon_size);
        }
        label.set_label(&info.display_name());
        // The name column ellipsizes; show the full name on hover.
        row.set_tooltip_text(Some(&info.display_name()));
        mark_drop_folder(&row, &info);
    });
    let col =
        gtk::ColumnViewColumn::builder().title("Name").factory(&factory).expand(true).resizable(true).build();
    col.set_sorter(Some(&info_sorter(|a, b| {
        a.display_name().to_lowercase().cmp(&b.display_name().to_lowercase())
    })));
    col
}

fn size_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::builder().xalign(1.0).build();
        label.add_css_class("numeric");
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        if info.file_type() == gio::FileType::Directory {
            label.set_label("");
        } else {
            label.set_label(&format::human_size(info.size().max(0) as u64));
        }
    });
    let col = gtk::ColumnViewColumn::builder().title("Size").factory(&factory).resizable(true).build();
    col.set_sorter(Some(&info_sorter(|a, b| a.size().cmp(&b.size()))));
    col
}

fn modified_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_child(Some(&gtk::Label::builder().xalign(0.0).build()));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        match info.modification_date_time() {
            Some(dt) => label.set_label(&format::modified(&dt)),
            None => label.set_label(""),
        }
    });
    let col =
        gtk::ColumnViewColumn::builder().title("Modified").factory(&factory).resizable(true).build();
    col.set_sorter(Some(&info_sorter(|a, b| {
        let key = |i: &gio::FileInfo| i.modification_date_time().map(|d| d.to_unix()).unwrap_or(0);
        key(a).cmp(&key(b))
    })));
    col
}

fn grid_factory(size: i32) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let image = gtk::Image::builder().pixel_size(size).build();
        let label = gtk::Label::builder()
            .justify(gtk::Justification::Center)
            .wrap(true)
            // Character-level wrapping: long unbroken filenames (e.g.
            // `_OceanofPDF.com_Everyday_Ayurveda_..._Bhattacharya.pdf`, common in
            // Downloads) have no spaces to break on. With the default word wrap
            // the label's *minimum* width becomes the whole filename, which blows
            // each grid cell out to full width and collapses the grid to a single
            // column. WordChar lets it break mid-word, keeping cells narrow.
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars((size / 6).max(8))
            .width_chars((size / 8).max(6))
            .build();
        label.add_css_class("fs-name");
        let cell = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        cell.set_halign(gtk::Align::Center);
        cell.append(&image);
        cell.append(&label);
        item.set_child(Some(&cell));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let cell = item.child().and_downcast::<gtk::Box>().unwrap();
        let image = cell.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = image.next_sibling().and_downcast::<gtk::Label>().unwrap();
        image.set_pixel_size(size);
        set_themed_icon(&image, &info);
        set_thumbnail(&image, &info, size);
        label.set_label(&info.display_name());
        // Names are wrapped/ellipsized in the grid, so surface the full name on
        // hover.
        cell.set_tooltip_text(Some(&info.display_name()));
        mark_drop_folder(&cell, &info);
    });
    factory
}

fn set_themed_icon(image: &gtk::Image, info: &gio::FileInfo) {
    if let Some(icon) = info.icon() {
        image.set_from_gicon(&icon);
    } else {
        image.set_icon_name(Some("text-x-generic"));
    }
}

/// For image files, replace the themed icon with a scaled thumbnail — decoded on
/// a background thread so a folder full of images never blocks the UI.
///
/// Each request tags the reused `Image` widget with a fresh token; when the
/// decode returns we only apply it if the widget still carries that token, so a
/// recycled cell (scrolled to a different file) is never overwritten by a stale
/// thumbnail.
fn set_thumbnail(image: &gtk::Image, info: &gio::FileInfo, size: i32) {
    static THUMB_GEN: AtomicU64 = AtomicU64::new(0);

    let is_image = info.content_type().map(|c| c.starts_with("image/")).unwrap_or(false);
    if !is_image {
        return;
    }
    let Some(file) = info.attribute_object("standard::file").and_downcast::<gio::File>() else {
        return;
    };
    let Some(path) = file.path() else { return };

    let token = THUMB_GEN.fetch_add(1, AtomicOrdering::Relaxed) + 1;
    unsafe { image.set_data("thumb-token", token) };

    let image = image.clone();
    glib::spawn_future_local(async move {
        let decoded = gio::spawn_blocking(move || decode_thumbnail(&path, size)).await;
        let Ok(Some((width, height, stride, has_alpha, pixels))) = decoded else {
            return;
        };
        // Only apply if this widget is still showing the file we decoded for.
        let current = unsafe { image.data::<u64>("thumb-token") };
        if current.map(|p| unsafe { *p.as_ref() }) != Some(token) {
            return;
        }
        let bytes = glib::Bytes::from_owned(pixels);
        let format =
            if has_alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
        let texture = gdk::MemoryTexture::new(width, height, format, &bytes, stride as usize);
        image.set_paintable(Some(&texture));
    });
}

/// Decode and scale an image to raw RGBA(/RGB) bytes off the main thread.
/// Returns `(width, height, stride, has_alpha, pixels)`.
fn decode_thumbnail(path: &std::path::Path, size: i32) -> Option<(i32, i32, i32, bool, Vec<u8>)> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, size, size, true).ok()?;
    let pixels = pixbuf.read_pixel_bytes().to_vec();
    Some((pixbuf.width(), pixbuf.height(), pixbuf.rowstride(), pixbuf.has_alpha(), pixels))
}

// --- Small helpers ----------------------------------------------------------

fn info_sorter<F>(cmp: F) -> gtk::CustomSorter
where
    F: Fn(&gio::FileInfo, &gio::FileInfo) -> Ordering + 'static,
{
    gtk::CustomSorter::new(move |a, b| {
        let a = a.downcast_ref::<gio::FileInfo>().unwrap();
        let b = b.downcast_ref::<gio::FileInfo>().unwrap();
        let a_dir = a.file_type() == gio::FileType::Directory;
        let b_dir = b.file_type() == gio::FileType::Directory;
        let ord = match (a_dir, b_dir) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => cmp(a, b),
        };
        ord.into()
    })
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
    use super::{GNOME_COPIED, URI_LIST, parse_clipboard};

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

    #[test]
    fn drop_kind_honours_modifier_then_filesystem() {
        use super::drop_kind;
        use crate::ops::Kind;
        use gtk::gdk::DragAction;

        // A drag next to a real dir on the same filesystem.
        let tmp = std::env::temp_dir().join(format!("filescope-drop-{}", std::process::id()));
        let dest = tmp.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let src = tmp.join("f");
        std::fs::write(&src, b"x").unwrap();
        let sources = [src];

        // An explicit single action wins regardless of filesystem.
        assert_eq!(drop_kind(DragAction::MOVE, &sources, &dest), Kind::Move);
        assert_eq!(drop_kind(DragAction::COPY, &sources, &dest), Kind::Copy);
        // No modifier (both offered) + same filesystem → move, like Nautilus.
        assert_eq!(drop_kind(DragAction::COPY | DragAction::MOVE, &sources, &dest), Kind::Move);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
