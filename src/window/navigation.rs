// Tabs and folder navigation: history, breadcrumbs, and refreshing a listing.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

// --- Tabs -------------------------------------------------------------------

/// Create a tab showing `dir`, optionally selecting it.
pub(super) fn new_tab(app: &Rc<super::App>, dir: gio::File, select: bool) {
    let dir_list =
        gtk::DirectoryList::builder().attributes(super::ATTRS).monitored(false).io_priority(glib::Priority::DEFAULT).build();
    let directory = super::directory::Contents::new(&dir_list);

    let show_hidden = app.show_hidden.clone();
    let filter = gtk::CustomFilter::new(move |obj| {
        let info = obj.downcast_ref::<gio::FileInfo>().unwrap();
        show_hidden.get() || !(info.is_hidden() || info.name().to_string_lossy().starts_with('.'))
    });
    let filtered = gtk::FilterListModel::new(Some(directory.items.clone()), Some(filter.clone()));
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
    let browser = preview_pane.split.clone();
    view_stack.set_hexpand(true);
    view_stack.set_size_request(160, -1);
    browser.set_start_child(Some(&view_stack));
    let terminal_pane = super::terminal::Pane::new();
    terminal_pane.widget().set_start_child(Some(&browser));
    content.append(&terminal_pane.widget());

    let tab = Rc::new(super::Tab {
        page: RefCell::new(None),
        dir_list,
        directory,
        filter,
        selection: selection.clone(),
        folder_model: sort_model.clone(),
        columns: super::columns::Pane::new(),
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
        navigation_generation: Cell::new(0),
    });

    tab.view_stack.add_named(&tab.columns.widget, Some("columns"));

    // Register the tab before appending: appending the first page auto-selects
    // it, which fires `selected-page-notify` → `active_tab()`; the tab must
    // already be in `app.tabs` (its page is filled in a moment later).
    app.tabs.borrow_mut().push(tab.clone());
    let page = app.tab_view.append(&content);
    page.set_title("Home");
    *tab.page.borrow_mut() = Some(page.clone());

    super::search::connect(app, &tab);
    super::terminal::connect(app, &tab);

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
    super::view::watch_metadata(&tab.directory.items, &grid_view);
    super::view::watch_metadata(&tab.directory.items, &column_view);
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
        _ => tab.current_dir().map(super::Place::Folder),
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
    if let Some(parent) = tab.current_dir().and_then(|f| f.parent()) {
        navigate(app, tab, parent);
    }
}

