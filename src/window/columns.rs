//! Folder branches for the Columns view.

struct Branch<T> {
    entries: Vec<(u64, T)>,
    focused: usize,
    serial: u64,
}

impl<T> Branch<T> {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            focused: 0,
            serial: 0,
        }
    }
    fn push(&mut self, value: T) -> u64 {
        self.serial += 1;
        self.entries.push((self.serial, value));
        self.serial
    }
    fn truncate(&mut self, len: usize) {
        self.entries.truncate(len);
        self.focused = self.focused.min(len.saturating_sub(1));
    }
    fn focus(&mut self, index: usize) {
        if index < self.entries.len() {
            self.focused = index;
        }
    }
    fn active(&self) -> Option<&T> {
        self.entries.get(self.focused).map(|(_, value)| value)
    }
    fn accepts(&self, index: usize, id: u64) -> bool {
        self.entries
            .get(index)
            .is_some_and(|(current, _)| *current == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_selection_replaces_only_descendants() {
        let mut branch = Branch::new();
        let root = branch.push("Documents");
        let projects = branch.push("Projects");
        branch.push("Client A");
        branch.push("Invoices");
        branch.truncate(2);
        branch.push("Client B");
        assert_eq!(
            branch
                .entries
                .iter()
                .map(|(_, name)| *name)
                .collect::<Vec<_>>(),
            ["Documents", "Projects", "Client B"]
        );
        assert!(branch.accepts(0, root) && branch.accepts(1, projects));
    }

    #[test]
    fn column_actions_use_focused_selection() {
        let mut branch = Branch::new();
        branch.push(vec!["parent-selected.txt"]);
        branch.push(vec!["child-selected.txt", "another.txt"]);
        branch.focus(1);
        assert_eq!(
            branch.active().unwrap(),
            &["child-selected.txt", "another.txt"]
        );
        branch.focus(0);
        assert_eq!(branch.active().unwrap(), &["parent-selected.txt"]);
    }

    #[test]
    fn stale_column_load_cannot_replace_new_branch() {
        let mut branch = Branch::new();
        branch.push("Documents");
        let old = branch.push("Client A");
        branch.truncate(1);
        let current = branch.push("Client B");
        assert!(!branch.accepts(1, old));
        assert!(branch.accepts(1, current));
    }
}

use super::*;

struct Column {
    file: gio::File,
    source: gtk::DirectoryList,
    contents: Rc<directory::Contents>,
    filter: gtk::CustomFilter,
    selection: gtk::MultiSelection,
    view: gtk::ColumnView,
    body: gtk::Box,
    splitter: gtk::Paned,
    message: gtk::Label,
    width: Cell<i32>,
    zoom: Rc<Cell<i32>>,
    restoring: RefCell<Option<Vec<std::path::PathBuf>>>,
}

impl Column {
    fn select_names(&self, names: &[std::path::PathBuf]) {
        self.selection.unselect_all();
        for pos in 0..self.selection.n_items() {
            if self
                .selection
                .item(pos)
                .and_downcast::<gio::FileInfo>()
                .is_some_and(|info| names.contains(&info.name()))
            {
                self.selection.select_item(pos, false);
            }
        }
    }
}

impl Drop for Column {
    fn drop(&mut self) {
        self.contents.stop();
    }
}

pub(super) struct Pane {
    pub widget: gtk::ScrolledWindow,
    branch: RefCell<Branch<Rc<Column>>>,
    changing: Cell<bool>,
}

impl Pane {
    pub fn new() -> Self {
        Self {
            widget: gtk::ScrolledWindow::builder()
                .vexpand(true)
                .hexpand(true)
                .vscrollbar_policy(gtk::PolicyType::Never)
                .build(),
            branch: RefCell::new(Branch::new()),
            changing: Cell::new(false),
        }
    }

    fn active(&self) -> Option<Rc<Column>> {
        self.branch.borrow().active().cloned()
    }
    pub fn selection(&self) -> Option<gtk::MultiSelection> {
        self.active().map(|c| c.selection.clone())
    }
    pub fn file(&self) -> Option<gio::File> {
        self.active().map(|c| c.file.clone())
    }
    pub fn restoring(&self) -> bool {
        self.active()
            .is_some_and(|c| c.restoring.borrow().is_some())
    }
    pub fn source(&self) -> Option<gtk::DirectoryList> {
        self.active().map(|c| c.source.clone())
    }
    pub fn view(&self) -> Option<gtk::ColumnView> {
        self.active().map(|c| c.view.clone())
    }

    pub fn clear(&self) {
        self.changing.set(true);
        self.widget.set_child(gtk::Widget::NONE);
        self.branch.borrow_mut().truncate(0);
        self.changing.set(false);
    }

    pub fn set_location(&self, app: &Rc<App>, tab: &Rc<Tab>, file: &gio::File) {
        self.changing.set(true);
        let existing = self
            .branch
            .borrow()
            .entries
            .iter()
            .position(|(_, c)| c.file.equal(file));
        if let Some(index) = existing {
            self.branch.borrow_mut().truncate(index + 1);
        } else {
            let parent = file.parent().and_then(|parent| {
                self.branch
                    .borrow()
                    .entries
                    .iter()
                    .position(|(_, c)| c.file.equal(&parent))
            });
            if let Some(index) = parent {
                self.branch.borrow_mut().truncate(index + 1);
                self.append(app, tab, file.clone());
            } else {
                self.branch.borrow_mut().truncate(0);
                // Start with the current folder and up to two ancestors.
                let mut chain = vec![file.clone()];
                while chain.len() < 3 {
                    let Some(parent) = chain.last().and_then(|f| f.parent()) else {
                        break;
                    };
                    chain.push(parent);
                }
                for dir in chain.into_iter().rev() {
                    self.append(app, tab, dir);
                }
            }
        }
        let last = self.branch.borrow().entries.len().saturating_sub(1);
        self.branch.borrow_mut().focus(last);
        let ancestors: Vec<_> = self
            .branch
            .borrow()
            .entries
            .windows(2)
            .filter_map(|pair| {
                pair[1]
                    .1
                    .file
                    .basename()
                    .map(|name| (pair[0].1.clone(), name))
            })
            .collect();
        for (column, name) in ancestors {
            let names = vec![name];
            if column.source.is_loading() || column.restoring.borrow().is_some() {
                // The new path supersedes any selection saved by an earlier refresh.
                *column.restoring.borrow_mut() = Some(names);
            } else {
                column.select_names(&names);
            }
        }
        self.rebuild();
        self.changing.set(false);
        self.reveal(last);
    }

    fn append(&self, app: &Rc<App>, tab: &Rc<Tab>, file: gio::File) {
        let source = gtk::DirectoryList::builder()
            .attributes(ATTRS)
            .monitored(false)
            .build();
        let contents = directory::Contents::new(&source);
        let hidden = app.show_hidden.clone();
        let filter = gtk::CustomFilter::new(move |object| {
            let info = object.downcast_ref::<gio::FileInfo>().unwrap();
            hidden.get() || !(info.is_hidden() || info.name().to_string_lossy().starts_with('.'))
        });
        let filtered =
            gtk::FilterListModel::new(Some(contents.items.clone()), Some(filter.clone()));
        let sorted = gtk::SortListModel::new(Some(filtered), tab.folder_model.sorter());
        let selection = gtk::MultiSelection::new(Some(sorted));
        let view = gtk::ColumnView::builder()
            .focusable(true)
            .model(&selection)
            .enable_rubberband(true)
            .build();
        let zoom = Rc::new(Cell::new(app.zoom.get()));
        let name = view::name_column(Some(tab), &view, &zoom);
        name.set_title(Some(
            &file
                .basename()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/".into()),
        ));
        // The shared sort action controls all columns; this header names the folder.
        name.set_sorter(gtk::Sorter::NONE);
        name.set_resizable(false);
        view.append_column(&name);
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&view)
            .build();
        let message = gtk::Label::builder()
            .label("Loading…")
            .wrap(true)
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        message.add_css_class("dim-label");
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_size_request(160, -1);
        body.append(&message);
        body.append(&scroll);
        let splitter = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&body)
            .resize_start_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .wide_handle(true)
            .position(260)
            .build();
        let column = Rc::new(Column {
            file: file.clone(),
            source: source.clone(),
            contents,
            filter,
            selection: selection.clone(),
            view: view.clone(),
            body,
            splitter: splitter.clone(),
            message,
            width: Cell::new(260),
            zoom,
            restoring: RefCell::new(None),
        });
        let weak = Rc::downgrade(&column);
        splitter.connect_position_notify(move |pane| {
            if let Some(column) = weak.upgrade() {
                column.width.set(pane.position().max(160));
            }
        });
        let index = self.branch.borrow().entries.len();
        let id = self.branch.borrow_mut().push(column.clone());
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        selection.connect_selection_changed(move |_, _, _| {
            if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                if tab.columns.live(&tab, index, id) {
                    update_chrome(&app);
                    update_preview(&tab);
                    let a = Rc::downgrade(&app);
                    let t = Rc::downgrade(&tab);
                    // Let GTK finish moving keyboard focus before reparenting columns.
                    glib::idle_add_local_once(move || {
                        if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                            if tab.columns.live(&tab, index, id) {
                                tab.columns.selected(&app, &tab, index);
                            }
                        }
                    });
                }
            }
        });
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        selection.connect_items_changed(move |_, _, _, _| {
            if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                if tab.columns.live(&tab, index, id) {
                    tab.columns.update_message(index);
                    // Defer until GTK has finished updating its selection.
                    let a = Rc::downgrade(&app);
                    let t = Rc::downgrade(&tab);
                    glib::idle_add_local_once(move || {
                        if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                            if tab.columns.live(&tab, index, id) {
                                tab.columns.selected(&app, &tab, index);
                            }
                        }
                    });
                }
            }
        });
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        source.connect_loading_notify(move |source| {
            if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                if !tab.columns.changing.get() && tab.columns.branch.borrow().accepts(index, id) {
                    tab.columns.update_message(index);
                    if !source.is_loading() {
                        tab.columns.loaded(&app, &tab, index);
                    }
                }
            }
        });
        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        click.connect_pressed(move |_, _, _, _| {
            if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                if tab.columns.live(&tab, index, id) {
                    tab.columns.focus(&app, &tab, index, true);
                }
            }
        });
        column.body.add_controller(click);
        let focus = gtk::EventControllerFocus::new();
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        focus.connect_enter(move |_| {
            if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                if tab.columns.live(&tab, index, id) {
                    tab.columns.focus(&app, &tab, index, true);
                }
            }
        });
        view.add_controller(focus);
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        view.connect_activate(move |_, pos| {
            if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
                if !tab.columns.live(&tab, index, id) {
                    return;
                }
                tab.columns.focus(&app, &tab, index, true);
                commands::activate(&app, &tab, pos);
            }
        });
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        key.connect_key_pressed(move |controller, key, _, modifiers| {
            if modifiers.intersects(
                gtk::gdk::ModifierType::CONTROL_MASK
                    | gtk::gdk::ModifierType::SHIFT_MASK
                    | gtk::gdk::ModifierType::ALT_MASK
                    | gtk::gdk::ModifierType::SUPER_MASK,
            ) || controller
                .widget()
                .is_some_and(|w| view::focus_is_text_input(&w))
            {
                return glib::Propagation::Proceed;
            }
            let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) else {
                return glib::Propagation::Proceed;
            };
            if !tab.columns.live(&tab, index, id) {
                return glib::Propagation::Proceed;
            }
            let next = match key {
                gtk::gdk::Key::Left => index.checked_sub(1),
                gtk::gdk::Key::Right => Some(index + 1),
                _ => return glib::Propagation::Proceed,
            };
            if let Some(next) = next {
                tab.columns.focus(&app, &tab, next, true);
                let column = tab
                    .columns
                    .branch
                    .borrow()
                    .entries
                    .get(next)
                    .map(|(_, c)| c.clone());
                if let Some(col) = column {
                    col.view.grab_focus();
                    tab.columns.reveal(next);
                }
            }
            glib::Propagation::Stop
        });
        view.add_controller(key);
        view::attach_view_keys(app, &view);
        view::attach_context_menu(app, &view);
        view::attach_drop_target(app, tab, &view);
        source.set_file(Some(&file));
    }

    fn live(&self, tab: &Tab, index: usize, id: u64) -> bool {
        !self.changing.get()
            && tab.view_stack.visible_child_name().as_deref() == Some("columns")
            && self.branch.borrow().accepts(index, id)
    }

    fn loaded(&self, app: &Rc<App>, tab: &Rc<Tab>, index: usize) {
        let Some(column) = self
            .branch
            .borrow()
            .entries
            .get(index)
            .map(|(_, c)| c.clone())
        else {
            return;
        };
        let next = self
            .branch
            .borrow()
            .entries
            .get(index + 1)
            .map(|(_, c)| c.file.clone());
        let restore = column.restoring.borrow_mut().take();
        let names = restore.or_else(|| next.and_then(|f| f.basename()).map(|name| vec![name]));
        if let Some(names) = names {
            self.changing.set(true);
            column.select_names(&names);
            self.changing.set(false);
        }
        if tab.view_stack.visible_child_name().as_deref() == Some("columns") {
            self.selected(app, tab, index);
        }
    }

    fn update_message(&self, index: usize) {
        let Some(column) = self
            .branch
            .borrow()
            .entries
            .get(index)
            .map(|(_, c)| c.clone())
        else {
            return;
        };
        let text = if let Some(error) = column.source.error() {
            format!("Couldn’t open folder: {error}")
        } else if column.source.is_loading() {
            "Loading…".into()
        } else if column.selection.n_items() == 0 {
            "This Folder Is Empty".into()
        } else {
            String::new()
        };
        column.message.set_label(&text);
        column.message.set_visible(!text.is_empty());
    }

    fn focus(&self, app: &Rc<App>, tab: &Rc<Tab>, index: usize, remember: bool) {
        let Some(column) = self
            .branch
            .borrow()
            .entries
            .get(index)
            .map(|(_, c)| c.clone())
        else {
            return;
        };
        if self.branch.borrow().focused != index {
            if remember && let Some(place) = navigation::here(tab) {
                tab.back.borrow_mut().push(place);
                tab.fwd.borrow_mut().clear();
            }
            self.branch.borrow_mut().focus(index);
            tab.begin_navigation();
        }
        if !tab.dir_list.file().is_some_and(|f| f.equal(&column.file)) {
            tab.dir_list.set_file(Some(&column.file));
        }
        navigation::rebuild_breadcrumb(app, tab, &column.file);
        if let Some(page) = tab.page.borrow().as_ref() {
            page.set_title(
                &column
                    .file
                    .basename()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".into()),
            );
        }
        for (i, (_, c)) in self.branch.borrow().entries.iter().enumerate() {
            if i == index {
                c.body.add_css_class("fs-column-focused");
            } else {
                c.body.remove_css_class("fs-column-focused");
            }
        }
        places::sync_sidebar(app);
        update_chrome(app);
        update_preview(tab);
    }

    fn selected(&self, app: &Rc<App>, tab: &Rc<Tab>, index: usize) {
        // Background changes in an ancestor must not steal command focus.
        if self.branch.borrow().focused != index {
            return;
        }
        let Some(column) = self.active() else {
            return;
        };
        if column.source.is_loading() || column.restoring.borrow().is_some() {
            return;
        }
        let bits = column.selection.selection();
        let info = (bits.size() == 1)
            .then(|| {
                column
                    .selection
                    .item(bits.minimum())
                    .and_downcast::<gio::FileInfo>()
            })
            .flatten();
        let child = info
            .as_ref()
            .filter(|i| i.file_type() == gio::FileType::Directory)
            .map(|i| column.file.child(i.name()));
        let existing = self
            .branch
            .borrow()
            .entries
            .get(index + 1)
            .map(|(_, c)| c.file.clone());
        if child.as_ref().map(|f| f.uri()) != existing.as_ref().map(|f| f.uri()) {
            self.changing.set(true);
            self.branch.borrow_mut().truncate(index + 1);
            if let Some(child) = child {
                self.append(app, tab, child);
            }
            self.rebuild();
            self.changing.set(false);
            // Keep the selection's column focused so Down continues among siblings.
            self.reveal(index);
        }
        if info.is_some_and(|i| i.file_type() != gio::FileType::Directory) {
            tab.preview_pane.set_visible(true);
        }
        update_chrome(app);
        update_preview(tab);
    }

    fn rebuild(&self) {
        let columns: Vec<_> = self
            .branch
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        // Keep retained views parented: focus, scroll positions and context menus survive.
        for (index, column) in columns.iter().enumerate().rev() {
            if let Some(next) = columns.get(index + 1) {
                if column.splitter.end_child().as_ref() != Some(next.splitter.upcast_ref()) {
                    column.splitter.set_end_child(Some(&next.splitter));
                }
            } else if column
                .splitter
                .end_child()
                .is_none_or(|child| child.is::<gtk::Paned>())
            {
                column
                    .splitter
                    .set_end_child(Some(&gtk::Box::new(gtk::Orientation::Vertical, 0)));
            }
            if index == self.branch.borrow().focused {
                column.body.add_css_class("fs-column-focused");
            } else {
                column.body.remove_css_class("fs-column-focused");
            }
        }
        if let Some(first) = columns.first() {
            first.splitter.set_size_request(
                columns
                    .iter()
                    .map(|c| c.width.get() + 6)
                    .sum::<i32>()
                    .max(1),
                -1,
            );
            if self.widget.child().as_ref() != Some(first.splitter.upcast_ref()) {
                self.widget.set_child(Some(&first.splitter));
            }
        } else {
            self.widget.set_child(gtk::Widget::NONE);
        }
    }

    fn reveal(&self, index: usize) {
        let start: i32 = self
            .branch
            .borrow()
            .entries
            .iter()
            .take(index)
            .map(|(_, c)| c.width.get() + 6)
            .sum();
        let width = self
            .branch
            .borrow()
            .entries
            .get(index)
            .map(|(_, c)| c.width.get())
            .unwrap_or(260);
        let adjustment = self.widget.hadjustment();
        glib::idle_add_local_once(move || {
            let left = start as f64;
            let right = (start + width) as f64;
            if left < adjustment.value() {
                adjustment.set_value(left);
            } else if right > adjustment.value() + adjustment.page_size() {
                adjustment.set_value((right - adjustment.page_size()).max(0.0));
            }
        });
    }

    pub fn refresh(&self) {
        let columns: Vec<_> = self
            .branch
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        self.changing.set(true);
        for column in columns {
            let bits = column.selection.selection();
            let names = (0..bits.size())
                .filter_map(|i| {
                    column
                        .selection
                        .item(bits.nth(i as u32))
                        .and_downcast::<gio::FileInfo>()
                        .map(|info| info.name())
                })
                .collect();
            *column.restoring.borrow_mut() = Some(names);
            column.source.set_file(gio::File::NONE);
            column.source.set_file(Some(&column.file));
        }
        self.changing.set(false);
    }

    pub fn filter_changed(&self) {
        let columns: Vec<_> = self
            .branch
            .borrow()
            .entries
            .iter()
            .map(|(_, c)| c.clone())
            .collect();
        for column in columns {
            column.filter.changed(gtk::FilterChange::Different);
        }
    }

    pub fn set_zoom(&self, zoom: i32) {
        for (_, column) in &self.branch.borrow().entries {
            if column.zoom.replace(zoom) != zoom {
                view::resize_names(column.view.upcast_ref(), view::list_icon(zoom));
            }
        }
    }
}

