use std::cell::Cell;
use std::path::Path;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use glib::translate::{from_glib_full, ToGlibPtr};
use crate::trash;

/// Keep the storage identity for Restore, but render the original name and type.
fn item_info(root: &Path, entry: &trash::Entry) -> gio::FileInfo {
    let file = gio::File::for_path(root.join("files").join(&entry.name));
    let info = file.query_info(super::ATTRS, gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
        .or_else(|_| file.query_info(super::ATTRS, gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, gio::Cancellable::NONE))
        .unwrap_or_else(|_| {
            let info = gio::FileInfo::new();
            info.set_file_type(gio::FileType::Unknown);
            info.set_size(0);
            info
        });
    let label = entry.original.file_name().unwrap_or_else(|| std::ffi::OsStr::new(&entry.name)).to_string_lossy();
    info.set_name(&entry.name);
    info.set_display_name(&label);
    info.set_attribute_boolean("standard::is-hidden", label.starts_with('.'));
    info.set_attribute_object("standard::file", &file);
    if info.file_type() != gio::FileType::Directory
        && info.attribute_string("standard::content-type").as_deref().is_none_or(gio::content_type_is_unknown) {
        // A null data pointer guesses by filename; an empty slice means an empty file.
        let mut uncertain = 0;
        let mime: glib::GString = unsafe {
            from_glib_full(gio::ffi::g_content_type_guess(
                entry.original.as_path().to_glib_none().0, std::ptr::null(), 0, &mut uncertain,
            ))
        };
        info.set_content_type(&mime);
        info.set_icon(&gio::content_type_get_icon(&mime));
    }
    info.set_attribute_string("trash::orig-path", &entry.original.to_string_lossy());
    info.set_attribute_string("trash::deletion-date", entry.deleted.as_deref().unwrap_or(""));
    info.set_attribute_string("filescope::original-location", &entry.original.parent().unwrap_or(Path::new("")).to_string_lossy());
    info
}

fn entry_from_info(info: &gio::FileInfo) -> Option<trash::Entry> {
    Some(trash::Entry {
        name: info.name().to_str()?.to_owned(),
        original: info.attribute_string("trash::orig-path")?.as_str().into(),
        deleted: info.attribute_string("trash::deletion-date").filter(|s| !s.is_empty()).map(|s| s.to_string()),
    })
}

pub(super) struct Pane {
    pub widget: gtk::Box,
    pub selection: gtk::MultiSelection,
    model: gio::ListStore,
    stack: gtk::Stack,
    grid: gtk::GridView,
    list: gtk::ColumnView,
    zoom: Rc<Cell<i32>>,
}

impl Pane {
    pub fn new(app: &Rc<super::App>, tab: &Rc<super::Tab>) -> Rc<Self> {
        let model = gio::ListStore::new::<gio::FileInfo>();
        let sorted = gtk::SortListModel::new(Some(model.clone()), None::<gtk::Sorter>);
        let selection = gtk::MultiSelection::new(Some(sorted.clone()));
        let zoom = Rc::new(Cell::new(app.zoom.get()));
        let grid = gtk::GridView::builder().model(&selection).max_columns(24).enable_rubberband(true).build();
        grid.add_css_class("fs-grid");
        grid.set_factory(Some(&super::view::grid_factory(None, &grid, &zoom)));
        let list = gtk::ColumnView::builder().model(&selection).enable_rubberband(true).build();
        super::view::fill_columns(None, &list, &zoom);
        list.append_column(&detail_column("Original location", "filescope::original-location"));
        list.append_column(&detail_column("Deleted", "trash::deletion-date"));
        if let Some(sorter) = list.sorter() {
            sorted.set_sorter(Some(&super::view::with_hidden_last(&sorter)));
        }
        if let Some(column) = list.columns().item(0).and_downcast::<gtk::ColumnViewColumn>() {
            column.set_fixed_width(220);
            list.sort_by_column(Some(&column), gtk::SortType::Ascending);
        }
        let stack = gtk::Stack::builder().vexpand(true).build();
        let grid_scroll = gtk::ScrolledWindow::builder().vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).child(&grid).build();
        let list_scroll = gtk::ScrolledWindow::builder().vexpand(true).child(&list).build();
        stack.add_named(&grid_scroll, Some("grid"));
        stack.add_named(&list_scroll, Some("list"));
        let empty = adw::StatusPage::builder().title("Trash Is Empty")
            .description("Items you delete land here, and can be put back.").build();
        crate::icon::show_status(&empty, "user-trash-symbolic", 128);
        stack.add_named(&empty, Some("empty"));
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let bar = gtk::Box::builder().spacing(12).halign(gtk::Align::End)
            .margin_top(12).margin_bottom(8).margin_start(12).margin_end(12).build();
        let restore_button = gtk::Button::with_label("Restore");
        restore_button.set_action_name(Some("trash.restore"));
        bar.append(&restore_button);
        let empty_button = gtk::Button::with_label("Empty Trash");
        empty_button.add_css_class("destructive-action");
        let weak = Rc::downgrade(app);
        empty_button.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() { super::places::confirm_empty_trash(&app); }
        });
        bar.append(&empty_button);
        widget.append(&bar);
        widget.append(&stack);
        let pane = Rc::new(Self { widget, selection: selection.clone(), model, stack, grid, list, zoom });
        let restore = gio::SimpleAction::new("restore", None);
        restore.set_enabled(false);
        let a = Rc::downgrade(app);
        let t = Rc::downgrade(tab);
        let p = Rc::downgrade(&pane);
        restore.connect_activate(move |_, _| {
            if let (Some(app), Some(tab), Some(pane)) = (a.upgrade(), t.upgrade(), p.upgrade()) {
                super::places::restore_trash_selection(&app, &tab, &pane.selected());
            }
        });
        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&restore);
        pane.widget.insert_action_group("trash", Some(&actions));
        let weak = Rc::downgrade(app);
        selection.connect_selection_changed(move |selection, _, _| {
            let count = selection.selection().size();
            restore.set_enabled(count > 0);
            restore_button.set_label(&if count == 0 { "Restore".into() } else { format!("Restore ({count})") });
            if let Some(app) = weak.upgrade() { super::update_chrome(&app); }
        });
        super::view::attach_context_menu(app, &pane.grid);
        super::view::attach_context_menu(app, &pane.list);
        attach_menu_key(&pane.grid);
        attach_menu_key(&pane.list);
        pane
    }

    pub fn refresh(&self, app: &Rc<super::App>) {
        let root = trash::home_trash();
        let items: Vec<_> = trash::entries_in(&root).iter().map(|entry| item_info(&root, entry)).collect();
        // Storage names can be reused by another file manager between refreshes.
        self.selection.unselect_all();
        self.model.splice(0, self.model.n_items(), &items);
        self.show_mode(app);
    }

    pub fn show_mode(&self, app: &super::App) {
        self.set_zoom(app.zoom.get());
        self.stack.set_visible_child_name(if self.model.n_items() == 0 { "empty" } else if app.is_list.get() { "list" } else { "grid" });
    }

    pub fn set_zoom(&self, size: i32) {
        if self.zoom.replace(size) == size { return; }
        super::view::resize_names(self.grid.upcast_ref(), size);
        super::view::resize_names(self.list.upcast_ref(), super::view::list_icon(size));
    }

    fn selected(&self) -> Vec<trash::Entry> {
        let selected = self.selection.selection();
        (0..selected.size()).filter_map(|i| self.selection.item(selected.nth(i as u32))
            .and_downcast::<gio::FileInfo>().and_then(|info| entry_from_info(&info))).collect()
    }
}

