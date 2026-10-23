// Ranked, paged search results shared by Ctrl+F and GNOME's "more" action.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct Pane {
    pub widget: gtk::Box,
    pub model: gio::ListStore,
    pub active: Cell<bool>,
    pub query: RefCell<String>,
    view: gtk::ListView,
    message: gtk::Label,
    more: gtk::Button,
    generation: Cell<u64>,
    next: Cell<Option<usize>>,
    loading: Cell<bool>,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
    metadata_cancel: RefCell<Option<gio::Cancellable>>,
}

impl Pane {
    pub fn new(selection: &gtk::MultiSelection) -> Self {
        let view = gtk::ListView::builder()
            .model(selection)
            .enable_rubberband(true)
            .build();
        view.add_css_class("fs-search-results");
        let scroller = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&view)
            .build();
        let message = gtk::Label::builder()
            .xalign(0.0)
            .margin_start(16)
            .margin_end(16)
            .margin_top(8)
            .margin_bottom(8)
            .wrap(true)
            .build();
        message.add_css_class("dim-label");
        let more = gtk::Button::builder()
            .label("Load more results")
            .halign(gtk::Align::Center)
            .margin_top(8)
            .margin_bottom(8)
            .visible(false)
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&message);
        widget.append(&scroller);
        widget.append(&more);
        Self {
            widget,
            view,
            message,
            more,
            model: gio::ListStore::new::<gio::FileInfo>(),
            active: Cell::new(false),
            query: RefCell::new(String::new()),
            generation: Cell::new(0),
            next: Cell::new(None),
            loading: Cell::new(false),
            cancel: RefCell::new(None),
            metadata_cancel: RefCell::new(None),
        }
    }

    pub fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(cancel) = self.cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(cancel) = self.metadata_cancel.borrow_mut().take() {
            cancel.cancel();
        }
        self.loading.set(false);
    }
}

pub(super) fn connect(app: &Rc<App>, tab: &Rc<Tab>) {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let row = gtk::Box::builder()
            .spacing(12)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let icon = gtk::Image::builder().pixel_size(32).build();
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let parent = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        parent.add_css_class("dim-label");
        parent.add_css_class("caption");
        text.set_hexpand(true);
        text.append(&name);
        text.append(&parent);
        row.append(&icon);
        row.append(&text);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let row = item.child().and_downcast::<gtk::Box>().unwrap();
        let icon = row.first_child().and_downcast::<gtk::Image>().unwrap();
        if let Some(gicon) = info.icon() {
            icon.set_from_gicon(&gicon);
        } else {
            icon.clear();
        }
        let text = row.last_child().and_downcast::<gtk::Box>().unwrap();
        text.first_child()
            .and_downcast::<gtk::Label>()
            .unwrap()
            .set_label(&info.display_name());
        let file = info
            .attribute_object("standard::file")
            .and_downcast::<gio::File>();
        let parent = file
            .as_ref()
            .and_then(|f| f.parent())
            .map(|p| p.parse_name().to_string())
            .unwrap_or_default();
        text.last_child()
            .and_downcast::<gtk::Label>()
            .unwrap()
            .set_label(&parent);
        row.set_tooltip_text(file.as_ref().map(|f| f.parse_name()).as_deref());
        unsafe {
            row.set_data("fs-position", item.position());
        }
    });
    tab.search.view.set_factory(Some(&factory));
    let a = Rc::downgrade(app);
    let t = Rc::downgrade(tab);
    tab.search.view.connect_activate(move |_, pos| {
        if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
            commands::activate(&app, &tab, pos);
        }
    });
    view::attach_context_menu(app, &tab.search.view);
    view::attach_view_keys(app, &tab.search.view);
    let a = Rc::downgrade(app);
    let t = Rc::downgrade(tab);
    tab.search_entry.connect_search_changed(move |_| {
        if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) {
            changed(&app, &tab, true);
        }
    });
    let a = Rc::downgrade(app);
    let t = Rc::downgrade(tab);
    tab.search_bar
        .connect_search_mode_enabled_notify(move |bar| {
            if !bar.is_search_mode()
                && let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade())
            {
                tab.search_entry.set_text("");
                stop(&app, &tab);
            }
        });
    let a = Rc::downgrade(app);
    let t = Rc::downgrade(tab);
    tab.search.more.connect_clicked(move |_| {
        if let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade())
            && let Some(offset) = tab.search.next.get()
        {
            load(&app, &tab, offset);
        }
    });
}

