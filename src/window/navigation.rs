// Tabs and folder navigation: history, breadcrumbs, and refreshing a listing.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

// --- Tabs -------------------------------------------------------------------

/// Create a tab showing `dir`, optionally selecting it.
pub(super) fn new_tab(app: &Rc<super::App>, dir: gio::File, select: bool) {
    let dir_list =
        gtk::DirectoryList::builder().attributes(super::ATTRS).io_priority(glib::Priority::DEFAULT).build();

    let show_hidden = app.show_hidden.clone();
    let filter = gtk::CustomFilter::new(move |obj| {
        let info = obj.downcast_ref::<gio::FileInfo>().unwrap();
        show_hidden.get() || !(info.is_hidden() || info.name().to_string_lossy().starts_with('.'))
    });
    let filtered = gtk::FilterListModel::new(Some(dir_list.clone()), Some(filter.clone()));
    let sort_model = gtk::SortListModel::new(Some(filtered), None::<gtk::Sorter>);
    let selection = gtk::MultiSelection::new(Some(sort_model.clone()));

    let grid_size = app.zoom.get();
    let grid_view = gtk::GridView::builder().model(&selection).max_columns(24).enable_rubberband(true).build();
    grid_view.add_css_class("fs-grid");
    let grid_scroller =
        gtk::ScrolledWindow::builder().vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).child(&grid_view).build();

    let column_view = gtk::ColumnView::builder()
        .model(&selection)
        .show_column_separators(false)
        .enable_rubberband(true)
        .build();
    let list_scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&column_view).build();

    let computer_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let computer_scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&computer_box).build();

    let trash_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    trash_box.set_vexpand(true);

    // Shown in place of the grid/list when the folder has no visible items.
    let empty_page = adw::StatusPage::builder()
        .title("This Folder Is Empty")
        .description("Drop files here, or paste to add some.")
        .build();
    crate::icon::show_status(&empty_page, "folder-symbolic", 128);

    let view_stack = gtk::Stack::new();
    view_stack.add_named(&grid_scroller, Some("grid"));
    view_stack.add_named(&list_scroller, Some("list"));
    view_stack.add_named(&computer_scroller, Some("computer"));
    view_stack.add_named(&trash_box, Some("trash"));
    view_stack.add_named(&empty_page, Some("empty"));

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

    let search_entry = gtk::SearchEntry::builder().placeholder_text("Search this folder and subfolders").hexpand(true).build();
    let search_bar = gtk::SearchBar::builder().build();
    search_bar.set_child(Some(&search_entry));
    search_bar.connect_entry(&search_entry);
    let search = super::search::Pane::new(&selection);
    view_stack.add_named(&search.widget, Some("search"));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&crumb_scroller);
    content.append(&search_bar);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let preview_pane = crate::preview::pane::Pane::new(app.preview_pane_default.get());
    let browser = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    view_stack.set_hexpand(true);
    browser.append(&view_stack);
    browser.append(&preview_pane.widget);
    content.append(&browser);
    let terminal_pane = super::terminal::Pane::new();
    content.append(&terminal_pane.widget());

    let tab = Rc::new(super::Tab {
        page: RefCell::new(None),
        dir_list,
        filter,
        selection: selection.clone(),
        folder_model: sort_model.clone(),
        search,
        view_stack,
        grid_view: grid_view.clone(),
        column_view: column_view.clone(),
        breadcrumb,
        computer_box,
        trash_box,
        trash_view: RefCell::new(None),
        preview_pane,
        search_bar: search_bar.clone(),
        search_entry: search_entry.clone(),
        back: RefCell::new(Vec::new()),
        fwd: RefCell::new(Vec::new()),
        grid_zoom: Rc::new(Cell::new(grid_size)),
        list_zoom: Rc::new(Cell::new(grid_size)),
        terminal: terminal_pane,
    });

    // Register the tab before appending: appending the first page auto-selects
    // it, which fires `selected-page-notify` → `active_tab()`; the tab must
    // already be in `app.tabs` (its page is filled in a moment later).
    app.tabs.borrow_mut().push(tab.clone());
    let page = app.tab_view.append(&content);
    page.set_title("Home");
    *tab.page.borrow_mut() = Some(page.clone());

    super::search::connect(app, &tab);

    // Activation on both views.
    {
        let app = Rc::downgrade(app);
        let tab = Rc::downgrade(&tab);
        grid_view.connect_activate(move |_, pos| { if let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) { super::commands::activate(&app, &tab, pos); } });
    }
    {
        let app = Rc::downgrade(app);
        let tab = Rc::downgrade(&tab);
        column_view.connect_activate(move |_, pos| { if let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) { super::commands::activate(&app, &tab, pos); } });
    }
    // Selection → chrome.
    {
        let app = Rc::downgrade(app);
        let tab = Rc::downgrade(&tab);
        selection.connect_selection_changed(move |_, _, _| {
            let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) else { return; };
            super::update_chrome(&app);
            super::update_preview(&tab);
        });
    }
    {
        let app = Rc::downgrade(app);
        let tab = Rc::downgrade(&tab);
        selection.connect_items_changed(move |_, _, _, _| {
            let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) else { return; };
            super::update_chrome(&app);
            refresh_view_mode(&app, &tab, false);
            super::update_preview(&tab);
        });
    }
    {
        let weak = Rc::downgrade(&tab);
        tab.view_stack.connect_visible_child_name_notify(move |_| {
            if let Some(tab) = weak.upgrade() { super::update_preview(&tab); }
        });
    }
    {
        let weak = Rc::downgrade(&tab);
        tab.dir_list.connect_file_notify(move |_| {
            if let Some(tab) = weak.upgrade() { super::update_preview(&tab); }
        });
    }
    // The listing loads asynchronously; re-evaluate the empty state once it does.
    {
        let dir_list = tab.dir_list.clone();
        let app = Rc::downgrade(app);
        let tab = Rc::downgrade(&tab);
        dir_list.connect_loading_notify(move |_| { if let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) { refresh_view_mode(&app, &tab, false); } });
    }
    // Context menu.
    super::view::attach_context_menu(app, &grid_view);
    super::view::attach_context_menu(app, &column_view);

    // Drag starts on an item. Empty space is the rubber band, set on the views above.
    // Drop still targets the whole view: a folder, or the empty space around it.
    grid_view.set_factory(Some(&super::view::grid_factory(Some(&tab), &grid_view, &tab.grid_zoom)));
    super::view::fill_columns(Some(&tab), &column_view, &tab.list_zoom);
    if let Some(sorter) = column_view.sorter() {
        sort_model.set_sorter(Some(&super::view::with_hidden_last(&sorter)));
    }
    // Seed this tab with the shared sort selection (defaults to Name ascending).
    if let Some(col) =
        column_view.columns().item(app.sort_key.get() as u32).and_downcast::<gtk::ColumnViewColumn>()
    {
        let dir =
            if app.sort_desc.get() { gtk::SortType::Descending } else { gtk::SortType::Ascending };
        column_view.sort_by_column(Some(&col), dir);
    }
    super::view::attach_drop_target(app, &tab, &grid_view);
    super::view::attach_drop_target(app, &tab, &column_view);

    // Space / Backspace / Delete act on the view, not globally — see
    // `attach_view_keys` for why they can't be accelerators.
    super::view::attach_view_keys(app, &grid_view);
    super::view::attach_view_keys(app, &column_view);

    set_dir(app, &tab, &dir);
    if select {
        app.tab_view.set_selected_page(&page);
    }
}