/// Point `tab` at `file`, leaving any "This PC" view, and refresh chrome.
pub(super) fn set_dir(app: &Rc<super::App>, tab: &Rc<super::Tab>, file: &gio::File) {
    tab.begin_navigation();
    // A new folder starts unfiltered — close any open search from the last one.
    tab.search_bar.set_search_mode(false);
    tab.dir_list.set_file(Some(file));
    protect_batch_directory(app, &tab.directory, true);
    if app.view_mode.get() == super::ViewMode::Columns {
        tab.columns.set_location(app, tab, file);
    } else {
        tab.columns.clear();
    }
    set_path_visible(tab, true);
    // Force out of any "This PC" view into the directory (empty/grid/list).
    refresh_view_mode(app, tab, true);
    // Only the displayed tab may take focus; never defer this until a load completes.
    if tab.columns.widget.is_mapped() && let Some(view) = tab.columns.view() {
        view.grab_focus();
    }
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
    if app.view_mode.get() == super::ViewMode::Columns {
        if tab.columns.file().is_none() && let Some(file) = tab.dir_list.file() {
            tab.columns.set_location(app, tab, &file);
        }
        tab.view_stack.set_visible_child_name("columns");
        super::apply_visible_zoom(app, tab);
        return;
    }
    let empty = !tab.dir_list.is_loading() && tab.selection.n_items() == 0;
    let name = if empty {
        "empty"
    } else if app.view_mode.get() == super::ViewMode::List {
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
    if tab.view_stack.visible_child_name().as_deref() == Some("columns") { tab.columns.refresh(); return; }
    tab.directory.refresh();
}

pub(super) fn renamed_location(file: &gio::File, from: &gio::File, to: &gio::File) -> Option<gio::File> {
    if file.equal(from) { Some(to.clone()) }
    else { from.relative_path(file).map(|relative| to.resolve_relative_path(relative)) }
}

pub(super) fn mapped_location(file: &gio::File, pairs: &[(gio::File, gio::File)]) -> Option<gio::File> {
    pairs.iter().find_map(|(from, to)| renamed_location(file, from, to))
}

pub(super) fn batch_busy(tab: &super::Tab) -> bool {
    if tab.view_stack.visible_child_name().as_deref() == Some("columns") { tab.columns.held() }
    else { tab.directory.held() }
}

/// Pending scopes also protect views opened while their worker is running.
pub(super) struct RenameScope {
    holds: RefCell<Vec<super::directory::RenameHold>>,
    aliases: RefCell<HashMap<(usize, glib::GString), (gio::File, gio::File, bool)>>,
    requests: Vec<(gio::File, gio::File)>,
    identities: Vec<super::directory::EntryIdentity>,
    finishing: Cell<bool>,
}
impl RenameScope {
    fn protect(&self, contents: &Rc<super::directory::Contents>, search: bool, new_choice: bool) {
        if self.finishing.get() { return; }
        let Some(file) = contents.file() else { return; };
        let canonical = file.path().and_then(|p| p.canonicalize().ok()).map(gio::File::for_path).unwrap_or_else(|| file.clone());
        BatchRename::aliases_for(&file, &canonical, &self.requests, &mut self.aliases.borrow_mut());
        let related = |location: &gio::File| self.requests.iter().any(|(from, to)| {
            from.parent().is_some_and(|p| p.equal(location)) || to.parent().is_some_and(|p| p.equal(location))
                || location.equal(from) || location.has_prefix(from) || (search && from.has_prefix(location))
        });
        let affected = related(&file) || related(&canonical) || self.aliases.borrow().values().any(|(from, _, _)| {
            file.equal(from) || file.has_prefix(from) || from.parent().is_some_and(|p| p.equal(&file))
        });
        if !affected { return; }
        let mut holds = self.holds.borrow_mut();
        if holds.iter().any(|hold| hold.applies_to(contents) && (!new_choice || hold.same_directory(&file))) { return; }
        // A same-path navigation may now refer to a replacement directory.
        holds.retain(|hold| !hold.applies_to(contents));
        let mut entries: Vec<_> = self.requests.iter().map(|(from, _)| from.clone()).collect();
        entries.extend(self.aliases.borrow().values().filter(|(_, _, row)| *row).map(|(from, _, _)| from.clone()));
        entries.retain(|entry| entry.parent().is_some_and(|parent| parent.equal(&file)));
        holds.push(contents.hold_renames(&entries));
    }
}

pub(super) fn protect_batch_directory(app: &Rc<super::App>, contents: &Rc<super::directory::Contents>, new_choice: bool) {
    let scopes: Vec<_> = {
        let mut pending = app.pending_renames.borrow_mut();
        pending.retain(|scope| scope.strong_count() > 0);
        pending.iter().filter_map(std::rc::Weak::upgrade).collect()
    };
    for scope in scopes { scope.protect(contents, false, new_choice); }
}

pub(super) struct BatchRename { scope: Rc<RenameScope> }
impl BatchRename {
    pub fn new(app: &Rc<super::App>, requests: &[crate::batch_rename::Rename]) -> Self {
        let scope = Rc::new(RenameScope {
            requests: requests.iter().map(|r| (gio::File::for_path(&r.from), gio::File::for_path(&r.to))).collect(),
            identities: requests.iter().map(crate::batch_rename::Rename::entry_identity).collect(),
            holds: RefCell::new(Vec::new()), aliases: RefCell::new(HashMap::new()), finishing: Cell::new(false),
        });
        app.pending_renames.borrow_mut().retain(|scope| scope.strong_count() > 0);
        app.pending_renames.borrow_mut().push(Rc::downgrade(&scope));
        let tabs: Vec<_> = app.tabs.borrow().iter().cloned().collect();
        for tab in tabs {
            scope.protect(&tab.directory, tab.search.active.get(), false);
            for contents in tab.columns.directories() { scope.protect(&contents, false, false); }
            for history in [&tab.back, &tab.fwd] {
                for place in history.borrow().iter() {
                    if let super::Place::Folder(file) | super::Place::Search { root: file, .. } = place {
                        let canonical = file.path().and_then(|p| p.canonicalize().ok()).map(gio::File::for_path).unwrap_or_else(|| file.clone());
                        Self::aliases_for(file, &canonical, &scope.requests, &mut scope.aliases.borrow_mut());
                    }
                }
            }
        }
        super::update_chrome(app);
        Self { scope }
    }

    fn aliases_for(file: &gio::File, canonical: &gio::File, pairs: &[(gio::File, gio::File)], aliases: &mut HashMap<(usize, glib::GString), (gio::File, gio::File, bool)>) {
        let mut entries = Vec::new();
        let mut cursor = Some(file.clone());
        while let Some(entry) = cursor {
            cursor = entry.parent();
            if let Some(parent) = cursor.as_ref().and_then(|p| p.path()).and_then(|p| p.canonicalize().ok()).map(gio::File::for_path) {
                entries.push((entry, parent));
            }
        }
        for (index, (from, to)) in pairs.iter().enumerate() {
            // Resolve a directory entry's parent, not the symlink entry itself.
            let lexical = entries.iter().find(|(entry, parent)| entry.basename() == from.basename()
                && from.parent().is_some_and(|p| p.equal(parent))).map(|(entry, _)| entry.clone());
            let alias_from = if lexical.is_some() { lexical }
            else if from.parent().is_some_and(|p| p.equal(canonical)) {
                from.basename().map(|name| file.child(name))
            } else if canonical.equal(from) { Some(file.clone()) }
            else if let Some(relative) = from.relative_path(canonical) {
                let mut parent = Some(file.clone());
                for _ in relative.components() { parent = parent.and_then(|p| p.parent()); }
                parent
            } else { None };
            let Some(alias_from) = alias_from else { continue; };
            let row_mapping = alias_from.basename() == from.basename() && alias_from.parent()
                .and_then(|p| p.path()).and_then(|p| p.canonicalize().ok()).map(gio::File::for_path)
                .is_some_and(|parent| from.parent().is_some_and(|p| p.equal(&parent)));
            let alias_to = if row_mapping {
                alias_from.parent().zip(to.basename()).map(|(parent, name)| parent.child(name)).unwrap_or_else(|| to.clone())
            } else { to.clone() };
            // A link to the folder is a browsing alias, not a renamed directory entry.
            if !alias_from.equal(from) || !alias_to.equal(to) {
                aliases.entry((index, alias_from.uri())).or_insert((alias_from, alias_to, row_mapping));
            }
        }
    }

    pub fn finish(self, app: &Rc<super::App>, outcome: Option<&crate::batch_rename::Outcome>) {
        self.scope.finishing.set(true);
        let completed: HashSet<_> = outcome.into_iter().flat_map(|o| &o.undo)
            .map(|inverse| (gio::File::for_path(&inverse.to).uri(), gio::File::for_path(&inverse.from).uri())).collect();
        let successful: HashSet<_> = self.scope.requests.iter().enumerate()
            .filter(|(_, (from, to))| completed.contains(&(from.uri(), to.uri()))).map(|(index, _)| index).collect();
        let mut pairs: Vec<_> = self.scope.requests.iter().enumerate().filter(|(index, _)| successful.contains(index)).map(|(_, pair)| pair.clone()).collect();
        let aliases: Vec<_> = self.scope.aliases.borrow().iter().filter(|((index, _), _)| successful.contains(index)).map(|((index, _), alias)| (*index, alias.clone())).collect();
        let mut rows = pairs.clone();
        let mut identities: HashMap<_, _> = self.scope.requests.iter().enumerate().filter(|(index, _)| successful.contains(index))
            .map(|(index, (from, _))| (from.uri(), self.scope.identities[index])).collect();
        for (index, (from, to, row)) in &aliases {
            pairs.push((from.clone(), to.clone()));
            if *row {
                rows.push((from.clone(), to.clone()));
                identities.insert(from.uri(), self.scope.identities[*index]);
            }
        }
        apply_renames(app, &pairs, &rows, Some(&identities), |contents| self.scope.holds.borrow().iter().any(|hold| {
            hold.applies_to(contents) && contents.file().is_some_and(|file| {
                mapped_location(&file, &pairs).is_none_or(|destination| hold.same_directory(&destination))
            })
        }));
        drop(self);
        super::update_chrome(app);
    }
}

/// The filesystem rename already succeeded. Keep open paths and item identities in sync.
pub(super) fn renamed(app: &Rc<super::App>, origin: &Rc<super::Tab>, from: &std::path::Path, to: &std::path::Path) {
    let from = gio::File::for_path(from); let to = gio::File::for_path(to);
    let pairs = [(from, to.clone())];
    apply_renames(app, &pairs, &pairs, None, |_| true);
    if !origin.search.active.get() && origin.current_dir().is_some_and(|dir| to.parent().is_some_and(|parent| dir.equal(&parent))) {
        if let Some(name) = to.basename() {
            let app = app.clone(); let tab = origin.clone();
            select_names(origin, &[name.to_string_lossy().into_owned()], move || focus_selection(&app, &tab));
        }
    }
    super::update_chrome(app);
}

fn apply_renames(app: &Rc<super::App>, pairs: &[(gio::File, gio::File)], rows: &[(gio::File, gio::File)], identities: Option<&HashMap<glib::GString, super::directory::EntryIdentity>>, allowed: impl Fn(&Rc<super::directory::Contents>) -> bool) {
    if pairs.is_empty() { return; }
    let tabs: Vec<_> = app.tabs.borrow().iter().cloned().collect();
    for tab in tabs {
        let in_scope = |root: &gio::File| pairs.iter().any(|(from, to)| {
            from.equal(root) || from.has_prefix(root) || to.equal(root) || to.has_prefix(root) || root.has_prefix(from)
        });
        let refresh_search = tab.search.active.get() && (
            tab.dir_list.file().is_some_and(|root| in_scope(&root)
                || root.path().and_then(|path| path.canonicalize().ok()).is_some_and(|path| in_scope(&gio::File::for_path(path))))
            || (0..tab.search.model.n_items()).filter_map(|i| tab.search.model.item(i).and_downcast::<gio::FileInfo>())
                .filter_map(|info| super::commands::file_from_info(&info, None)).any(|file| pairs.iter().any(|(from, _)| file.equal(from) || file.has_prefix(from)))
        );
        let own_directory = allowed(&tab.directory);
        if own_directory { tab.directory.renamed(rows, identities); }
        let column_location = tab.columns.renamed(pairs, rows, identities, &allowed);
        let location = if tab.view_stack.visible_child_name().as_deref() == Some("columns") { column_location }
            else if own_directory { tab.dir_list.file().and_then(|file| mapped_location(&file, pairs)) } else { None };
        if let Some(file) = location {
            tab.begin_navigation();
            tab.directory.relocate(&file);
            rebuild_breadcrumb(app, &tab, &file);
            if let Some(page) = tab.page.borrow().as_ref() {
                page.set_title(&file.basename().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into()));
            }
        }
        if refresh_search { super::search::refresh(&tab); }
        for history in [&tab.back, &tab.fwd] {
            for place in history.borrow_mut().iter_mut() {
                match place {
                    super::Place::Folder(file) | super::Place::Search { root: file, .. } => {
                        if let Some(new) = mapped_location(file, pairs) { *file = new; }
                    }
                    _ => {}
                }
            }
        }
        super::update_preview(&tab);
    }
    super::places::sync_sidebar(app);
    super::update_chrome(app);
}