pub(super) fn focus_widget(app: &Rc<App>, tab: &Rc<Tab>, widget: &gtk::Widget) {
    if tab.view_stack.visible_child_name().as_deref() != Some("columns") {
        return;
    }
    let index = tab
        .columns
        .branch
        .borrow()
        .entries
        .iter()
        .position(|(_, c)| c.view.upcast_ref::<gtk::Widget>() == widget);
    if let Some(index) = index {
        tab.columns.focus(app, tab, index, true);
    }
}

#[cfg(test)]
mod gtk_tests {
    use super::*;
    use std::time::Duration;

    async fn settle() {
        glib::timeout_future(Duration::from_millis(180)).await;
    }
    async fn loaded(column: &Column) {
        for _ in 0..200 {
            if !column.source.is_loading() {
                settle().await;
                return;
            }
            glib::timeout_future(Duration::from_millis(10)).await;
        }
        panic!("folder did not finish loading: {}", column.file.uri());
    }
    fn select(column: &Column, name: &str) {
        let pos = (0..column.selection.n_items())
            .find(|&pos| {
                column
                    .selection
                    .item(pos)
                    .and_downcast::<gio::FileInfo>()
                    .is_some_and(|i| i.name() == std::path::Path::new(name))
            })
            .unwrap_or_else(|| panic!("missing {name} in {}", column.file.uri()));
        column.selection.select_item(pos, true);
    }
    fn column(tab: &Tab, index: usize) -> Rc<Column> {
        tab.columns.branch.borrow().entries[index].1.clone()
    }

