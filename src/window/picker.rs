//! An isolated chooser using the same directory loader and cells as Filescope.
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

#[derive(Clone, Debug)]
enum Mode { Open { multiple: bool, directory: bool }, Save, SaveFiles(Vec<OsString>) }

fn selection_paths(mode: &Mode, folder: &Path, selected: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let Mode::Open { multiple, directory } = mode else { return Err("This request needs a filename".into()); };
    let paths = if *directory && selected.is_empty() { vec![folder.to_path_buf()] } else { selected.to_vec() };
    if paths.is_empty() || (!multiple && paths.len() != 1) {
        return Err(if *multiple { "Select at least one item" } else { "Select one item" }.into());
    }
    if paths.iter().any(|path| !path.is_absolute() || if *directory { !path.is_dir() } else { !path.is_file() }) {
        return Err(if *directory { "Select folders only" } else { "Select files only" }.into());
    }
    Ok(paths)
}

fn save_paths(mode: &Mode, folder: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
    if !folder.is_absolute() || !folder.is_dir() { return Err("Choose an existing folder".into()); }
    let names = match mode {
        Mode::Save => vec![OsString::from(name)],
        Mode::SaveFiles(names) => names.clone(),
        _ => return Err("This request opens existing files".into()),
    };
    if names.is_empty() || names.iter().any(|name| !valid_name(name)) {
        return Err("Enter a filename without /, NUL, or a parent-directory path".into());
    }
    let paths: Vec<_> = names.iter().map(|name| folder.join(name)).collect();
    if paths.iter().any(|path| path.is_dir()) { return Err("A folder already has that name".into()); }
    Ok(paths)
}

fn valid_name(name: &OsStr) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.as_bytes().contains(&b'/') && !name.as_bytes().contains(&0)
}

type FilterSpec = (String, Vec<(u32, String)>);
type ChoiceSpec = (String, String, Vec<(String, String)>, String);

struct Options {
    mode: Mode,
    folder: PathBuf,
    name: String,
    raw_name: Option<OsString>,
    filters: Vec<FilterSpec>,
    current_filter: u32,
    choices: Vec<ChoiceSpec>,
    accept_label: String,
    modal: bool,
}

fn option<T: glib::variant::FromVariant>(dict: &glib::VariantDict, key: &str) -> Result<Option<T>, String> {
    dict.lookup(key).map_err(|_| format!("Invalid {key} option"))
}

fn byte_path(dict: &glib::VariantDict, key: &str) -> Result<Option<PathBuf>, String> {
    option::<Vec<u8>>(dict, key)?.map(|mut bytes| {
        if bytes.pop() != Some(0) || bytes.contains(&0) { return Err(format!("Invalid {key} path")); }
        let path = PathBuf::from(OsString::from_vec(bytes));
        if !path.is_absolute() { return Err(format!("{key} must be absolute")); }
        Ok(path)
    }).transpose()
}

impl Options {
    fn parse(method: &str, options: &glib::Variant) -> Result<Self, String> {
        if !options.is_type(glib::VariantTy::VARDICT) { return Err("Expected chooser options".into()); }
        let dict = glib::VariantDict::new(Some(options));
        let mode = match method {
            "OpenFile" => Mode::Open { multiple: option(&dict, "multiple")?.unwrap_or(false), directory: option(&dict, "directory")?.unwrap_or(false) },
            "SaveFile" => Mode::Save,
            "SaveFiles" => {
                let files = option::<Vec<Vec<u8>>>(&dict, "files")?.ok_or("Missing files option")?;
                let names = files.into_iter().map(|mut bytes| {
                    if bytes.pop() != Some(0) { return Err("File names must be NUL-terminated".to_string()); }
                    let name = OsString::from_vec(bytes);
                    if !valid_name(&name) { return Err("Invalid destination filename".into()); }
                    Ok(name)
                }).collect::<Result<Vec<_>, String>>()?;
                if names.is_empty() { return Err("No destination filenames".into()); }
                Mode::SaveFiles(names)
            }
            _ => return Err("Unsupported chooser request".into()),
        };
        let current_file = byte_path(&dict, "current_file")?;
        let folder = byte_path(&dict, "current_folder")?
            .or_else(|| current_file.as_deref().and_then(Path::parent).map(Path::to_path_buf))
            .filter(|path| path.is_dir()).unwrap_or_else(glib::home_dir);
        let suggested_name = option::<String>(&dict, "current_name")?;
        let raw_name = if suggested_name.is_none() { current_file.as_deref().and_then(Path::file_name).map(OsStr::to_os_string) } else { None };
        let name = suggested_name.unwrap_or_else(|| current_file.as_deref()
            .and_then(Path::file_name).map(|name| name.to_string_lossy().into_owned()).unwrap_or_default());
        let mut filters = option::<Vec<FilterSpec>>(&dict, "filters")?.unwrap_or_default();
        if filters.iter().any(|(_, rules)| rules.iter().any(|(kind, value)| *kind > 1 || value.contains('\0'))) {
            return Err("Invalid file filter".into());
        }
        let current_filter = if let Some(filter) = option::<FilterSpec>(&dict, "current_filter")? {
            if filter.1.iter().any(|(kind, value)| *kind > 1 || value.contains('\0')) { return Err("Invalid current filter".into()); }
            if filters.is_empty() { filters.push(filter.clone()); }
            filters.iter().position(|item| item == &filter).ok_or("Current filter is not in filters")? as u32
        } else { 0 };
        let mut choices = option::<Vec<ChoiceSpec>>(&dict, "choices")?.unwrap_or_default();
        let mut ids = std::collections::HashSet::new();
        for (id, _, values, selected) in &mut choices {
            if id.is_empty() || !ids.insert(id.clone()) { return Err("Invalid choice identifier".into()); }
            if values.is_empty() {
                if selected.is_empty() { *selected = "false".into(); }
                if selected != "true" && selected != "false" { return Err("Invalid boolean choice".into()); }
            } else {
                if selected.is_empty() { *selected = values[0].0.clone(); }
                if !values.iter().any(|(id, _)| id == selected) { return Err("Invalid selected choice".into()); }
            }
        }
        Ok(Self { mode, folder, name, raw_name, filters, current_filter, choices,
            accept_label: option(&dict, "accept_label")?.unwrap_or_else(|| if method == "OpenFile" { "_Open" } else { "_Save" }.into()),
            modal: option(&dict, "modal")?.unwrap_or(true) })
    }
}

