// The main window: a places/bookmarks sidebar beside a tabbed content area.
// Each tab is an independent folder view (grid or list, zoomable) with its own
// history and breadcrumb. A special "This PC" view lists drives with capacity.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{bookmarks, fileops, format, preview};

const CSS: &str = "
.fs-grid { padding: 8px; }
.fs-grid > child { border-radius: 12px; padding: 8px; margin: 2px; transition: background 120ms; }
.fs-grid > child:hover { background: alpha(@window_fg_color, 0.06); }
.fs-grid > child:selected { background: alpha(@accent_bg_color, 0.22); }
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

#[derive(Default)]
struct Clip {
    files: Vec<gio::File>,
    cut: bool,
}

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
    clipboard: RefCell<Clip>,
    /// Grid icon size in pixels (list scales from it); shared view mode.
    zoom: Cell<i32>,
    is_list: Cell<bool>,
    show_hidden: Rc<Cell<bool>>,
    bookmarks: RefCell<Vec<PathBuf>>,
    preview: RefCell<Option<adw::Window>>,
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

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&tab_view));
    let content = adw::ToolbarView::new();
    content.add_top_bar(&header);
    content.add_top_bar(&tab_bar);
    content.set_content(Some(&toasts));
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
        clipboard: RefCell::new(Clip::default()),
        zoom: Cell::new(80),
        is_list: Cell::new(false),
        show_hidden: Rc::new(Cell::new(false)),
        bookmarks: RefCell::new(bookmarks::load()),
        preview: RefCell::new(None),
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
            } else if let Some(path) = unsafe { row.data::<PathBuf>("path") } {
                navigate(&s, &t, gio::File::for_path(unsafe { path.as_ref() }.clone()));
            }
        });
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
}

// --- Tabs -------------------------------------------------------------------