/// The currently active tab (there is always at least one).
pub(super) fn active_tab(app: &Rc<super::App>) -> Rc<super::Tab> {
    if let Some(page) = app.tab_view.selected_page()
        && let Some(t) =
            app.tabs.borrow().iter().find(|t| t.page.borrow().as_ref() == Some(&page)).cloned()
    {
        return t;
    }
    app.tabs.borrow()[0].clone()
}

// --- Navigation -------------------------------------------------------------

pub(super) fn navigate(app: &Rc<super::App>, tab: &Rc<super::Tab>, file: gio::File) {
    if let Some(place) = here(tab) {
        push_visit(&mut tab.back.borrow_mut(), &mut tab.fwd.borrow_mut(), place);
    }
    set_dir(app, tab, &file);
}

/// Leave the current page for Trash, so Back returns here.
pub(super) fn go_trash(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    if tab.view_stack.visible_child_name().as_deref() != Some("trash")
        && let Some(place) = here(tab)
    {
        push_visit(&mut tab.back.borrow_mut(), &mut tab.fwd.borrow_mut(), place);
    }
    super::places::show_trash(app, tab);
}

pub(super) fn go_back(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    let Some(here) = here(tab) else { return };
    let Some(prev) = retreat(&mut tab.back.borrow_mut(), &mut tab.fwd.borrow_mut(), here) else {
        return;
    };
    open_place(app, tab, prev);
}

