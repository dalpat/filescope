//! Request-owned paging/cancellation around Filescope's existing ranked search.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) struct State {
    pub widget: gtk::Box,
    pub entry: gtk::SearchEntry,
    pub location: gtk::ColumnViewColumn,
    pub active: Cell<bool>,
    model: gio::ListStore,
    message: gtk::Label,
    more: gtk::Button,
    query: RefCell<String>,
    previous: RefCell<Vec<gio::File>>,
    generation: Cell<u64>,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
    metadata_cancel: RefCell<Option<gio::Cancellable>>,
    loading: Cell<bool>,
    next: Cell<Option<usize>>,
    limited: Cell<bool>,
}

impl State {
    pub fn new() -> Self {
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Search this folder (Ctrl+F)")
            .hexpand(true)
            .build();
        entry.update_property(&[gtk::accessible::Property::Label("Search this folder")]);
        let message = gtk::Label::builder().xalign(0.0).wrap(true).build();
        message.add_css_class("dim-label");
        let more = gtk::Button::with_label("Load more results");
        more.set_visible(false);
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        widget.append(&entry);
        widget.append(&message);
        widget.append(&more);
        let location = location_column();
        Self {
            widget,
            entry,
            location,
            message,
            more,
            active: Cell::new(false),
            model: gio::ListStore::new::<gio::FileInfo>(),
            query: RefCell::new(String::new()),
            previous: RefCell::new(Vec::new()),
            generation: Cell::new(0),
            cancel: RefCell::new(None),
            metadata_cancel: RefCell::new(None),
            loading: Cell::new(false),
            next: Cell::new(None),
            limited: Cell::new(false),
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

pub(super) fn location_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        item.downcast_ref::<gtk::ListItem>()
            .unwrap()
            .set_child(Some(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            ));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let text = item
            .item()
            .and_downcast::<gio::FileInfo>()
            .and_then(|info| {
                info.attribute_object("standard::file")
                    .and_downcast::<gio::File>()
            })
            .and_then(|file| file.parent())
            .map(|file| file.parse_name())
            .unwrap_or_default();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        label.set_label(&text);
        label.set_tooltip_text(Some(&text));
    });
    gtk::ColumnViewColumn::builder()
        .title("Location")
        .factory(&factory)
        .resizable(true)
        .fixed_width(220)
        .visible(false)
        .build()
}

fn message(picker: &Picker) {
    if !picker.search.active.get() {
        return;
    }
    let scope = picker
        .source
        .file()
        .map(|file| file.parse_name())
        .unwrap_or_default();
    let coverage = if picker.search.limited.get() {
        " — limited search; index unavailable"
    } else if picker.search.next.get().is_some() {
        " — more indexed results available"
    } else {
        " — indexed locations"
    };
    picker.search.message.set_label(&format!(
        "{} results in {scope}{coverage}; hidden files excluded",
        picker.filtered.n_items()
    ));
}

pub(super) fn stop(picker: &Picker, restore: bool) {
    picker.search.cancel();
    let active = picker.search.active.replace(false);
    picker.search.query.borrow_mut().clear();
    if !picker.search.entry.text().is_empty() {
        picker.search.entry.set_text("");
    }
    if !active {
        return;
    }
    picker.selection.unselect_all();
    picker.filtered.set_model(Some(&picker.contents.items));
    if let Some(sorter) = picker.list.sorter() {
        picker
            .sorted
            .set_sorter(Some(&super::super::view::with_hidden_last(&sorter)));
    }
    picker.search.location.set_visible(false);
    picker.search.model.remove_all();
    picker.search.message.set_label("");
    picker.search.more.set_visible(false);
    let previous = picker.search.previous.take();
    if restore {
        for i in 0..picker.selection.n_items() {
            let file = picker
                .selection
                .item(i)
                .and_downcast::<gio::FileInfo>()
                .and_then(|info| {
                    info.attribute_object("standard::file")
                        .and_downcast::<gio::File>()
                });
            if file.is_some_and(|file| previous.iter().any(|previous| file.equal(previous))) {
                picker.selection.select_item(i, false);
            }
        }
    }
}


