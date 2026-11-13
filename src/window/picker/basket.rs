//! Exact request-local identities, in insertion order, separate from browsing.
use super::*;
use std::collections::{BTreeMap, HashSet};

#[derive(Clone)]
struct Entry {
    uri: String,
    file: gio::File,
    info: gio::FileInfo,
}

pub(super) struct State {
    pub enabled: bool,
    pub widget: gtk::Box,
    count: gtk::Label,
    model: gio::ListStore,
    items: RefCell<Vec<Entry>>,
    ids: RefCell<HashSet<String>>,
    errors: RefCell<BTreeMap<String, String>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
    cancel: RefCell<Option<gio::Cancellable>>,
}

impl State {
    pub fn new(mode: &Mode) -> Self {
        let enabled = matches!(
            mode,
            Mode::Open {
                multiple: true,
                directory: false
            }
        );
        let model = gio::ListStore::new::<gio::FileInfo>();
        let list = gtk::ColumnView::builder()
            .model(&gtk::NoSelection::new(Some(model.clone())))
            .build();
        super::super::view::fill_columns(None, &list, &Rc::new(Cell::new(64)));
        for i in 0..list.columns().n_items() {
            if let Some(column) = list
                .columns()
                .item(i)
                .and_downcast::<gtk::ColumnViewColumn>()
            {
                column.set_sorter(None::<&gtk::Sorter>);
            }
        }
        let location = search::location_column();
        location.set_visible(true);
        list.append_column(&location);
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let button = gtk::Button::with_label("Remove");
            button.set_action_name(Some("basket.remove"));
            item.downcast_ref::<gtk::ListItem>()
                .unwrap()
                .set_child(Some(&button));
        });
        factory.connect_bind(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            let button = item.child().and_downcast::<gtk::Button>().unwrap();
            if let Some(file) = item
                .item()
                .and_downcast::<gio::FileInfo>()
                .and_then(|info| {
                    info.attribute_object("standard::file")
                        .and_downcast::<gio::File>()
                })
            {
                button.set_action_target_value(Some(&file.uri().to_variant()));
                button.set_tooltip_text(Some(&format!("Remove {}", file.parse_name())));
            }
        });
        list.append_column(&gtk::ColumnViewColumn::builder().factory(&factory).build());
        let count = gtk::Label::builder()
            .label("Selected files: 0")
            .xalign(0.0)
            .build();
        count.add_css_class("heading");
        let help = gtk::Label::builder().label("Files stay selected across folders, in the order added. Use Remove to exclude a file.").xalign(0.0).wrap(true).build();
        help.add_css_class("dim-label");
        let widget = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        widget.append(&count);
        widget.append(&help);
        widget.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .max_content_height(140)
                .propagate_natural_height(true)
                .build(),
        );
        Self {
            enabled,
            widget,
            count,
            model,
            items: RefCell::new(Vec::new()),
            ids: RefCell::new(HashSet::new()),
            errors: RefCell::new(BTreeMap::new()),
            generation: Cell::new(0),
            busy: Cell::new(false),
            cancel: RefCell::new(None),
        }
    }

    pub fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.busy.set(false);
        self.count
            .set_label(&format!("Selected files: {}", self.items.borrow().len()));
        if let Some(cancel) = self.cancel.borrow_mut().take() {
            cancel.cancel();
        }
    }

    pub fn close(&self) {
        self.cancel();
        self.items.borrow_mut().clear();
        self.ids.borrow_mut().clear();
        self.errors.borrow_mut().clear();
        self.model.remove_all();
    }
}

fn file(picker: &Picker, position: u32) -> Option<(gio::File, gio::FileInfo)> {
    let info = picker
        .selection
        .item(position)
        .and_downcast::<gio::FileInfo>()?;
    let file = info
        .attribute_object("standard::file")
        .and_downcast::<gio::File>()?;
    Some((file, info))
}

fn report(picker: &Picker) {
    picker.basket.count.set_label(&format!(
        "Selected files: {}",
        picker.basket.items.borrow().len()
    ));
    let errors = picker.basket.errors.borrow();
    if errors.is_empty() {
        picker.error.set_label("");
    } else {
        picker.error.set_label(&format!(
            "Some selected files cannot be opened. Remove them or choose a matching file type.\n{}",
            errors.values().cloned().collect::<Vec<_>>().join("\n")
        ));
    }
}

pub(super) fn selection_changed(picker: &Picker, position: u32, count: u32) {
    if !picker.basket.enabled || picker.complete.borrow().is_none() {
        return;
    }
    let mut added = false;
    for position in position
        ..position
            .saturating_add(count)
            .min(picker.selection.n_items())
    {
        if !picker.selection.is_selected(position) {
            continue;
        }
        let Some((file, info)) = file(picker, position) else {
            continue;
        };
        if info.file_type() == gio::FileType::Directory {
            continue;
        }
        let uri = file.uri().to_string();
        if !picker.basket.ids.borrow_mut().insert(uri.clone()) {
            continue;
        }
        // Directory refresh mutates its infos; a retained selection is a snapshot.
        let info = info.dup();
        picker.basket.items.borrow_mut().push(Entry {
            uri,
            file,
            info: info.clone(),
        });
        picker.basket.model.append(&info);
        added = true;
    }
    if added {
        picker.basket.cancel();
        report(picker);
    }
}