/// Create a tab showing `dir`, optionally selecting it.
fn new_tab(app: &Rc<App>, dir: gio::File, select: bool) {
    let dir_list =
        gtk::DirectoryList::builder().attributes(ATTRS).io_priority(glib::Priority::DEFAULT).build();

    let show_hidden = app.show_hidden.clone();
    let filter = {
        let show_hidden = show_hidden.clone();
        gtk::CustomFilter::new(move |obj| {
            if show_hidden.get() {
                return true;
            }
            let info = obj.downcast_ref::<gio::FileInfo>().unwrap();
            !info.is_hidden() && !info.name().to_string_lossy().starts_with('.')
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
    if let Some(first) = column_view.columns().item(0).and_downcast::<gtk::ColumnViewColumn>() {
        column_view.sort_by_column(Some(&first), gtk::SortType::Ascending);
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

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&crumb_scroller);
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

    set_dir(app, &tab, &dir);
    if select {
        app.tab_view.set_selected_page(&page);
    }
}

/// The currently active tab (there is always at least one).
fn active_tab(app: &Rc<App>) -> Rc<Tab> {
    if let Some(page) = app.tab_view.selected_page() {
        if let Some(t) =
            app.tabs.borrow().iter().find(|t| t.page.borrow().as_ref() == Some(&page)).cloned()
        {
            return t;
        }
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

    for (label, icon, file) in drives() {
        flow.insert(&drive_card(app, tab, &label, &icon, file), -1);
    }
    box_.append(&flow);

    if let Some(page) = tab.page.borrow().as_ref() {
        page.set_title("This PC");
    }
    tab.view_stack.set_visible_child_name("computer");
    update_chrome(app);
}

/// The drives to show: the filesystem root plus every mounted volume.
fn drives() -> Vec<(String, gio::Icon, gio::File)> {
    let mut out: Vec<(String, gio::Icon, gio::File)> = Vec::new();
    out.push(("Filesystem".to_string(), fallback_icon(), gio::File::for_path("/")));

    let monitor = gio::VolumeMonitor::get();
    for mount in monitor.mounts() {
        let file = mount.default_location();
        // Skip the root (already added) and anything without a real path.
        if file.path().as_deref() == Some(std::path::Path::new("/")) {
            continue;
        }
        out.push((mount.name().to_string(), mount.icon(), file));
    }
    out
}

fn fallback_icon() -> gio::Icon {
    gio::Icon::for_string("drive-harddisk-symbolic").expect("built-in icon name is valid")
}

/// One drive tile: name, capacity bar, and "X free of Y".
fn drive_card(app: &Rc<App>, tab: &Rc<Tab>, label: &str, icon: &gio::Icon, file: gio::File) -> gtk::Widget {
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

    if let Ok(info) = file.query_filesystem_info("filesystem::*", gio::Cancellable::NONE) {
        let size = info.attribute_uint64("filesystem::size");
        let free = info.attribute_uint64("filesystem::free");
        if size > 0 {
            let used = size.saturating_sub(free);
            bar.set_fraction(used as f64 / size as f64);
            caption.set_label(&format!("{} free of {}", format::human_size(free), format::human_size(size)));
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
        button.connect_clicked(move |_| navigate(&app, &tab, file.clone()));
    }
    button.upcast()
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
        if let Some(sorter) = tab.column_view.sorter() {
            if let Some(sm) = tab.selection.model().and_downcast::<gtk::SortListModel>() {
                sm.set_sorter(Some(&sorter));
            }
        }
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
    if let [(file, info)] = items.as_slice() {
        if info.file_type() == gio::FileType::Directory {
            navigate(app, &tab, file.clone());
            return;
        }
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
    if let [(file, info)] = items.as_slice() {
        if info.file_type() != gio::FileType::Directory {
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

fn set_clipboard(app: &Rc<App>, cut: bool) {
    let files: Vec<gio::File> = selected(&active_tab(app)).into_iter().map(|(f, _)| f).collect();
    if files.is_empty() {
        return;
    }
    let n = files.len();
    *app.clipboard.borrow_mut() = Clip { files, cut };
    update_chrome(app);
    toast(app, &format!("{} {n} item{}", if cut { "Cut" } else { "Copied" }, plural(n)));
}

fn paste(app: &Rc<App>) {
    let tab = active_tab(app);
    let Some(dir) = tab.dir_list.file().and_then(|f| f.path()) else { return };
    let (files, cut) = {
        let clip = app.clipboard.borrow();
        (clip.files.clone(), clip.cut)
    };
    if files.is_empty() {
        return;
    }
    let (mut ok, mut failed) = (0u32, 0u32);
    for file in &files {
        let Some(src) = file.path() else {
            failed += 1;
            continue;
        };
        let r = if cut { fileops::move_into(&src, &dir) } else { fileops::copy_into(&src, &dir) };
        if r.is_ok() { ok += 1 } else { failed += 1 }
    }
    if cut {
        app.clipboard.borrow_mut().files.clear();
    }
    update_chrome(app);
    refresh(&tab);
    let verb = if cut { "Moved" } else { "Pasted" };
    toast(app, &format!("{verb} {ok} item{}{}", plural(ok as usize),
        if failed > 0 { format!("; {failed} failed") } else { String::new() }));
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
        let tab = active_tab(&app);
        let (mut ok, mut failed) = (0u32, 0u32);
        for (file, _) in &selected(&tab) {
            match file.path().ok_or(()).and_then(|p| fileops::remove(&p).map_err(|_| ())) {
                Ok(()) => ok += 1,
                Err(()) => failed += 1,
            }
        }
        refresh(&tab);
        toast(&app, &format!("Deleted {ok} item{}{}", plural(ok as usize),
            if failed > 0 { format!("; {failed} failed") } else { String::new() }));
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

fn show_properties(app: &Rc<App>) {
    let tab = active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else {
        return;
    };
    let name = info.display_name().to_string();
    let is_dir = info.file_type() == gio::FileType::Directory;
    let kind = if is_dir {
        "Folder".to_string()
    } else {
        info.content_type().map(|ct| gio::content_type_get_description(&ct).to_string()).unwrap_or_else(|| "File".to_string())
    };
    let size = if is_dir {
        file.path().and_then(|p| std::fs::read_dir(p).ok()).map(|rd| format!("{} items", rd.count())).unwrap_or_default()
    } else {
        format::human_size(info.size().max(0) as u64)
    };
    let location = file.parent().and_then(|p| p.path()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let modified = info.modification_date_time().map(|dt| format::modified(&dt)).unwrap_or_default();
    let perms = permission_string(info.attribute_uint32("unix::mode"));
    let body = format!(
        "Type:\t{kind}\nSize:\t{size}\nLocation:\t{location}\nModified:\t{modified}\nPermissions:\t{perms}"
    );
    let dialog = adw::AlertDialog::new(Some(&name), Some(&body));
    dialog.add_responses(&[("close", "Close")]);
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    dialog.present(Some(&app.window));
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
    let has_clip = !app.clipboard.borrow().files.is_empty();
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
    let row = gtk::ListBoxRow::builder().child(&label).selectable(false).activatable(false).build();
    row
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

fn primary_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let a = gio::Menu::new();
    a.append(Some("New Tab"), Some("win.new-tab"));
    a.append(Some("New Folder…"), Some("win.new-folder"));
    a.append(Some("Bookmark This Folder"), Some("win.bookmark"));
    menu.append_section(None, &a);
    let b = gio::Menu::new();
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