pub(super) fn go_forward(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    let Some(here) = here(tab) else { return };
    let Some(next) = advance(&mut tab.back.borrow_mut(), &mut tab.fwd.borrow_mut(), here) else {
        return;
    };
    open_place(app, tab, next);
}

/// Record `here` and drop the forward stack. Opening something new always does this.
fn push_visit(back: &mut Vec<super::Place>, fwd: &mut Vec<super::Place>, here: super::Place) {
    back.push(here);
    fwd.clear();
}

/// Step back. Returns where to show, and remembers `here` for Forward.
fn retreat(back: &mut Vec<super::Place>, fwd: &mut Vec<super::Place>, here: super::Place) -> Option<super::Place> {
    let prev = back.pop()?;
    fwd.push(here);
    Some(prev)
}

/// Step forward. Returns where to show, and remembers `here` for Back.
fn advance(back: &mut Vec<super::Place>, fwd: &mut Vec<super::Place>, here: super::Place) -> Option<super::Place> {
    let next = fwd.pop()?;
    back.push(here);
    Some(next)
}

/// Where the tab is showing right now, for the back/forward stack.
pub(super) fn here(tab: &super::Tab) -> Option<super::Place> {
    if tab.search.active.get() {
        return tab.dir_list.file().map(|root| super::Place::Search { root, query: tab.search.query.borrow().clone() });
    }
    match tab.view_stack.visible_child_name().as_deref() {
        Some("computer") => Some(super::Place::Computer),
        Some("trash") => Some(super::Place::Trash),
        _ => tab.dir_list.file().map(super::Place::Folder),
    }
}

fn open_place(app: &Rc<super::App>, tab: &Rc<super::Tab>, place: super::Place) {
    match place {
        super::Place::Computer => super::places::show_computer(app, tab),
        super::Place::Trash => super::places::show_trash(app, tab),
        super::Place::Folder(file) => set_dir(app, tab, &file),
        super::Place::Search { root, query } => {
            set_dir(app, tab, &root);
            tab.search_bar.set_search_mode(true);
            tab.search_entry.set_text(&query);
            super::search::changed(app, tab, false);
        }
    }
}

pub(super) fn go_up(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    if let Some(parent) = tab.dir_list.file().and_then(|f| f.parent()) {
        navigate(app, tab, parent);
    }
}

/// Point `tab` at `file`, leaving any "This PC" view, and refresh chrome.
pub(super) fn set_dir(app: &Rc<super::App>, tab: &Rc<super::Tab>, file: &gio::File) {
    // A new folder starts unfiltered — close any open search from the last one.
    tab.search_bar.set_search_mode(false);
    tab.dir_list.set_file(Some(file));
    set_path_visible(tab, true);
    // Force out of any "This PC" view into the directory (empty/grid/list).
    refresh_view_mode(app, tab, true);
    rebuild_breadcrumb(app, tab, file);
    if let Some(page) = tab.page.borrow().as_ref() {
        let title =
            file.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
        page.set_title(&title);
    }
    super::places::sync_sidebar(app);
    super::update_chrome(app);
}

/// Show the right page for a tab's directory: the "empty" placeholder when the
/// folder has finished loading with no visible items, otherwise the grid or list
/// per the current view mode. With `force` false this leaves a "This PC" view
/// alone, so background updates don't yank the user out of it.
pub(super) fn refresh_view_mode(app: &Rc<super::App>, tab: &Rc<super::Tab>, force: bool) {
    // "This PC" and "Trash" are modes of their own — background updates (a
    // listing settling, an item count changing) must not yank the user out.
    if tab.search.active.get() {
        tab.view_stack.set_visible_child_name("search");
        return;
    }
    let showing = tab.view_stack.visible_child_name();
    if !force && matches!(showing.as_deref(), Some("computer") | Some("trash")) {
        return;
    }
    let empty = !tab.dir_list.is_loading() && tab.selection.n_items() == 0;
    let name = if empty {
        "empty"
    } else if app.is_list.get() {
        "list"
    } else {
        "grid"
    };
    tab.view_stack.set_visible_child_name(name);
    super::apply_visible_zoom(app, tab);
}