pub(super) fn focus_selection(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    if !Rc::ptr_eq(&active_tab(app), tab) { return; }
    let selection = super::visible_selection(tab).selection();
    if selection.is_empty() { return; }
    let position = selection.minimum();
    let flags = gtk::ListScrollFlags::FOCUS;
    match tab.view_stack.visible_child_name().as_deref() {
        Some("columns") => { if let Some(view) = tab.columns.view() { view.scroll_to(position, None, flags, None); } }
        Some("list") => tab.column_view.scroll_to(position, None, flags, None),
        Some("grid") => tab.grid_view.scroll_to(position, flags, None),
        _ => {}
    }
}

pub(super) fn selected_uris(model: &gtk::MultiSelection) -> std::collections::HashSet<glib::GString> {
    let bits = model.selection();
    (0..bits.size()).filter_map(|i| model.item(bits.nth(i as u32)).and_downcast::<gio::FileInfo>())
        .filter_map(|i| i.attribute_object("standard::file").and_downcast::<gio::File>().map(|f| f.uri())).collect()
}
fn model_uris(model: &gtk::MultiSelection) -> std::collections::HashSet<glib::GString> {
    (0..model.n_items()).filter_map(|i| model.item(i).and_downcast::<gio::FileInfo>())
        .filter_map(|i| i.attribute_object("standard::file").and_downcast::<gio::File>().map(|f| f.uri())).collect()
}