fn file_filter(spec: &FilterSpec) -> gtk::FileFilter {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(&spec.0));
    for (kind, value) in &spec.1 {
        if *kind == 0 { filter.add_pattern(value); } else { filter.add_mime_type(value); }
    }
    filter
}

enum ChoiceControl { Boolean(String, gtk::CheckButton), List(String, Vec<String>, gtk::DropDown) }

struct Picker {
    window: glib::WeakRef<gtk::Window>,
    #[cfg(test)]
    css: glib::WeakRef<gtk::CssProvider>,
    options: Options,
    source: gtk::DirectoryList,
    contents: Rc<super::directory::Contents>,
    selection: gtk::SelectionModel,
    path: gtk::Entry,
    name: gtk::Entry,
    raw_name: RefCell<Option<OsString>>,
    error: gtk::Label,
    filters: gtk::DropDown,
    choices: Vec<ChoiceControl>,
    overwrite: Cell<bool>,
    complete: RefCell<Option<Box<dyn FnOnce(u32, glib::Variant)>>>,
}

fn empty_results() -> glib::Variant { glib::VariantDict::new(None).end() }

impl Picker {
    fn folder(&self) -> Option<PathBuf> { self.source.file().and_then(|file| file.path()) }

    fn set_name(&self, name: &OsStr) {
        self.name.set_text(&name.to_string_lossy());
        self.raw_name.replace(Some(name.to_os_string()));
    }

    fn selected(&self) -> Vec<PathBuf> {
        let selected = self.selection.selection();
        (0..selected.size()).filter_map(|i| self.selection.item(selected.nth(i as u32))
            .and_downcast::<gio::FileInfo>()?.attribute_object("standard::file")
            .and_downcast::<gio::File>()?.path()).collect()
    }

    fn finish(&self, response: u32, results: glib::Variant) {
        let window = self.window.upgrade();
        self.reply(response, results);
        if let Some(window) = window { window.destroy(); }
    }

    fn reply(&self, response: u32, results: glib::Variant) {
        let Some(complete) = self.complete.borrow_mut().take() else { return; };
        self.contents.stop();
        if let Some(window) = self.window.upgrade() {
            unsafe { window.steal_data::<Rc<Self>>("fs-picker"); }
        }
        complete(response, if response == 0 { results } else { empty_results() });
    }

    fn navigate(&self, path: &Path) {
        if !path.is_absolute() || !path.is_dir() { self.error.set_label("Choose an existing local folder"); return; }
        self.selection.unselect_all();
        self.path.set_text(&path.to_string_lossy());
        self.source.set_file(Some(&gio::File::for_path(path)));
        self.error.set_label("");
    }