/// The path bar names a folder. This PC has no path, so it is hidden there.
pub(super) fn set_path_visible(tab: &super::Tab, on: bool) {
    if let Some(parent) = tab.breadcrumb.parent() {
        parent.set_visible(on);
    }
    if let Some(sep) = tab.search_bar.next_sibling() {
        sep.set_visible(on);
    }
}

pub(super) fn refresh(tab: &Rc<super::Tab>) {
    if tab.search.active.get() { super::search::refresh(tab); return; }
    if let Some(file) = tab.dir_list.file() {
        tab.dir_list.set_file(gio::File::NONE);
        tab.dir_list.set_file(Some(&file));
    }
}

/// Select `names` in `tab` once the folder listing has them. `then` runs after
/// a successful selection, so a properties dialog can read that selection.
pub(super) fn select_names(tab: &Rc<super::Tab>, names: &[String], then: impl Fn() + 'static) {
    if names.is_empty() {
        return;
    }
    let names = Rc::new(names.to_vec());
    let then = Rc::new(RefCell::new(Some(then)));
    let done = Rc::new(Cell::new(false));
    let attempt = Rc::new({
        let tab = Rc::downgrade(tab);
        let names = names.clone();
        let then = then.clone();
        let done = done.clone();
        move || {
            if done.get() {
                return;
            }
            let Some(tab) = tab.upgrade() else { return; };
            let selection = &tab.selection;
            let mut found = Vec::new();
            for i in 0..selection.n_items() {
                let Some(obj) = selection.item(i) else { continue };
                let Ok(info) = obj.downcast::<gio::FileInfo>() else { continue };
                let name = info.name().to_string_lossy().into_owned();
                if names.iter().any(|wanted| wanted == &name) {
                    found.push(i);
                }
            }
            let loading = tab.dir_list.is_loading();
            if found.is_empty() && loading {
                return;
            }
            if loading && found.len() < names.len() {
                return;
            }
            for (n, pos) in found.iter().enumerate() {
                selection.select_item(*pos, n == 0);
            }
            done.set(true);
            let callback = then.borrow_mut().take();
            if !found.is_empty() && let Some(callback) = callback { callback(); }
        }
    });
    attempt();
    {
        let attempt = attempt.clone();
        tab.selection.connect_items_changed(move |_, _, _, _| attempt());
    }
    {
        let attempt = attempt.clone();
        tab.dir_list.connect_loading_notify(move |_| attempt());
    }
}

pub(super) fn rebuild_breadcrumb(app: &Rc<super::App>, tab: &Rc<super::Tab>, file: &gio::File) {
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
            let app = Rc::downgrade(app);
            let tab = Rc::downgrade(tab);
            let target = f.clone();
            button.connect_clicked(move |_| { if let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) { navigate(&app, &tab, target.clone()); } });
        }
        tab.breadcrumb.append(&button);
    }
}

#[cfg(test)]
mod tests {
    use super::super::Place;
    use gtk::prelude::*;

    #[test]
    fn navigation_returns_to_search_with_scope_and_query() {
        let root = gtk::gio::File::for_path("/home/user/Documents");
        let mut back = vec![Place::Search { root: root.clone(), query: "annual report".into() }];
        let mut fwd = Vec::new();
        let previous = super::retreat(&mut back, &mut fwd, Place::Folder(root.child("reports"))).unwrap();
        assert!(matches!(&previous, Place::Search { root: scope, query } if scope == &root && query == "annual report"));
        let next = super::advance(&mut back, &mut fwd, previous).unwrap();
        assert!(matches!(next, Place::Folder(_)));
        assert!(matches!(back.last(), Some(Place::Search { .. })));
    }

    #[test]
    fn back_from_a_folder_returns_to_this_pc() {
        let mut back = Vec::new();
        let mut fwd = Vec::new();
        // Open a folder while This PC is showing.
        super::push_visit(&mut back, &mut fwd, Place::Computer);
        let back_to = super::retreat(
            &mut back,
            &mut fwd,
            Place::Folder(gtk::gio::File::for_path("/tmp/notes")),
        );
        assert!(matches!(back_to, Some(Place::Computer)));
        assert!(matches!(fwd.last(), Some(Place::Folder(_))));
    }

    #[test]
    fn back_from_trash_returns_to_this_pc() {
        let mut back = Vec::new();
        let mut fwd = Vec::new();
        super::push_visit(&mut back, &mut fwd, Place::Computer);
        let back_to = super::retreat(&mut back, &mut fwd, Place::Trash);
        assert!(matches!(back_to, Some(Place::Computer)));
    }
}