fn changed(picker: &Rc<Picker>) {
    if picker.location.unresolved.get() || !picker.search.entry.is_sensitive() {
        return;
    }
    let query = picker.search.entry.text().to_string();
    if query.trim().is_empty() {
        stop(picker, true);
        return;
    }
    if picker.search.active.get() && *picker.search.query.borrow() == query {
        return;
    }
    if !picker.search.active.get() {
        picker
            .search
            .previous
            .replace(picker.selected().iter().map(gio::File::for_path).collect());
    }
    picker.search.cancel();
    picker.search.active.set(true);
    picker.search.query.replace(query);
    picker.selection.unselect_all();
    picker.search.model.remove_all();
    picker.filtered.set_model(Some(&picker.search.model));
    picker.sorted.set_sorter(None::<&gtk::Sorter>);
    picker.search.location.set_visible(true);
    picker.search.next.set(None);
    picker.search.more.set_visible(false);
    load(picker, 0);
}

fn load(picker: &Rc<Picker>, offset: usize) {
    if picker.search.loading.replace(true) {
        return;
    }
    let Some(root) = picker.folder() else {
        picker.search.loading.set(false);
        return;
    };
    let terms = picker
        .search
        .query
        .borrow()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let generation = picker.search.generation.get();
    let cancel = Arc::new(AtomicBool::new(false));
    picker.search.cancel.replace(Some(cancel.clone()));
    let metadata_cancel = gio::Cancellable::new();
    picker
        .search
        .metadata_cancel
        .replace(Some(metadata_cancel.clone()));
    picker.search.message.set_label(&format!(
        "Searching {}… (hidden files excluded)",
        root.display()
    ));
    picker.search.more.set_sensitive(false);
    let weak = Rc::downgrade(picker);
    glib::spawn_future_local(async move {
        let page = crate::search_provider::search_page(root, terms, cancel.clone(), offset).await;
        let mut infos = Vec::new();
        for uri in page.ids {
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let file = gio::File::for_uri(&uri);
            if let Ok(Ok(info)) = gio::CancellableFuture::new(
                file.query_info_future(
                    super::super::ATTRS,
                    gio::FileQueryInfoFlags::NONE,
                    glib::Priority::DEFAULT,
                ),
                metadata_cancel.clone(),
            )
            .await
            {
                info.set_attribute_object("standard::file", &file);
                infos.push(info);
            }
        }
        let Some(picker) = weak.upgrade() else {
            return;
        };
        if cancel.load(Ordering::Relaxed)
            || generation != picker.search.generation.get()
            || !picker.search.active.get()
            || picker.complete.borrow().is_none()
        {
            return;
        }
        let mut seen: std::collections::HashSet<_> = (0..picker.search.model.n_items())
            .filter_map(|i| picker.search.model.item(i).and_downcast::<gio::FileInfo>())
            .filter_map(|info| {
                info.attribute_object("standard::file")
                    .and_downcast::<gio::File>()
            })
            .map(|file| file.uri())
            .collect();
        infos.retain(|info| {
            info.attribute_object("standard::file")
                .and_downcast::<gio::File>()
                .is_some_and(|file| seen.insert(file.uri()))
        });
        picker
            .search
            .model
            .splice(picker.search.model.n_items(), 0, &infos);
        picker.search.next.set(page.next_offset);
        picker.search.limited.set(page.limited);
        picker.search.loading.set(false);
        picker.search.more.set_sensitive(true);
        picker.search.more.set_visible(page.next_offset.is_some());
        message(&picker);
    });
}

pub(super) fn connect(picker: &Rc<Picker>) {
    let weak = Rc::downgrade(picker);
    picker.search.entry.connect_search_changed(move |_| {
        if let Some(picker) = weak.upgrade() {
            changed(&picker);
        }
    });
    let weak = Rc::downgrade(picker);
    picker.search.entry.connect_activate(move |_| {
        if let Some(picker) = weak.upgrade() {
            changed(&picker);
        }
    });
    let weak = Rc::downgrade(picker);
    picker.search.entry.connect_stop_search(move |_| {
        if let Some(picker) = weak.upgrade() {
            stop(&picker, true);
            picker.list.grab_focus();
        }
    });
    let weak = Rc::downgrade(picker);
    picker.search.more.connect_clicked(move |_| {
        if let Some(picker) = weak.upgrade()
            && let Some(offset) = picker.search.next.get()
        {
            load(&picker, offset);
        }
    });
    let weak = Rc::downgrade(picker);
    picker.filtered.connect_items_changed(move |_, _, _, _| {
        if let Some(picker) = weak.upgrade() {
            message(&picker);
        }
    });
}