pub(super) fn filter_changed(picker: &Picker) {
    if !picker.basket.enabled {
        return;
    }
    picker.basket.cancel();
    picker.basket.errors.borrow_mut().clear();
    if let Some(spec) = picker
        .options
        .filters
        .get(picker.filters.selected() as usize)
    {
        let filter = file_filter(spec);
        for item in picker.basket.items.borrow().iter() {
            if !filter.match_(&item.info) {
                picker.basket.errors.borrow_mut().insert(
                    item.uri.clone(),
                    format!(
                        "{}: does not match the chosen file type",
                        item.file.parse_name()
                    ),
                );
            }
        }
    }
    report(picker);
}

fn remove(picker: &Picker, uri: &str) {
    let position = picker
        .basket
        .items
        .borrow()
        .iter()
        .position(|item| item.uri == uri);
    // A retained file can sit outside the visible folder, so Remove does not
    // emit the selection change that already cancels a pending grant.
    if position.is_some() {
        picker.acceptance.cancel();
    }
    picker.basket.cancel();
    if let Some(position) = position {
        picker.basket.items.borrow_mut().remove(position);
        picker.basket.ids.borrow_mut().remove(uri);
        picker.basket.errors.borrow_mut().remove(uri);
        picker.basket.model.remove(position as u32);
        for i in 0..picker.selection.n_items() {
            if file(picker, i).is_some_and(|(file, _)| file.uri() == uri) {
                picker.selection.unselect_item(i);
            }
        }
    }
    report(picker);
}

fn same_identity(previous: &gio::FileInfo, current: &gio::FileInfo) -> bool {
    previous.has_attribute("unix::inode")
        && current.has_attribute("unix::inode")
        && previous.attribute_uint64("unix::inode") == current.attribute_uint64("unix::inode")
        && previous.attribute_uint32("unix::device") == current.attribute_uint32("unix::device")
}

pub(super) fn accept(picker: &Rc<Picker>) {
    if picker.basket.busy.replace(true) {
        return;
    }
    let items = picker.basket.items.borrow().clone();
    if items.is_empty() {
        picker.basket.busy.set(false);
        picker.error.set_label("Select at least one file");
        return;
    }
    let generation = picker.basket.generation.get();
    picker
        .basket
        .count
        .set_label(&format!("Checking {} selected files…", items.len()));
    let cancel = gio::Cancellable::new();
    picker.basket.cancel.replace(Some(cancel.clone()));
    let filter = picker
        .options
        .filters
        .get(picker.filters.selected() as usize)
        .map(file_filter);
    let weak = Rc::downgrade(picker);
    glib::spawn_future_local(async move {
        let mut paths = Vec::new();
        let mut identities = Vec::new();
        let mut errors = BTreeMap::new();
        for item in items {
            let result = gio::CancellableFuture::new(
                item.file.query_info_future(
                    super::super::ATTRS,
                    gio::FileQueryInfoFlags::NONE,
                    glib::Priority::DEFAULT,
                ),
                cancel.clone(),
            )
            .await;
            if cancel.is_cancelled() {
                return;
            }
            let error = match result {
                Ok(Ok(info)) if info.file_type() != gio::FileType::Regular => {
                    Some("is no longer a regular file".to_string())
                }
                Ok(Ok(info)) if !same_identity(&item.info, &info) => {
                    Some("changed since it was selected; remove it and select it again".to_string())
                }
                Ok(Ok(info)) if filter.as_ref().is_some_and(|filter| !filter.match_(&info)) => {
                    Some("does not match the chosen file type".to_string())
                }
                Ok(Ok(_)) => match item.file.path() {
                    Some(path) => {
                        identities.push((
                            item.info.attribute_uint32("unix::device") as u64,
                            item.info.attribute_uint64("unix::inode"),
                        ));
                        paths.push(path);
                        None
                    }
                    None => Some("needs an accessible local file".into()),
                },
                Ok(Err(error)) => Some(error.to_string()),
                Err(_) => return,
            };
            if let Some(error) = error {
                errors.insert(item.uri, format!("{}: {error}", item.file.parse_name()));
            }
        }
        let Some(picker) = weak.upgrade() else {
            return;
        };
        if generation != picker.basket.generation.get() || picker.complete.borrow().is_none() {
            return;
        }
        picker.basket.busy.set(false);
        picker.basket.errors.replace(errors);
        if picker.basket.errors.borrow().is_empty() {
            picker.accepted_with(&paths, Some(identities));
        } else {
            report(&picker);
        }
    });
}

pub(super) fn connect(picker: &Rc<Picker>) {
    if !picker.basket.enabled {
        return;
    }
    let action = gio::SimpleAction::new("remove", Some(glib::VariantTy::STRING));
    let weak = Rc::downgrade(picker);
    action.connect_activate(move |_, value| {
        if let (Some(picker), Some(uri)) = (weak.upgrade(), value.and_then(|value| value.str())) {
            remove(&picker, uri);
        }
    });
    let group = gio::SimpleActionGroup::new();
    group.add_action(&action);
    picker
        .basket
        .widget
        .insert_action_group("basket", Some(&group));
}