/// A deletion may select a neighbour only while the original view/user choice is unchanged.
pub(super) struct RemovalSelection {
    tab: std::rc::Weak<super::Tab>,
    model: gtk::MultiSelection,
    directory: gio::File,
    generation: u64,
    removed: std::collections::HashSet<glib::GString>,
    neighbour: Option<glib::GString>,
    position: u32,
    cancelled: Rc<Cell<bool>>,
    handler: Option<glib::SignalHandlerId>,
}
impl RemovalSelection {
    pub fn capture(app: &Rc<super::App>, paths: &[std::path::PathBuf]) -> Option<Self> {
        let tab = active_tab(app);
        let directory = tab.current_dir()?;
        let model = super::visible_selection(&tab);
        let original = selected_uris(&model);
        let removed: std::collections::HashSet<_> = paths.iter().map(|p| gio::File::for_path(p).uri()).collect();
        let affected: std::collections::HashSet<_> = original.intersection(&removed).cloned().collect();
        if affected.is_empty() { return None; }
        let position = model.selection().minimum();
        let neighbours: Vec<_> = (0..model.n_items()).filter_map(|pos| {
            let uri = model.item(pos).and_downcast::<gio::FileInfo>()?.attribute_object("standard::file")
                .and_downcast::<gio::File>()?.uri();
            (!removed.contains(&uri)).then_some((pos, uri))
        }).collect();
        let neighbour = neighbours.iter().find(|(pos, _)| *pos >= position).or_else(|| neighbours.last()).map(|(_, uri)| uri.clone());
        let cancelled = Rc::new(Cell::new(false));
        let changed = cancelled.clone();
        let handler = model.connect_selection_changed(move |model, _, _| {
            let current = selected_uris(model);
            let available = model_uris(model);
            if !current.is_subset(&original)
                || original.difference(&current).any(|uri| available.contains(uri)) {
                changed.set(true);
            }
        });
        Some(Self { tab: Rc::downgrade(&tab), model, directory, generation: tab.navigation_generation.get(),
            removed: affected, neighbour, position, cancelled, handler: Some(handler) })
    }