    fn editor(widget: &gtk::Widget) -> Option<gtk::Entry> {
        if let Some(stack) = widget.downcast_ref::<gtk::Stack>() {
            if stack.visible_child_name().as_deref() == Some("edit") {
                return stack.child_by_name("edit").and_downcast::<gtk::Entry>();
            }
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(entry) = editor(&widget) {
                return Some(entry);
            }
            child = widget.next_sibling();
        }
        None
    }

    fn review_fixture() -> (adw::Application, std::path::PathBuf) {
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let application = adw::Application::builder()
            .application_id("dev.filescope.ColumnsRegression")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!(
            "filescope-columns-regression-{}",
            std::process::id()
        ));
        for name in ["A", "B", "Empty"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("A/one"), "one").unwrap();
        std::fs::write(root.join("A/two"), "two").unwrap();
        (application, root)
    }

    fn assert_column_has_keyboard_focus(app: &App, column: &Column) {
        let actual = gtk::prelude::RootExt::focus(&app.window).expect("keyboard focus");
        assert!(
            actual == column.view.clone().upcast::<gtk::Widget>()
                || actual.is_ancestor(&column.view),
            "keyboard focus is in {:?}, not the command column {}",
            actual.type_(),
            column.file.uri()
        );
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn columns_navigation_keeps_keyboard_and_command_focus_together() {
        let (application, root) = review_fixture();
        let file = gio::File::for_path(&root);
        glib::MainContext::default().block_on(async {
            super::super::open_files(&application, &[file.clone()]);
            let app = super::super::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            super::super::set_view_mode(&app, ViewMode::Columns);
            let parent = column(&tab, 2);
            loaded(&parent).await;
            parent.view.grab_focus();
            select(&parent, "A");
            settle().await;
            let child = column(&tab, 3);
            loaded(&child).await;
            child.view.grab_focus();
            select(&child, "one");
            settle().await;
            parent.view.grab_focus();
            assert_column_has_keyboard_focus(&app, &parent);
            assert!(commands::selected(&tab)[0].0.equal(&file.child("A")));
            commands::open_selected(&app);
            settle().await;
            assert_column_has_keyboard_focus(&app, &child);
            assert!(commands::selected(&tab)[0].0.equal(&file.child("A/one")));
            // Activation (Enter/double click) must obey the same focus contract.
            parent.view.grab_focus();
            parent
                .view
                .emit_by_name::<()>("activate", &[&parent.selection.selection().minimum()]);
            settle().await;
            assert_column_has_keyboard_focus(&app, &child);
            navigation::go_up(&app, &tab);
            settle().await;
            assert_column_has_keyboard_focus(&app, &parent);
            navigation::go_back(&app, &tab);
            settle().await;
            assert_column_has_keyboard_focus(&app, &tab.columns.active().unwrap());
            navigation::go_forward(&app, &tab);
            settle().await;
            assert_column_has_keyboard_focus(&app, &parent);
            navigation::navigate(&app, &tab, file.child("Empty"));
            loaded(&tab.columns.active().unwrap()).await;
            assert_column_has_keyboard_focus(&app, &tab.columns.active().unwrap());
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn columns_background_navigation_does_not_steal_focus() {
        let (application, root) = review_fixture();
        let file = gio::File::for_path(&root);
        glib::MainContext::default().block_on(async {
            super::super::open_files(&application, &[file.clone()]);
            let app = super::super::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            super::super::set_view_mode(&app, ViewMode::Columns);
            loaded(&tab.columns.active().unwrap()).await;
            let foreground = tab.columns.active().unwrap();
            foreground.view.grab_focus();
            navigation::new_tab(&app, file.child("A"), false);
            let background = app.tabs.borrow().last().unwrap().clone();
            loaded(&background.columns.active().unwrap()).await;
            assert_column_has_keyboard_focus(&app, &foreground);
            navigation::navigate(&app, &background, file.child("B"));
            loaded(&background.columns.active().unwrap()).await;
            assert_column_has_keyboard_focus(&app, &foreground);
            // A user can focus search before the active directory finishes loading.
            navigation::navigate(&app, &tab, file.child("A"));
            tab.search_bar.set_search_mode(true);
            tab.search_entry.grab_focus();
            loaded(&tab.columns.active().unwrap()).await;
            assert!(view::focus_is_text_input(tab.search_entry.upcast_ref()));
            assert!(Rc::ptr_eq(&navigation::active_tab(&app), &tab));
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn columns_view_switch_preserves_an_empty_selection() {
        let (application, root) = review_fixture();
        glib::MainContext::default().block_on(async {
            super::super::open_files(&application, &[gio::File::for_path(root.join("A"))]);
            let app = super::super::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            for mode in [ViewMode::List, ViewMode::Grid] {
                super::super::set_view_mode(&app, mode);
                settle().await;
                navigation::select_names(&tab, &["one".into()], || {});
                assert_eq!(commands::selected(&tab).len(), 1);
                super::super::set_view_mode(&app, ViewMode::Columns);
                loaded(&tab.columns.active().unwrap()).await;
                tab.columns.selection().unwrap().unselect_all();
                assert!(commands::selected(&tab).is_empty());
                super::super::set_view_mode(&app, mode);
                settle().await;
                assert!(
                    commands::selected(&tab).is_empty(),
                    "cleared Columns selection reappeared in {mode:?}"
                );
            }
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn columns_navigation_updates_retained_ancestor_selection() {
        let (application, root) = review_fixture();
        let file = gio::File::for_path(&root);
        glib::MainContext::default().block_on(async {
            super::super::open_files(&application, &[file.clone()]);
            let app = super::super::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            super::super::set_view_mode(&app, ViewMode::Columns);
            let parent = column(&tab, 2);
            loaded(&parent).await;
            parent.view.grab_focus();
            select(&parent, "A");
            settle().await;
            navigation::navigate(&app, &tab, file.child("B"));
            loaded(&tab.columns.active().unwrap()).await;
            let selected_name = || {
                let bits = parent.selection.selection();
                assert_eq!(bits.size(), 1);
                parent
                    .selection
                    .item(bits.minimum())
                    .and_downcast::<gio::FileInfo>()
                    .unwrap()
                    .name()
            };
            assert_eq!(selected_name(), std::path::Path::new("B"));
            // A pending reload must not later restore the old branch highlight.
            navigation::refresh(&tab);
            navigation::navigate(&app, &tab, file.child("A"));
            loaded(&parent).await;
            assert_eq!(selected_name(), std::path::Path::new("A"));
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn columns_widget_navigation_actions_and_cleanup() {
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let application = adw::Application::builder()
            .application_id("dev.filescope.ColumnsTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root =
            std::env::temp_dir().join(format!("filescope-columns-ui-{}", std::process::id()));
        std::fs::create_dir_all(root.join("A/deep/leaf")).unwrap();
        std::fs::create_dir_all(root.join("B")).unwrap();
        std::fs::write(root.join("A/child.txt"), "child").unwrap();
        std::fs::write(root.join("parent.txt"), "parent").unwrap();
        let file = gio::File::for_path(&root);
        glib::MainContext::default().block_on(async {
            super::super::open_files(&application, &[file.clone()]);
            let app = super::super::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            settle().await;
            super::super::set_view_mode(&app, ViewMode::Columns);
            assert_eq!(
                tab.view_stack.visible_child_name().as_deref(),
                Some("columns")
            );
            assert_eq!(tab.columns.branch.borrow().entries.len(), 3);
            let parent = column(&tab, 2);
            loaded(&parent).await;
            let ancestor_id = tab.columns.branch.borrow().entries[1].0;
            let parent_splitter = parent.splitter.clone();
            parent.splitter.set_position(300);
            select(&parent, "A");
            settle().await;
            assert_eq!(parent.splitter, parent_splitter);
            assert!(parent.width.get() >= 290);
            assert_eq!(tab.columns.branch.borrow().entries.len(), 4);
            assert!(commands::selected(&tab)[0].0.equal(&file.child("A")));
            let child = column(&tab, 3);
            loaded(&child).await;
            tab.columns.focus(&app, &tab, 3, true);
            select(&child, "child.txt");
            settle().await;
            assert_eq!(
                commands::selected(&tab)
                    .iter()
                    .map(|(f, _)| f.uri())
                    .collect::<Vec<_>>(),
                [file.child("A/child.txt").uri()]
            );
            assert!(tab.current_dir().unwrap().equal(&file.child("A")));
            assert_eq!(
                navigation::here(&tab).as_ref().and_then(terminal_cwd),
                Some(root.join("A"))
            );
            assert!(tab.preview_pane.widget.is_visible());
            commands::set_clipboard(&app, false);
            let copied = Rc::new(RefCell::new(None));
            let result = copied.clone();
            commands::read_clipboard_files(&app.window.clipboard(), move |cut, paths| {
                assert!(!cut);
                *result.borrow_mut() = Some(paths);
            });
            settle().await;
            assert_eq!(*copied.borrow(), Some(vec![root.join("A/child.txt")]));
            assert!(app.window.lookup_action("rename").unwrap().is_enabled());
            navigation::refresh(&tab);
            loaded(&child).await;
            assert!(
                commands::selected(&tab)[0]
                    .0
                    .equal(&file.child("A/child.txt"))
            );
            assert_eq!(tab.columns.branch.borrow().entries.len(), 4);
            commands::rename_selected(&app);
            settle().await;
            let entry =
                editor(tab.columns.widget.upcast_ref()).expect("column inline rename editor");
            entry.set_text("renamed.txt");
            entry.emit_activate();
            settle().await;
            assert!(root.join("A/renamed.txt").exists());
            assert!(root.join("parent.txt").exists());
            commands::undo(&app);
            settle().await;
            assert!(root.join("A/child.txt").exists());
            commands::new_folder(&app);
            settle().await;
            let entry = editor(tab.columns.widget.upcast_ref()).expect("new folder name editor");
            entry.set_text("Created Here");
            entry.emit_activate();
            settle().await;
            assert!(root.join("A/Created Here").is_dir());
            assert!(!root.join("Created Here").exists());
            // Delete acts only on the focused selection, even with an ancestor selected.
            select(&child, "Created Here");
            commands::delete_selected(&app);
            settle().await;
            let dialog = app
                .window
                .visible_dialog()
                .and_downcast::<adw::AlertDialog>()
                .unwrap();
            dialog.emit_by_name::<()>("response", &[&"delete"]);
            dialog.close();
            settle().await;
            assert!(!root.join("A/Created Here").exists());
            assert!(root.join("A/child.txt").exists());
            assert!(root.join("parent.txt").exists());
            tab.columns.focus(&app, &tab, 2, true);
            assert!(commands::selected(&tab)[0].0.equal(&file.child("A")));
            select(&parent, "B");
            view::popup_context_menu(
                parent.view.upcast_ref(),
                &super::super::context_menu_for(&app, true),
                50.0,
                50.0,
            );
            settle().await;
            let menu = unsafe { parent.view.data::<gtk::PopoverMenu>("fs-context-menu") }
                .expect("context menu survives branch replacement");
            assert!(unsafe { menu.as_ref() }.is_visible());
            unsafe { menu.as_ref() }.popdown();
            assert_eq!(tab.columns.branch.borrow().entries.len(), 4);
            assert!(column(&tab, 3).file.equal(&file.child("B")));
            assert_eq!(tab.columns.branch.borrow().entries[1].0, ancestor_id);
            // A callback from the discarded column must not alter B or command targets.
            child.selection.unselect_all();
            select(&child, "deep");
            settle().await;
            assert!(column(&tab, 3).file.equal(&file.child("B")));
            assert!(
                commands::selected(&tab)[0].0.equal(&file.child("B")),
                "focus={} selected={:?}",
                tab.columns.branch.borrow().focused,
                commands::selected(&tab)
                    .iter()
                    .map(|(f, _)| f.uri())
                    .collect::<Vec<_>>()
            );
            select(&parent, "A");
            settle().await;
            tab.columns.focus(&app, &tab, 3, true);
            let child = column(&tab, 3);
            loaded(&child).await;
            select(&child, "child.txt");
            super::super::set_view_mode(&app, ViewMode::List);
            settle().await;
            assert!(tab.current_dir().unwrap().equal(&file.child("A")));
            assert!(
                commands::selected(&tab)[0]
                    .0
                    .equal(&file.child("A/child.txt"))
            );
            super::super::set_view_mode(&app, ViewMode::Columns);
            settle().await;
            assert!(tab.current_dir().unwrap().equal(&file.child("A")));
            assert!(
                commands::selected(&tab)[0]
                    .0
                    .equal(&file.child("A/child.txt"))
            );
            navigation::navigate(&app, &tab, file.child("A/deep"));
            navigation::navigate(&app, &tab, file.child("A/deep/leaf"));
            assert!(tab.columns.branch.borrow().entries.len() > 3);
            navigation::go_back(&app, &tab);
            assert!(tab.current_dir().unwrap().equal(&file.child("A/deep")));
            navigation::go_forward(&app, &tab);
            assert!(tab.current_dir().unwrap().equal(&file.child("A/deep/leaf")));
            navigation::go_up(&app, &tab);
            assert!(tab.current_dir().unwrap().equal(&file.child("A/deep")));
            navigation::navigate(&app, &tab, file.child("missing"));
            let failed = tab.columns.active().unwrap();
            loaded(&failed).await;
            assert!(failed.source.error().is_some());
            assert!(failed.message.text().contains("Couldn’t open"));
            navigation::go_up(&app, &tab);
            assert!(tab.current_dir().unwrap().equal(&file));
            let denied_path = root.join("denied");
            std::fs::create_dir(&denied_path).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&denied_path, std::fs::Permissions::from_mode(0o000)).unwrap();
            navigation::navigate(&app, &tab, file.child("denied"));
            let denied = tab.columns.active().unwrap();
            loaded(&denied).await;
            assert!(denied.source.error().is_some());
            navigation::go_up(&app, &tab);
            std::fs::set_permissions(&denied_path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let large_path = root.join("large");
            std::fs::create_dir(&large_path).unwrap();
            for i in 0..2000 {
                std::fs::write(large_path.join(format!("item-{i:04}")), "").unwrap();
            }
            navigation::navigate(&app, &tab, file.child("large"));
            let large = tab.columns.active().unwrap();
            loaded(&large).await;
            assert_eq!(large.selection.n_items(), 2000);
            select(&large, "item-1999");
            let selected_model = super::super::visible_selection(&tab);
            tab.columns.set_zoom(128);
            assert_eq!(selected_model, super::super::visible_selection(&tab));
            assert!(
                commands::selected(&tab)[0]
                    .0
                    .equal(&file.child("large/item-1999"))
            );
            app.sort_desc.set(true);
            super::super::apply_sort_to(&app, &tab);
            assert!(
                commands::selected(&tab)[0]
                    .0
                    .equal(&file.child("large/item-1999"))
            );
            // Cancel a fresh enumeration by replacing its branch immediately.
            navigation::refresh(&tab);
            navigation::navigate(&app, &tab, file.child("B"));
            settle().await;
            assert!(tab.current_dir().unwrap().equal(&file.child("B")));
            assert_eq!(super::super::visible_selection(&tab).n_items(), 0);
            std::fs::remove_dir(root.join("B")).unwrap();
            navigation::refresh(&tab);
            let removed = tab.columns.active().unwrap();
            loaded(&removed).await;
            assert!(removed.source.error().is_some());
            navigation::go_up(&app, &tab);
            assert!(tab.current_dir().unwrap().equal(&file));
            let weak = Rc::downgrade(&tab.columns.active().unwrap());
            tab.shutdown();
            assert!(weak.upgrade().is_none());
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }
}