    fn accept(self: &Rc<Self>) {
        if self.overwrite.get() || self.complete.borrow().is_none() { return; }
        let Some(mut folder) = self.folder() else { return; };
        let selected = self.selected();
        if matches!(self.options.mode, Mode::SaveFiles(_)) && selected.len() == 1 && selected[0].is_dir() { folder = selected[0].clone(); }
        if !matches!(self.options.mode, Mode::Open { directory: true, .. } | Mode::SaveFiles(_))
            && selected.len() == 1 && selected[0].is_dir() {
            self.navigate(&selected[0]); return;
        }
        let paths = match self.options.mode {
            Mode::Open { .. } => selection_paths(&self.options.mode, &folder, &selected),
            Mode::Save => match self.raw_name.borrow().as_ref() {
                Some(name) => save_paths(&Mode::SaveFiles(vec![name.clone()]), &folder, ""),
                None => save_paths(&Mode::Save, &folder, &self.name.text()),
            },
            _ => save_paths(&self.options.mode, &folder, &self.name.text()),
        };
        let paths = match paths { Ok(paths) => paths, Err(error) => { self.error.set_label(&error); return; } };
        if matches!(self.options.mode, Mode::Open { directory: false, .. }) {
            if let Some(spec) = self.options.filters.get(self.filters.selected() as usize) {
                let filter = file_filter(spec);
                if paths.iter().any(|path| gio::File::for_path(path).query_info("standard::*", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
                    .is_ok_and(|info| !filter.match_(&info))) {
                    self.error.set_label("Select files matching the chosen file type"); return;
                }
            }
        }
        let existing: Vec<_> = if matches!(self.options.mode, Mode::Open { .. }) { Vec::new() }
            else { paths.iter().filter(|path| path.symlink_metadata().is_ok()).collect() };
        if existing.is_empty() { self.accepted(&paths); return; }
        let Some(window) = self.window.upgrade() else { return; };
        let message = existing.iter().filter_map(|path| path.file_name()).map(|name| name.to_string_lossy()).collect::<Vec<_>>().join("\n");
        let dialog = adw::AlertDialog::new(Some("Replace existing files?"), Some(&message));
        dialog.add_responses(&[("cancel", "Cancel"), ("replace", "Replace")]);
        dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        self.overwrite.set(true);
        let weak = Rc::downgrade(self);
        dialog.choose(&window, gio::Cancellable::NONE, move |response| {
            let Some(this) = weak.upgrade() else { return; };
            this.overwrite.set(false);
            if response == "replace" { this.accepted(&paths); }
        });
    }

    fn accepted(&self, paths: &[PathBuf]) {
        let results = glib::VariantDict::new(None);
        let uris: Vec<_> = paths.iter().map(|path| gio::File::for_path(path).uri().to_string()).collect();
        results.insert("uris", uris);
        if let Some(filter) = self.options.filters.get(self.filters.selected() as usize) { results.insert("current_filter", filter); }
        if !self.choices.is_empty() {
            let values: Vec<(String, String)> = self.choices.iter().map(|choice| match choice {
                ChoiceControl::Boolean(id, button) => (id.clone(), button.is_active().to_string()),
                ChoiceControl::List(id, values, dropdown) => (id.clone(), values[dropdown.selected() as usize].clone()),
            }).collect();
            results.insert("choices", values);
        }
        self.finish(0, results.end());
    }

    fn activate(self: &Rc<Self>, position: u32) {
        let Some(info) = self.selection.item(position).and_downcast::<gio::FileInfo>() else { return; };
        let Some(path) = info.attribute_object("standard::file").and_downcast::<gio::File>().and_then(|file| file.path()) else { return; };
        if path.is_dir() { self.navigate(&path); }
        else {
            self.selection.select_item(position, true);
            if matches!(self.options.mode, Mode::Save) { self.set_name(path.file_name().unwrap_or_default()); }
            self.accept();
        }
    }
}

/// The callback is consumed on acceptance, cancellation, or destruction.
pub(crate) fn show(app: &adw::Application, method: &str, app_id: &str, parent_window: &str, title: &str,
    options: &glib::Variant, complete: impl FnOnce(u32, glib::Variant) + 'static) -> gtk::Window {
    let window: gtk::Window = adw::ApplicationWindow::builder().application(app).title(title)
        .default_width(800).default_height(560).build().upcast();
    let options = match Options::parse(method, options) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("filescope: file picker: {error}");
            let weak = window.downgrade();
            glib::idle_add_local_once(move || { if let Some(window) = weak.upgrade() { window.destroy(); } complete(2, empty_results()); });
            return window;
        }
    };
    // A modal request must not block another application's picker.
    gtk::WindowGroup::new().add_window(&window);
    window.set_modal(options.modal);
    let css = gtk::CssProvider::new();
    css.load_from_string(super::CSS);
    gtk::style_context_add_provider_for_display(&gtk::prelude::WidgetExt::display(&window), &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(title, app_id)));
    body.append(&header);
    let toolbar = gtk::Box::builder().spacing(8).margin_start(12).margin_end(12).build();
    let up = gtk::Button::from_icon_name("go-up-symbolic");
    up.set_tooltip_text(Some("Parent folder (Alt+Up)"));
    let home = gtk::Button::from_icon_name("go-home-symbolic");
    home.set_tooltip_text(Some("Home"));
    let path = gtk::Entry::builder().hexpand(true).placeholder_text("Folder path").build();
    path.update_property(&[gtk::accessible::Property::Label("Folder path")]);
    let hidden = gtk::CheckButton::with_label("Hidden files");
    toolbar.append(&up); toolbar.append(&home); toolbar.append(&path); toolbar.append(&hidden);
    body.append(&toolbar);
    let source = gtk::DirectoryList::new(Some(super::ATTRS), gio::File::NONE);
    source.set_monitored(false);
    let contents = super::directory::Contents::new(&source);
    let active_filter = Rc::new(RefCell::new(options.filters.get(options.current_filter as usize).map(file_filter)));
    let current = active_filter.clone();
    let hidden_ref = hidden.downgrade();
    let directories_only = matches!(options.mode, Mode::Open { directory: true, .. } | Mode::SaveFiles(_));
    let filter = gtk::CustomFilter::new(move |item| {
        let Some(info) = item.downcast_ref::<gio::FileInfo>() else { return false; };
        if info.is_hidden() && !hidden_ref.upgrade().is_some_and(|button| button.is_active()) { return false; }
        if info.file_type() == gio::FileType::Directory { return true; }
        !directories_only && current.borrow().as_ref().is_none_or(|filter| filter.match_(info))
    });
    let filtered = gtk::FilterListModel::new(Some(contents.items.clone()), Some(filter.clone()));
    let sorted = gtk::SortListModel::new(Some(filtered), None::<gtk::Sorter>);
    let selection: gtk::SelectionModel = if matches!(options.mode, Mode::Open { multiple: true, .. }) {
        gtk::MultiSelection::new(Some(sorted.clone())).upcast()
    } else { gtk::SingleSelection::builder().model(&sorted).autoselect(false).can_unselect(true).build().upcast() };
    let list = gtk::ColumnView::builder().model(&selection).hexpand(true).vexpand(true).enable_rubberband(true).build();
    super::view::fill_columns(None, &list, &Rc::new(Cell::new(80)));
    if let Some(sorter) = list.sorter() { sorted.set_sorter(Some(&super::view::with_hidden_last(&sorter))); }
    if let Some(column) = list.columns().item(0).and_downcast::<gtk::ColumnViewColumn>() { list.sort_by_column(Some(&column), gtk::SortType::Ascending); }
    body.append(&gtk::ScrolledWindow::builder().child(&list).vexpand(true).build());
    let footer = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8)
        .margin_start(12).margin_end(12).margin_bottom(12).build();
    let name = gtk::Entry::builder().text(&options.name).hexpand(true).activates_default(true).build();
    name.update_property(&[gtk::accessible::Property::Label("Filename")]);
    if matches!(options.mode, Mode::Save) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Label::new(Some("Name"))); row.append(&name); footer.append(&row);
    }
    if let Mode::SaveFiles(names) = &options.mode {
        let names = names.iter().map(|name| name.to_string_lossy()).collect::<Vec<_>>().join(", ");
        footer.append(&gtk::Label::builder().label(format!("Files: {names}")).xalign(0.0).wrap(true).build());
    }
    let labels: Vec<_> = options.filters.iter().map(|(name, _)| name.as_str()).collect();
    let filters = gtk::DropDown::from_strings(&labels);
    filters.set_selected(options.current_filter);
    filters.update_property(&[gtk::accessible::Property::Label("File type")]);
    if !labels.is_empty() && !directories_only { footer.append(&filters); }
    let mut choices = Vec::new();
    for (id, label, values, selected) in &options.choices {
        if values.is_empty() {
            let button = gtk::CheckButton::with_label(label);
            button.set_active(selected == "true"); footer.append(&button);
            choices.push(ChoiceControl::Boolean(id.clone(), button));
        } else {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            row.append(&gtk::Label::new(Some(label)));
            let dropdown = gtk::DropDown::from_strings(&values.iter().map(|(_, label)| label.as_str()).collect::<Vec<_>>());
            dropdown.set_selected(values.iter().position(|(id, _)| id == selected).unwrap_or(0) as u32);
            dropdown.update_property(&[gtk::accessible::Property::Label(label)]);
            row.append(&dropdown); footer.append(&row);
            choices.push(ChoiceControl::List(id.clone(), values.iter().map(|(id, _)| id.clone()).collect(), dropdown));
        }
    }
    let error = gtk::Label::builder().xalign(0.0).wrap(true).build();
    error.add_css_class("error"); footer.append(&error);
    let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::End).build();
    let cancel = gtk::Button::with_label("Cancel");
    let accept = gtk::Button::with_mnemonic(&options.accept_label);
    accept.add_css_class("suggested-action"); buttons.append(&cancel); buttons.append(&accept); footer.append(&buttons);
    body.append(&footer);
    window.downcast_ref::<adw::ApplicationWindow>().unwrap().set_content(Some(&body));
    window.set_default_widget(Some(&accept));
    let raw_name = RefCell::new(options.raw_name.clone());
    let picker = Rc::new(Picker { window: window.downgrade(),
        #[cfg(test)] css: css.downgrade(),
        options, source, contents, selection, raw_name,
        path: path.clone(), name, error, filters: filters.clone(), choices, overwrite: Cell::new(false), complete: RefCell::new(Some(Box::new(complete))) });
    unsafe { window.set_data("fs-picker", picker.clone()); }
    let weak = Rc::downgrade(&picker);
    window.connect_close_request(move |_| { if let Some(picker) = weak.upgrade() { picker.finish(1, empty_results()); } glib::Propagation::Proceed });
    let weak = Rc::downgrade(&picker);
    window.connect_destroy(move |_| { if let Some(picker) = weak.upgrade() { picker.finish(1, empty_results()); } });
    let weak = Rc::downgrade(&picker);
    let css = RefCell::new(Some(css));
    window.connect_unrealize(move |window| {
        if let Some(css) = css.borrow_mut().take() {
            gtk::style_context_remove_provider_for_display(&gtk::prelude::WidgetExt::display(window), &css);
        }
        if let Some(picker) = weak.upgrade() { picker.reply(1, empty_results()); }
    });
    let weak = Rc::downgrade(&picker);
    cancel.connect_clicked(move |_| { if let Some(picker) = weak.upgrade() { picker.finish(1, empty_results()); } });
    let weak = Rc::downgrade(&picker);
    accept.connect_clicked(move |_| { if let Some(picker) = weak.upgrade() { picker.accept(); } });
    let weak = Rc::downgrade(&picker);
    list.connect_activate(move |_, position| { if let Some(picker) = weak.upgrade() { picker.activate(position); } });
    let weak = Rc::downgrade(&picker);
    picker.name.connect_changed(move |_| { if let Some(picker) = weak.upgrade() { picker.raw_name.borrow_mut().take(); } });
    let weak = Rc::downgrade(&picker);
    picker.selection.connect_selection_changed(move |_, _, _| { if let Some(picker) = weak.upgrade() {
        if matches!(picker.options.mode, Mode::Save) {
            let paths = picker.selected();
            if paths.len() == 1 && paths[0].is_file() { picker.set_name(paths[0].file_name().unwrap_or_default()); }
        }
    } });
    let weak = Rc::downgrade(&picker);
    path.connect_activate(move |entry| { if let Some(picker) = weak.upgrade() {
        let file = gio::File::for_commandline_arg(entry.text().as_str());
        if let Some(path) = file.path() { picker.navigate(&path); } else { picker.error.set_label("Choose a local folder"); }
    } });
    let weak = Rc::downgrade(&picker);
    up.connect_clicked(move |_| { if let Some(picker) = weak.upgrade() { if let Some(parent) = picker.folder().as_deref().and_then(Path::parent) { picker.navigate(parent); } } });
    let weak = Rc::downgrade(&picker);
    home.connect_clicked(move |_| { if let Some(picker) = weak.upgrade() { picker.navigate(&glib::home_dir()); } });
    let weak = Rc::downgrade(&picker);
    let changed_filter = filter.clone();
    filters.connect_selected_notify(move |dropdown| { if let Some(picker) = weak.upgrade() {
        picker.selection.unselect_all(); *active_filter.borrow_mut() = picker.options.filters.get(dropdown.selected() as usize).map(file_filter);
        changed_filter.changed(gtk::FilterChange::Different);
    } });
    hidden.connect_toggled(move |_| filter.changed(gtk::FilterChange::Different));
    let weak = Rc::downgrade(&picker);
    picker.source.connect_loading_notify(move |source| { if !source.is_loading() { if let Some(picker) = weak.upgrade() {
        if let Some(error) = source.error() { picker.error.set_label(&error.to_string()); }
    } } });
    let weak = Rc::downgrade(&picker);
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, modifiers| {
        let Some(picker) = weak.upgrade() else { return glib::Propagation::Proceed; };
        match key {
            gtk::gdk::Key::Escape => picker.finish(1, empty_results()),
            gtk::gdk::Key::l if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) => { picker.path.grab_focus(); picker.path.select_region(0, -1); },
            gtk::gdk::Key::Up if modifiers.contains(gtk::gdk::ModifierType::ALT_MASK) => { if let Some(parent) = picker.folder().as_deref().and_then(Path::parent) { picker.navigate(parent); } },
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
    picker.navigate(&picker.options.folder);
    gtk::prelude::WidgetExt::realize(&window);
    set_parent(&window, parent_window);
    window.present();
    if matches!(picker.options.mode, Mode::Save) { picker.name.grab_focus(); picker.name.select_region(0, -1); } else { list.grab_focus(); }
    window
}