fn detail_column(title: &str, attribute: &'static str) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_child(Some(&gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build()));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        let value = info.attribute_string(attribute).unwrap_or_default();
        label.set_label(&value);
        label.set_tooltip_text(Some(&value));
        unsafe { label.set_data("fs-position", item.position()); }
    });
    gtk::ColumnViewColumn::builder().title(title).factory(&factory).resizable(true).fixed_width(180).build()
}

fn attach_menu_key(view: &impl IsA<gtk::Widget>) {
    let key = gtk::EventControllerKey::new();
    let weak = view.as_ref().downgrade();
    key.connect_key_pressed(move |_, key, _, modifiers| {
        if key == gtk::gdk::Key::Menu || (key == gtk::gdk::Key::F10 && modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK)) {
            if let Some(view) = weak.upgrade() {
                let menu = gio::Menu::new();
                menu.append(Some("Restore"), Some("trash.restore"));
                super::view::popup_context_menu(&view, &menu, 12.0, 12.0);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    view.add_controller(key);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trash;
    use gtk::gio;

    fn write_png(path: &Path) {
        let image = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 1, 1).unwrap();
        image.fill(0xff8000ff);
        image.savev(path, "png", &[]).unwrap();
    }

    #[test]
    fn trash_content_type_preserves_detection_and_name_fallback() {
        let root = std::env::temp_dir().join(format!("filescope-trash-type-fix-{}", std::process::id()));
        std::fs::create_dir_all(root.join("files")).unwrap();
        write_png(&root.join("files/storage-id"));
        let detected = gio::File::for_path(root.join("files/storage-id"))
            .query_info(super::super::ATTRS, gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE).unwrap();
        for name in ["Screenshot", "photo.txt", "photo.png"] {
            let entry = trash::Entry { name: "storage-id".into(), original: Path::new("/original").join(name), deleted: None };
            let info = item_info(&root, &entry);
            assert_eq!(info.content_type().as_deref(), Some("image/png"), "lost detected type for {name}");
            assert!(info.icon().unwrap().equal(Some(&detected.icon().unwrap())));
        }
        let missing = trash::Entry { name: "unavailable".into(), original: "/original/photo.png".into(), deleted: None };
        assert_eq!(item_info(&root, &missing).content_type().as_deref(), Some("image/png"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_DATA_HOME"]
    fn trash_refresh_does_not_select_reused_storage_name() {
        use super::super::*;
        let data = std::path::PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap());
        assert!(data.starts_with(std::env::temp_dir()));
        let work = data.parent().unwrap();
        assert!(work.file_name().unwrap().to_string_lossy().starts_with("filescope-trash-fixes-"));
        let root = data.join("Trash");
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::create_dir_all(root.join("info")).unwrap();
        std::fs::create_dir_all(work.join("folder")).unwrap();
        let old = trash::Entry {
            name: "same.txt".into(), original: work.join("old/same.txt"), deleted: Some("2026-10-22T10:00:00".into()),
        };
        std::fs::write(root.join("files/same.txt"), b"old item").unwrap();
        std::fs::write(root.join("info/same.txt.trashinfo"), format!("[Trash Info]\nPath={}\nDeletionDate=2026-10-22T10:00:00\n", old.original.display())).unwrap();
        adw::init().unwrap();
        gtk::Settings::default().unwrap().set_gtk_enable_animations(false);
        let application = adw::Application::builder().application_id("dev.filescope.TrashRefreshTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE).build();
        application.register(gio::Cancellable::NONE).unwrap();
        glib::MainContext::default().block_on(async {
            open_files(&application, &[gio::File::for_path(work.join("folder"))]);
            let app = app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            navigation::go_trash(&app, &tab);
            let pane = tab.trash_view.borrow().clone().unwrap();
            pane.selection.select_item(0, true);
            assert_eq!(pane.selected(), vec![old.clone()]);
            // Another file manager restores the old item, then reuses its storage name.
            trash::restore_in(&root, &old).unwrap();
            let replacement = work.join("new/same.txt");
            std::fs::write(root.join("files/same.txt"), b"replacement item").unwrap();
            std::fs::write(root.join("info/same.txt.trashinfo"), format!("[Trash Info]\nPath={}\nDeletionDate=2026-10-22T11:00:00\n", replacement.display())).unwrap();
            gtk::prelude::WidgetExt::activate_action(&app.window, "win.refresh", None).unwrap();
            assert!(pane.selected().is_empty(), "refresh selected a replacement the user never chose");
            pane.widget.activate_action("trash.restore", None).unwrap();
            assert!(!replacement.exists());
            assert_eq!(std::fs::read(root.join("files/same.txt")).unwrap(), b"replacement item");
            assert!(root.join("info/same.txt.trashinfo").exists());
            app.window.close();
        });
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary Trash fixture"]
    fn trash_grid_and_list_keep_selection_and_restore_identity() {
        use super::super::*;
        let data = std::env::var_os("XDG_DATA_HOME").map(std::path::PathBuf::from).unwrap();
        assert!(data.starts_with(std::env::temp_dir()));
        assert!(data.parent().unwrap().file_name().unwrap().to_string_lossy().starts_with("filescope-trash-appearance-acceptance-"));
        let root = data.parent().unwrap();
        adw::init().unwrap();
        gtk::Settings::default().unwrap().set_gtk_enable_animations(false);
        let application = adw::Application::builder().application_id("dev.filescope.TrashAppearanceTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE).build();
        application.register(gio::Cancellable::NONE).unwrap();
        glib::MainContext::default().block_on(async {
            open_files(&application, &[gio::File::for_path(root.join("folder"))]);
            let app = app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            let mut pending = vec![app.window.clone().upcast::<gtk::Widget>()];
            let mut toggle = None;
            while let Some(widget) = pending.pop() {
                if let Some(button) = widget.downcast_ref::<gtk::ToggleButton>()
                    && matches!(button.tooltip_text().as_deref(), Some("Grid view" | "List view")) {
                    toggle = Some(button.clone());
                    break;
                }
                let mut child = widget.first_child();
                while let Some(widget) = child {
                    child = widget.next_sibling();
                    pending.push(widget);
                }
            }
            let toggle = toggle.unwrap();
            toggle.set_active(false);
            glib::timeout_future(std::time::Duration::from_millis(200)).await;
            tab.selection.select_item(0, true);
            let old_selection = tab.selection.selection().size();
            assert_eq!(old_selection, 1);
            navigation::go_trash(&app, &tab);
            let pane = tab.trash_view.borrow().clone().unwrap();
            assert!(pane.grid.enables_rubberband());
            assert!(pane.list.enables_rubberband());
            assert_eq!(pane.stack.visible_child_name().as_deref(), Some("grid"));
            assert_eq!(pane.grid.model().unwrap(), pane.selection.clone().upcast::<gtk::SelectionModel>());
            assert_eq!(pane.list.model().unwrap(), pane.selection.clone().upcast::<gtk::SelectionModel>());
            let initial = pane.model.n_items();
            assert!(initial >= 5);
            for i in 0..pane.selection.n_items() {
                let info = pane.selection.item(i).and_downcast::<gio::FileInfo>().unwrap();
                if info.display_name() == "notes.txt" { pane.selection.select_item(i, false); }
            }
            assert_eq!(pane.selected().len(), 2);
            assert_eq!(app.status.label(), "2 selected");
            assert_eq!(tab.selection.selection().size(), old_selection);
            pane.grid.emit_by_name::<()>("activate", &[&0u32]);
            pane.list.emit_by_name::<()>("activate", &[&0u32]);
            assert_eq!(trash::entries_in(&data.join("Trash")).len(), initial as usize);
            let first = pane.model.item(0);
            toggle.set_active(true);
            assert_eq!(pane.model.item(0), first, "switching views must not reload the model");
            pane.set_zoom(128);
            assert_eq!(pane.stack.visible_child_name().as_deref(), Some("list"));
            assert_eq!(pane.selected().len(), 2);
            assert_eq!(pane.zoom.get(), 128);
            pane.widget.activate_action("trash.restore", None).unwrap();
            assert_eq!(pane.model.n_items(), initial - 2);
            assert_eq!(std::fs::read_to_string(root.join("original/one/notes.txt")).unwrap(), "contents of a-storage");
            assert_eq!(std::fs::read_to_string(root.join("original/two/notes.txt")).unwrap(), "contents of b-storage");
            pane.selection.select_all();
            pane.widget.activate_action("trash.restore", None).unwrap();
            assert_eq!(pane.model.n_items(), 0);
            assert_eq!(pane.stack.visible_child_name().as_deref(), Some("empty"));
            assert_eq!(app.status.label(), "0 items");
            app.window.close();
        });
    }

    #[test]
    fn trash_item_uses_original_name_and_file_type() {
        let root = std::env::temp_dir().join(format!("filescope-trash-appearance-{}", std::process::id()));
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::write(root.join("files/unique-storage-name"), b"text").unwrap();
        let entry = trash::Entry {
            name: "unique-storage-name".into(), original: "/original/notes.txt".into(),
            deleted: Some("2026-10-22T12:00:00".into()),
        };
        let info = item_info(&root, &entry);
        assert_eq!(info.display_name(), "notes.txt");
        assert_eq!(info.file_type(), gio::FileType::Regular);
        assert_eq!(info.content_type().as_deref(), Some("text/plain"));
        let image = trash::Entry { original: "/original/photo.png".into(), ..entry.clone() };
        write_png(&root.join("files/unique-storage-name"));
        assert_eq!(item_info(&root, &image).content_type().as_deref(), Some("image/png"));
        assert_eq!(info.attribute_string("trash::orig-path").as_deref(), Some("/original/notes.txt"));
        assert_eq!(info.attribute_string("trash::deletion-date").as_deref(), Some("2026-10-22T12:00:00"));
        let stored = info.attribute_object("standard::file").and_downcast::<gio::File>().unwrap();
        assert_eq!(stored.path().unwrap(), root.join("files/unique-storage-name"));
        std::fs::create_dir(root.join("files/folder-id")).unwrap();
        let folder = trash::Entry { name: "folder-id".into(), original: "/original/Photos".into(), deleted: None };
        assert_eq!(item_info(&root, &folder).file_type(), gio::FileType::Directory);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trash_items_with_same_name_keep_distinct_ids() {
        let root = std::env::temp_dir().join("filescope-absent-trash-appearance");
        let a = trash::Entry { name: "notes.txt".into(), original: "/one/notes.txt".into(), deleted: None };
        let b = trash::Entry { name: "notes.txt.2".into(), original: "/two/notes.txt".into(), deleted: None };
        let first = item_info(&root, &a);
        let second = item_info(&root, &b);
        assert_eq!(first.display_name(), second.display_name());
        assert_ne!(first.name(), second.name());
        assert_eq!(entry_from_info(&first).unwrap(), a);
        assert_eq!(entry_from_info(&second).unwrap(), b);
    }
}