    pub fn finish(self, app: &Rc<super::App>) {
        let app = Rc::downgrade(app);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        glib::timeout_add_local(std::time::Duration::from_millis(20), move || {
            let (Some(app), Some(tab)) = (app.upgrade(), self.tab.upgrade()) else { return glib::ControlFlow::Break; };
            if self.cancelled.get() || tab.navigation_generation.get() != self.generation
                || !Rc::ptr_eq(&active_tab(&app), &tab) || super::visible_selection(&tab) != self.model
                || !tab.current_dir().is_some_and(|f| f.equal(&self.directory))
                || super::view::focus_is_text_input(app.window.upcast_ref()) {
                return glib::ControlFlow::Break;
            }
            let available = model_uris(&self.model);
            if self.removed.iter().any(|uri| available.contains(uri)) {
                return if std::time::Instant::now() < deadline { glib::ControlFlow::Continue } else { glib::ControlFlow::Break };
            }
            if !self.model.selection().is_empty() || self.model.n_items() == 0 { return glib::ControlFlow::Break; }
            let position = self.neighbour.as_ref().and_then(|uri| (0..self.model.n_items()).find(|&pos| {
                self.model.item(pos).and_downcast::<gio::FileInfo>().and_then(|i| i.attribute_object("standard::file").and_downcast::<gio::File>())
                    .is_some_and(|f| f.uri() == *uri)
            })).unwrap_or(self.position.min(self.model.n_items() - 1));
            self.model.select_item(position, true);
            focus_selection(&app, &tab);
            glib::ControlFlow::Break
        });
    }
}
impl Drop for RemovalSelection {
    fn drop(&mut self) { if let Some(handler) = self.handler.take() { self.model.disconnect(handler); } }
}