fn set_parent(window: &gtk::Window, parent: &str) {
    let Some(surface) = window.surface() else { return; };
    if let Some(handle) = parent.strip_prefix("wayland:") {
        if let Some(toplevel) = surface.downcast_ref::<gdk_wayland::WaylandToplevel>() { toplevel.set_transient_for_exported(handle); }
    } else if let Some(xid) = parent.strip_prefix("x11:").and_then(|value| u64::from_str_radix(value, 16).ok()) {
        if gtk::prelude::WidgetExt::display(window).type_().name() != "GdkX11Display" || xid == 0 { return; }
        use glib::translate::ToGlibPtr;
        // GDK's X11 symbols are optional on installations using Wayland only.
        unsafe {
            let display = libc::dlsym(libc::RTLD_DEFAULT, c"gdk_x11_display_get_xdisplay".as_ptr());
            let surface_id = libc::dlsym(libc::RTLD_DEFAULT, c"gdk_x11_surface_get_xid".as_ptr());
            let transient = libc::dlsym(libc::RTLD_DEFAULT, c"XSetTransientForHint".as_ptr());
            if display.is_null() || surface_id.is_null() || transient.is_null() { return; }
            let display: unsafe extern "C" fn(*mut gtk::gdk::ffi::GdkDisplay) -> *mut libc::c_void = std::mem::transmute(display);
            let surface_id: unsafe extern "C" fn(*mut gtk::gdk::ffi::GdkSurface) -> libc::c_ulong = std::mem::transmute(surface_id);
            let transient: unsafe extern "C" fn(*mut libc::c_void, libc::c_ulong, libc::c_ulong) -> libc::c_int = std::mem::transmute(transient);
            transient(display(gtk::prelude::WidgetExt::display(window).to_glib_none().0), surface_id(surface.to_glib_none().0), xid as libc::c_ulong);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!("filescope-picker-{}-{}", std::process::id(), gtk::glib::uuid_string_random()));
        std::fs::create_dir_all(path.join("folder")).unwrap();
        std::fs::write(path.join("one.txt"), "one").unwrap();
        std::fs::write(path.join("two.txt"), "two").unwrap();
        path
    }

    #[test]
    fn picker_respects_selection_mode() {
        let folder = fixture();
        let files = vec![folder.join("one.txt"), folder.join("two.txt")];
        assert_eq!(selection_paths(&Mode::Open { multiple: true, directory: false }, &folder, &files).unwrap(), files);
        assert!(selection_paths(&Mode::Open { multiple: false, directory: false }, &folder, &files).is_err());
        assert!(selection_paths(&Mode::Open { multiple: true, directory: false }, &folder, &[folder.join("folder")]).is_err());
        assert_eq!(selection_paths(&Mode::Open { multiple: false, directory: true }, &folder, &[]).unwrap(), vec![folder.clone()]);
        assert!(selection_paths(&Mode::Open { multiple: false, directory: true }, &folder, &files[..1]).is_err());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn picker_save_rejects_paths_and_directories() {
        let folder = fixture();
        for name in ["", ".", "..", "../escape", "/tmp/escape", "nul\0name", "folder"] {
            assert!(save_paths(&Mode::Save, &folder, name).is_err(), "{name:?}");
        }
        assert_eq!(save_paths(&Mode::Save, &folder, "a b.txt").unwrap(), vec![folder.join("a b.txt")]);
        assert_eq!(save_paths(&Mode::SaveFiles(vec!["one.txt".into(), "two.txt".into()]), &folder, "").unwrap(), vec![folder.join("one.txt"), folder.join("two.txt")]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    fn options(folder: &Path) -> glib::VariantDict {
        let options = glib::VariantDict::new(None);
        let mut bytes = folder.as_os_str().as_bytes().to_vec(); bytes.push(0);
        options.insert("current_folder", bytes);
        options
    }

    #[test]
    fn picker_options_preserve_paths_and_reject_wrong_types() {
        let folder = fixture();
        let dict = options(&folder);
        dict.insert("multiple", "yes");
        assert!(Options::parse("OpenFile", &dict.end()).is_err());
        let dict = options(&folder);
        dict.insert("current_folder", b"relative\0".to_vec());
        assert!(Options::parse("OpenFile", &dict.end()).is_err());
        let dict = options(&folder);
        dict.insert("files", vec![b"../escape\0".to_vec()]);
        assert!(Options::parse("SaveFiles", &dict.end()).is_err());
        let dict = options(&folder);
        let mut raw_file = folder.join(OsString::from_vec(b"name-\xff.txt".to_vec())).as_os_str().as_bytes().to_vec(); raw_file.push(0);
        dict.insert("current_file", raw_file);
        let parsed = Options::parse("SaveFile", &dict.end()).unwrap();
        assert_eq!(parsed.folder, folder);
        assert_eq!(parsed.raw_name.unwrap().as_bytes(), b"name-\xff.txt");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    fn picker_options_keep_current_filter_and_choices() {
        let folder = fixture();
        let dict = options(&folder);
        let filters: Vec<FilterSpec> = vec![("Text".into(), vec![(0, "*.txt".into())]), ("Images".into(), vec![(1, "image/*".into())])];
        dict.insert("filters", &filters);
        dict.insert("current_filter", &filters[1]);
        let choices: Vec<ChoiceSpec> = vec![("readonly".into(), "Read only".into(), vec![], "true".into()),
            ("encoding".into(), "Encoding".into(), vec![("utf8".into(), "UTF-8".into())], "".into())];
        dict.insert("choices", choices);
        let parsed = Options::parse("OpenFile", &dict.end()).unwrap();
        assert_eq!(parsed.current_filter, 1);
        assert_eq!(parsed.choices[1].3, "utf8");
        let dict = options(&folder);
        dict.insert("filters", vec![("Bad".to_string(), vec![(2u32, "*.txt".to_string())])]);
        assert!(Options::parse("OpenFile", &dict.end()).is_err());
        std::fs::remove_dir_all(folder).unwrap();
    }

    fn app() -> adw::Application {
        adw::init().unwrap();
        let app = adw::Application::builder().application_id("dev.filescope.PickerTest").flags(gio::ApplicationFlags::NON_UNIQUE).build();
        app.register(gio::Cancellable::NONE).unwrap();
        app
    }

    fn state(window: &gtk::Window) -> Rc<Picker> {
        unsafe { window.data::<Rc<Picker>>("fs-picker").unwrap().as_ref().clone() }
    }

    fn pump_until(ready: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ready() {
            while glib::MainContext::default().pending() { glib::MainContext::default().iteration(false); }
            assert!(std::time::Instant::now() < deadline, "chooser operation timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn load(picker: &Picker) { pump_until(|| !picker.source.is_loading() && picker.selection.n_items() > 0); }

    fn select(picker: &Picker, path: &Path, clear: bool) {
        let index = (0..picker.selection.n_items()).find(|index| picker.selection.item(*index)
            .and_downcast::<gio::FileInfo>().and_then(|info| info.attribute_object("standard::file").and_downcast::<gio::File>())
            .and_then(|file| file.path()).as_deref() == Some(path)).expect("file is visible");
        picker.selection.select_item(index, clear);
    }

    fn click(root: &gtk::Widget, label: &str) -> bool {
        if let Some(button) = root.downcast_ref::<gtk::Button>() { if button.label().as_deref() == Some(label) { button.emit_clicked(); return true; } }
        let mut child = root.first_child();
        while let Some(widget) = child { if click(&widget, label) { return true; } child = widget.next_sibling(); }
        false
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_open_cancel_and_requests_keep_separate_state() {
        let app = app(); let folder = fixture();
        let first = Rc::new(RefCell::new(Vec::new())); let result = first.clone();
        let one = show(&app, "OpenFile", "one", "", "Open", &options(&folder).end(), move |code, results| result.borrow_mut().push((code, results)));
        let second = Rc::new(RefCell::new(Vec::new())); let result = second.clone();
        let two = show(&app, "OpenFile", "two", "", "Open", &options(&folder).end(), move |code, results| result.borrow_mut().push((code, results)));
        assert_ne!(one.group(), two.group());
        let a = state(&one); let b = state(&two); load(&a); load(&b);
        select(&a, &folder.join("one.txt"), true); select(&b, &folder.join("two.txt"), true);
        assert!(click(one.upcast_ref(), "_Open"));
        assert_eq!(first.borrow().len(), 1); assert!(second.borrow().is_empty());
        let uris = glib::VariantDict::new(Some(&first.borrow()[0].1)).lookup::<Vec<String>>("uris").unwrap().unwrap();
        assert_eq!(uris, vec![gio::File::for_path(folder.join("one.txt")).uri().to_string()]);
        two.close(); b.finish(1, empty_results());
        assert_eq!(second.borrow().len(), 1); assert_eq!(second.borrow()[0].0, 1);
        assert_eq!(second.borrow()[0].1.n_children(), 0);
        let weak = Rc::downgrade(&a); drop(a); assert!(weak.upgrade().is_none());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_filters_multiple_and_folder_selection() {
        let app = app(); let folder = fixture(); std::fs::write(folder.join("other.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let dict = options(&folder); dict.insert("multiple", true);
        let spec: FilterSpec = ("Text".into(), vec![(0, "*.txt".into())]); dict.insert("filters", vec![spec.clone()]);
        let choices: Vec<ChoiceSpec> = vec![("readonly".into(), "Read only".into(), vec![], "true".into())]; dict.insert("choices", choices);
        let result = Rc::new(RefCell::new(None)); let output = result.clone();
        let window = show(&app, "OpenFile", "test", "", "Open", &dict.end(), move |code, results| { output.replace(Some((code, results))); });
        let picker = state(&window); load(&picker);
        assert_eq!(picker.selection.n_items(), 3);
        select(&picker, &folder.join("one.txt"), true); select(&picker, &folder.join("two.txt"), false); picker.accept();
        let result = result.borrow(); let (code, values) = result.as_ref().unwrap(); assert_eq!(*code, 0);
        let values = glib::VariantDict::new(Some(values));
        assert_eq!(values.lookup::<Vec<String>>("uris").unwrap().unwrap().len(), 2);
        assert_eq!(values.lookup::<FilterSpec>("current_filter").unwrap().unwrap(), spec);
        assert_eq!(values.lookup::<Vec<(String, String)>>("choices").unwrap().unwrap(), vec![("readonly".into(), "true".into())]);
        let dict = options(&folder); dict.insert("directory", true);
        let result = Rc::new(RefCell::new(None)); let output = result.clone();
        let window = show(&app, "OpenFile", "test", "", "Folder", &dict.end(), move |code, results| { output.replace(Some((code, results))); });
        let picker = state(&window); load(&picker); assert_eq!(picker.selection.n_items(), 1); picker.accept();
        let result = result.borrow(); let (code, values) = result.as_ref().unwrap(); assert_eq!(*code, 0);
        assert_eq!(glib::VariantDict::new(Some(values)).lookup::<Vec<String>>("uris").unwrap().unwrap(), vec![gio::File::for_path(&folder).uri().to_string()]);
        let dict = options(&folder);
        let mime: FilterSpec = ("Images".into(), vec![(1, "image/*".into())]);
        dict.insert("filters", vec![spec, mime.clone()]); dict.insert("current_filter", &mime);
        let result = Rc::new(RefCell::new(None)); let output = result.clone();
        let window = show(&app, "OpenFile", "test", "", "Image", &dict.end(), move |code, results| { output.replace(Some((code, results))); });
        let picker = state(&window); load(&picker); assert_eq!(picker.selection.n_items(), 2);
        select(&picker, &folder.join("other.png"), true); picker.accept();
        let result = result.borrow(); let (code, values) = result.as_ref().unwrap(); assert_eq!(*code, 0);
        assert_eq!(glib::VariantDict::new(Some(values)).lookup::<FilterSpec>("current_filter").unwrap().unwrap(), mime);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_save_requires_overwrite_confirmation() {
        let app = app(); let folder = fixture(); let dict = options(&folder); dict.insert("current_name", "one.txt");
        let result = Rc::new(RefCell::new(None)); let output = result.clone();
        let window = show(&app, "SaveFile", "test", "", "Save", &dict.end(), move |code, results| { output.replace(Some((code, results))); });
        let picker = state(&window); load(&picker); assert_eq!(picker.name.text(), "one.txt"); picker.accept();
        assert!(result.borrow().is_none()); assert!(picker.overwrite.get());
        let parent = window.downcast_ref::<adw::ApplicationWindow>().unwrap();
        let dialog = parent.visible_dialog().unwrap();
        assert!(click(dialog.upcast_ref(), "Cancel")); pump_until(|| !picker.overwrite.get()); assert!(result.borrow().is_none());
        picker.accept();
        let dialog = parent.visible_dialog().unwrap(); assert!(click(dialog.upcast_ref(), "Replace"));
        pump_until(|| result.borrow().is_some());
        let result = result.borrow(); let (code, values) = result.as_ref().unwrap(); assert_eq!(*code, 0);
        assert_eq!(glib::VariantDict::new(Some(values)).lookup::<Vec<String>>("uris").unwrap().unwrap(), vec![gio::File::for_path(folder.join("one.txt")).uri().to_string()]);
        assert_eq!(std::fs::read_to_string(folder.join("one.txt")).unwrap(), "one");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_save_preserves_selected_raw_name_until_edited() {
        let app = app(); let folder = fixture();
        let first = folder.join(OsString::from_vec(b"name-\xff.txt".to_vec()));
        let second = folder.join(OsString::from_vec(b"name-\xfe.txt".to_vec()));
        std::fs::write(&first, "first").unwrap(); std::fs::write(&second, "second").unwrap();
        assert_eq!(first.file_name().unwrap().to_string_lossy(), second.file_name().unwrap().to_string_lossy());
        for suggested in [None, Some(&first)] {
            let dict = options(&folder);
            if let Some(file) = suggested { let mut bytes = file.as_os_str().as_bytes().to_vec(); bytes.push(0); dict.insert("current_file", bytes); }
            let result = Rc::new(RefCell::new(None)); let output = result.clone();
            let window = show(&app, "SaveFile", "test", "", "Save", &dict.end(), move |code, results| { output.replace(Some((code, results))); });
            let picker = state(&window); load(&picker); select(&picker, &second, true);
            if suggested.is_some() { picker.activate(picker.selection.selection().nth(0)); }
            else { assert!(click(window.upcast_ref(), "_Save")); }
            assert!(result.borrow().is_none(), "a selected existing raw filename needs overwrite confirmation");
            assert!(picker.overwrite.get());
            let parent = window.downcast_ref::<adw::ApplicationWindow>().unwrap();
            assert!(click(parent.visible_dialog().unwrap().upcast_ref(), "Cancel")); pump_until(|| !picker.overwrite.get());
            assert!(result.borrow().is_none()); assert!(!folder.join("name-\u{fffd}.txt").exists());
            picker.accept(); assert!(click(parent.visible_dialog().unwrap().upcast_ref(), "Replace"));
            pump_until(|| result.borrow().is_some());
            let result = result.borrow(); let (code, values) = result.as_ref().unwrap(); assert_eq!(*code, 0);
            assert_eq!(glib::VariantDict::new(Some(values)).lookup::<Vec<String>>("uris").unwrap().unwrap(), vec![gio::File::for_path(&second).uri().to_string()]);
            assert_eq!(std::fs::read_to_string(&first).unwrap(), "first"); assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        }
        let dict = options(&folder);
        let mut bytes = first.as_os_str().as_bytes().to_vec(); bytes.push(0); dict.insert("current_file", bytes);
        let result = Rc::new(RefCell::new(None)); let output = result.clone();
        let window = show(&app, "SaveFile", "test", "", "Save", &dict.end(), move |code, results| { output.replace(Some((code, results))); });
        let picker = state(&window); load(&picker); picker.accept();
        assert!(result.borrow().is_none()); assert!(picker.overwrite.get());
        let parent = window.downcast_ref::<adw::ApplicationWindow>().unwrap();
        assert!(click(parent.visible_dialog().unwrap().upcast_ref(), "Cancel")); pump_until(|| !picker.overwrite.get());
        select(&picker, &second, true); picker.name.set_text("new-name.txt"); picker.accept();
        let result = result.borrow(); let (code, values) = result.as_ref().unwrap(); assert_eq!(*code, 0);
        assert_eq!(glib::VariantDict::new(Some(values)).lookup::<Vec<String>>("uris").unwrap().unwrap(), vec![gio::File::for_path(folder.join("new-name.txt")).uri().to_string()]);
        assert!(!folder.join("new-name.txt").exists());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_savefiles_preserves_names_and_encoded_uris() {
        let app = app(); let folder = fixture(); let dict = options(&folder);
        dict.insert("files", vec![b"a b.txt\0".to_vec(), b"name-\xff.txt\0".to_vec()]);
        let result = Rc::new(RefCell::new(None)); let output = result.clone();
        let window = show(&app, "SaveFiles", "test", "", "Save", &dict.end(), move |code, results| { output.replace(Some((code, results))); });
        let picker = state(&window); load(&picker); select(&picker, &folder.join("folder"), true); picker.accept();
        let result = result.borrow(); let (code, values) = result.as_ref().unwrap(); assert_eq!(*code, 0);
        let uris = glib::VariantDict::new(Some(values)).lookup::<Vec<String>>("uris").unwrap().unwrap();
        assert_eq!(uris.len(), 2); assert!(uris[0].ends_with("a%20b.txt")); assert!(uris[1].ends_with("name-%FF.txt"));
        assert_eq!(gio::File::for_uri(&uris[0]).path().unwrap().parent().unwrap(), folder.join("folder"));
        assert!(!folder.join("a b.txt").exists());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_destroy_cancels_pending_overwrite_and_releases_state() {
        let app = app(); let folder = fixture(); let dict = options(&folder); dict.insert("current_name", "one.txt");
        let result = Rc::new(RefCell::new(Vec::new())); let output = result.clone();
        let window = show(&app, "SaveFile", "test", "", "Save", &dict.end(), move |code, results| output.borrow_mut().push((code, results)));
        let picker = state(&window); let weak = Rc::downgrade(&picker); load(&picker); picker.accept();
        let css = picker.css.clone(); assert!(css.upgrade().is_some());
        assert!(picker.overwrite.get());
        let dialog = window.downcast_ref::<adw::ApplicationWindow>().unwrap().visible_dialog().unwrap();
        window.destroy();
        assert!(css.upgrade().is_none(), "a closed picker must release its display CSS provider");
        assert_eq!(result.borrow().len(), 1); assert_eq!(result.borrow()[0].0, 1); assert_eq!(result.borrow()[0].1.n_children(), 0);
        dialog.emit_by_name::<()>("response", &[&"replace"]);
        while glib::MainContext::default().pending() { glib::MainContext::default().iteration(false); }
        assert_eq!(result.borrow().len(), 1);
        drop(picker); assert!(weak.upgrade().is_none());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