pub(super) fn stop(app: &Rc<App>, tab: &Rc<Tab>) {
    tab.search.cancel();
    tab.search.query.borrow_mut().clear();
    if tab.search.active.replace(false) {
        tab.begin_navigation();
        // Esc/clearing returns to the scope folder without a duplicate Back step.
        let root = tab.dir_list.file();
        if matches!(tab.back.borrow().last(), Some(Place::Folder(file)) if root.as_ref().is_some_and(|root| file.equal(root)))
        {
            tab.back.borrow_mut().pop();
        }
        tab.selection.set_model(Some(&tab.folder_model));
        tab.search.model.remove_all();
        navigation::refresh_view_mode(app, tab, true);
        if let (Some(page), Some(dir)) = (tab.page.borrow().as_ref(), tab.dir_list.file()) {
            page.set_title(
                &dir.basename()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "/".into()),
            );
        }
        update_chrome(app);
    }
}

pub(super) fn changed(app: &Rc<App>, tab: &Rc<Tab>, remember: bool) {
    let query = tab.search_entry.text().to_string();
    if !tab.search_bar.is_search_mode() || query.trim().is_empty() {
        stop(app, tab);
        return;
    }
    if tab.search.active.get() && *tab.search.query.borrow() == query {
        return;
    }
    let Some(root) = tab.dir_list.file().filter(|f| f.path().is_some()) else {
        return;
    };
    if remember && !tab.search.active.get() {
        tab.back.borrow_mut().push(Place::Folder(root));
        tab.fwd.borrow_mut().clear();
    }
    tab.begin_navigation();
    tab.search.cancel();
    tab.search.active.set(true);
    *tab.search.query.borrow_mut() = query.clone();
    tab.search.model.remove_all();
    tab.selection.set_model(Some(&tab.search.model));
    tab.search.next.set(None);
    tab.search.more.set_visible(false);
    tab.view_stack.set_visible_child_name("search");
    if let Some(page) = tab.page.borrow().as_ref() {
        page.set_title(&format!("Search: {query}"));
    }
    load(app, tab, 0);
}

fn load(app: &Rc<App>, tab: &Rc<Tab>, offset: usize) {
    if tab.search.loading.replace(true) {
        return;
    }
    let Some(root) = tab.dir_list.file().and_then(|f| f.path()) else {
        tab.search.loading.set(false);
        return;
    };
    let terms = tab
        .search
        .query
        .borrow()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let generation = tab.search.generation.get();
    let cancel = Arc::new(AtomicBool::new(false));
    *tab.search.cancel.borrow_mut() = Some(cancel.clone());
    let metadata_cancel = gio::Cancellable::new();
    *tab.search.metadata_cancel.borrow_mut() = Some(metadata_cancel.clone());
    tab.search.message.set_label("Searching…");
    tab.search.more.set_sensitive(false);
    update_chrome(app);
    let a = Rc::downgrade(app);
    let t = Rc::downgrade(tab);
    glib::spawn_future_local(async move {
        let page = crate::search_provider::search_page(root, terms, cancel.clone(), offset).await;
        let mut infos = Vec::new();
        for uri in page.ids {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let file = gio::File::for_uri(&uri);
            let info = gio::CancellableFuture::new(
                file.query_info_future(
                    ATTRS,
                    gio::FileQueryInfoFlags::NONE,
                    glib::Priority::DEFAULT,
                ),
                metadata_cancel.clone(),
            )
            .await;
            if let Ok(Ok(info)) = info {
                info.set_attribute_object("standard::file", &file);
                infos.push(info);
            }
        }
        let (Some(app), Some(tab)) = (a.upgrade(), t.upgrade()) else {
            return;
        };
        if cancel.load(Ordering::Relaxed)
            || generation != tab.search.generation.get()
            || !tab.search.active.get()
        {
            return;
        }
        // Index updates can shift page boundaries while the user loads more.
        let mut seen: std::collections::HashSet<_> = (0..tab.search.model.n_items())
            .filter_map(|i| tab.search.model.item(i).and_downcast::<gio::FileInfo>())
            .filter_map(|info| commands::file_from_info(&info, None).map(|f| f.uri()))
            .collect();
        infos.retain(|info| {
            commands::file_from_info(info, None).is_some_and(|f| seen.insert(f.uri()))
        });
        tab.search
            .model
            .splice(tab.search.model.n_items(), 0, &infos);
        tab.search.next.set(page.next_offset);
        tab.search.loading.set(false);
        tab.search.more.set_sensitive(true);
        tab.search.more.set_visible(page.next_offset.is_some());
        let total = tab.search.model.n_items();
        let message = if page.limited {
            format!("{total} results — limited search; the index is unavailable")
        } else if total == 0 && page.next_offset.is_none() {
            "No matches in this folder’s indexed locations".into()
        } else if page.next_offset.is_some() {
            format!("{total} results — more available")
        } else {
            format!("{total} results")
        };
        tab.search.message.set_label(&message);
        update_chrome(&app);
    });
}

pub(super) fn refresh(tab: &Tab) {
    tab.search.query.borrow_mut().clear();
    tab.search_entry.emit_by_name::<()>("search-changed", &[]);
}