/// Select `names` in `tab` once the folder listing has them. `then` runs after
/// a successful selection, so a properties dialog can read that selection.
pub(super) fn select_names(tab: &Rc<super::Tab>, names: &[String], then: impl Fn() + 'static) {
    select_names_if(tab, names, || true, then);
}

/// Operations can additionally cancel when their originating tab/input is no longer current.
pub(super) fn select_names_if(tab: &Rc<super::Tab>, names: &[String], still_current: impl Fn() -> bool + 'static, then: impl Fn() + 'static) {
    if names.is_empty() {
        return;
    }
    let names = Rc::new(names.to_vec());
    let then = Rc::new(RefCell::new(Some(then)));
    let done = Rc::new(Cell::new(false));
    let generation = tab.navigation_generation.get();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let directory = tab.current_dir().map(|file| file.uri());
    let model = super::visible_selection(tab);
    let original = selected_uris(&model);
    let model = model.downgrade();
    let attempt = Rc::new({
        let tab = Rc::downgrade(tab);
        let names = names.clone();
        let then = then.clone();
        let done = done.clone();
        move || {
            if done.get() {
                return;
            }
            let stop = || { done.set(true); then.borrow_mut().take(); };
            let Some(tab) = tab.upgrade() else { stop(); return; };
            let Some(model) = model.upgrade() else { stop(); return; };
            let selection = super::visible_selection(&tab);
            if tab.navigation_generation.get() != generation
                || tab.current_dir().map(|file| file.uri()) != directory
                || selection != model || !still_current() {
                stop();
                return;
            }
            let current = selected_uris(&selection);
            if current != original {
                let available = model_uris(&selection);
                if !current.is_subset(&original) || original.difference(&current).any(|uri| available.contains(uri)) {
                    stop();
                    return;
                }
            }
            let expired = std::time::Instant::now() >= deadline;
            if tab.view_stack.visible_child_name().as_deref() == Some("columns") && tab.columns.restoring() {
                if expired { stop(); }
                return;
            }
            let mut found = Vec::new();
            for i in 0..selection.n_items() {
                let Some(obj) = selection.item(i) else { continue };
                let Ok(info) = obj.downcast::<gio::FileInfo>() else { continue };
                let name = info.name().to_string_lossy().into_owned();
                if names.iter().any(|wanted| wanted == &name) {
                    found.push(i);
                }
            }
            if found.len() < names.len() && !expired { return; }
            for (n, pos) in found.iter().enumerate() {
                selection.select_item(*pos, n == 0);
            }
            done.set(true);
            let callback = then.borrow_mut().take();
            if !found.is_empty() && let Some(callback) = callback { callback(); }
        }
    });
    attempt();
    if !done.get() {
        glib::timeout_add_local(std::time::Duration::from_millis(10), move || {
            attempt();
            if done.get() { glib::ControlFlow::Break } else { glib::ControlFlow::Continue }
        });
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
