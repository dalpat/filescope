//! Shared header location: tab breadcrumbs and request-owned inline editing.
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::{Rc, Weak};

use super::{App, Place, Tab, navigation};

#[derive(Clone)]
struct Editing {
    tab: Weak<Tab>,
    generation: u64,
    base: gio::File,
}
impl Editing {
    fn matches(&self, tab: &Rc<Tab>) -> bool {
        self.tab.upgrade().is_some_and(|owner| Rc::ptr_eq(&owner, tab))
            && self.generation == tab.navigation_generation.get()
            && self.base.equal(&folder(tab))
    }
}

pub(super) struct Bar {
    pub widget: gtk::Stack,
    entry: gtk::Entry,
    breadcrumbs: gtk::ScrolledWindow,
    path_view: gtk::Viewport,
    title: gtk::Label,
    editing: RefCell<Option<Editing>>,
    pending: RefCell<Option<gio::Cancellable>>,
}
impl Bar {
    pub fn new() -> Self {
        let widget = gtk::Stack::builder().hexpand(true).vhomogeneous(false).build();
        widget.set_size_request(120, -1);
        widget.set_widget_name("filescope-header-location");
        let breadcrumbs = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        let adjustment = breadcrumbs.hadjustment();
        adjustment.connect_upper_notify(scroll_to_end);
        adjustment.connect_page_size_notify(scroll_to_end);
        let path_view = gtk::Viewport::builder().hscroll_policy(gtk::ScrollablePolicy::Natural).build();
        breadcrumbs.set_child(Some(&path_view));
        let entry = gtk::Entry::builder().hexpand(true).width_chars(1).max_width_chars(60)
            .placeholder_text("Folder path").build();
        entry.set_widget_name("filescope-location-entry");
        entry.update_property(&[gtk::accessible::Property::Label("Folder path")]);
        entry.set_icon_from_icon_name(gtk::EntryIconPosition::Secondary, Some("window-close-symbolic"));
        entry.set_icon_tooltip_text(gtk::EntryIconPosition::Secondary, Some("Cancel location entry (Escape)"));
        crate::path_completion::attach(&entry);
        let title = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::End).build();
        title.add_css_class("title");
        widget.add_named(&breadcrumbs, Some("path"));
        widget.add_named(&entry, Some("entry"));
        widget.add_named(&title, Some("title"));
        Self { widget, entry, breadcrumbs, path_view, title, editing: RefCell::new(None), pending: RefCell::new(None) }
    }

    fn cancel_check(&self) {
        if let Some(cancel) = self.pending.borrow_mut().take() { cancel.cancel(); }
    }

    pub fn stop(&self) {
        self.cancel_check();
        self.editing.borrow_mut().take();
    }
}

fn scroll_to_end(adjustment: &gtk::Adjustment) {
    let weak = adjustment.downgrade();
    // GTK finishes configuring the scroll range after its property notifications.
    glib::idle_add_local_once(move || {
        if let Some(adjustment) = weak.upgrade() {
            adjustment.set_value((adjustment.upper() - adjustment.page_size()).max(0.0));
        }
    });
}

fn folder(tab: &Tab) -> gio::File {
    match navigation::here(tab) {
        Some(Place::Trash) => gio::File::for_uri("trash:///"),
        Some(Place::Folder(file) | Place::Search { root: file, .. }) => file,
        _ => gio::File::for_path(glib::home_dir()),
    }
}

pub(super) fn sync(app: &Rc<App>, tab: &Rc<Tab>) {
    let bar = &app.location;
    if bar.editing.borrow().as_ref().is_some_and(|editing| editing.matches(tab)) { return; }
    bar.stop();
    bar.entry.remove_css_class("error");
    bar.entry.set_tooltip_text(None);
    let special = match tab.view_stack.visible_child_name().as_deref() {
        Some("computer") => Some("This PC"),
        Some("trash") => Some("Trash"),
        _ => None,
    };
    if let Some(title) = special {
        bar.title.set_label(title);
        bar.widget.set_visible_child_name("title");
    } else {
        if bar.path_view.child().as_ref() != Some(tab.breadcrumb.upcast_ref()) {
            bar.path_view.set_child(Some(&tab.breadcrumb));
            scroll_to_end(&bar.breadcrumbs.hadjustment());
        }
        bar.widget.set_visible_child_name("path");
    }
}

pub(super) fn edit(app: &Rc<App>) {
    let tab = navigation::active_tab(app);
    let base = folder(&tab);
    let bar = &app.location;
    bar.stop();
    let text = base.path().and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| base.uri().to_string());
    bar.entry.set_text(&text);
    *bar.editing.borrow_mut() = Some(Editing {
        tab: Rc::downgrade(&tab), generation: tab.navigation_generation.get(), base,
    });
    bar.widget.set_visible_child_name("entry");
    bar.entry.grab_focus();
    bar.entry.select_region(0, -1);
}

fn cancel(app: &Rc<App>) {
    app.location.stop();
    let tab = navigation::active_tab(app);
    sync(app, &tab);
    tab.view_stack.child_focus(gtk::DirectionType::TabForward);
}

fn error(app: &Rc<App>, message: &str) {
    app.location.entry.add_css_class("error");
    app.location.entry.set_tooltip_text(Some(message));
    super::toast(app, message);
}

fn submit(app: &Rc<App>) {
    let bar = &app.location;
    let Some(editing) = bar.editing.borrow().clone() else { return; };
    let Some(tab) = editing.tab.upgrade() else { cancel(app); return; };
    if !editing.matches(&tab) { cancel(app); return; }
    bar.cancel_check();
    let Some(file) = crate::path_completion::resolve(bar.entry.text().as_str(), &editing.base) else {
        error(app, "Enter a local folder path");
        return;
    };
    if super::is_trash_root(&file) {
        cancel(app);
        navigation::go_trash(app, &tab);
        return;
    }
    if file.path().is_none() {
        error(app, "Enter a local folder path");
        return;
    }
    let check = gio::Cancellable::new();
    bar.pending.replace(Some(check.clone()));
    let app = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        let result = gio::CancellableFuture::new(
            file.query_info_future("standard::type", gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT),
            check.clone(),
        ).await;
        let (Some(app), Some(tab)) = (app.upgrade(), editing.tab.upgrade()) else { return; };
        if check.is_cancelled() || !app.window.is_visible()
            || !Rc::ptr_eq(&navigation::active_tab(&app), &tab)
            || !app.location.editing.borrow().as_ref().is_some_and(|current| current.matches(&tab))
            || !editing.matches(&tab)
        { return; }
        app.location.pending.borrow_mut().take();
        match result {
            Ok(Ok(info)) if info.file_type() == gio::FileType::Directory => {
                cancel(&app);
                navigation::navigate(&app, &tab, file);
            }
            Ok(Ok(_)) => error(&app, "Choose a folder, not a file"),
            Ok(Err(failure)) => error(&app, &format!("Couldn't open folder: {failure}")),
            Err(_) => {},
        }
    });
}

pub(super) fn connect(app: &Rc<App>) {
    let weak = Rc::downgrade(app);
    app.location.entry.connect_activate(move |_| {
        if let Some(app) = weak.upgrade() { submit(&app); }
    });
    let weak = Rc::downgrade(app);
    app.location.entry.connect_changed(move |entry| {
        if let Some(app) = weak.upgrade() { app.location.cancel_check(); }
        entry.remove_css_class("error");
        entry.set_tooltip_text(None);
    });
    let weak = Rc::downgrade(app);
    app.location.entry.connect_icon_release(move |_, position| {
        if position == gtk::EntryIconPosition::Secondary
            && let Some(app) = weak.upgrade() { cancel(&app); }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_name(Some("filescope-location-keys"));
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(app);
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gtk::gdk::Key::Escape && let Some(app) = weak.upgrade() {
            cancel(&app);
            glib::Propagation::Stop
        } else { glib::Propagation::Proceed }
    });
    app.location.entry.add_controller(keys);
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::{gio, glib};
    use std::time::{Duration, Instant};

    async fn until(condition: impl Fn() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(4),
                "location editor did not settle"
            );
            glib::timeout_future(Duration::from_millis(20)).await;
        }
    }

    fn fixture() -> (adw::Application, Rc<super::super::App>, std::path::PathBuf) {
        fixture_at(1100, "Sans 11")
    }

    fn fixture_at(width: i32, font: &str) -> (adw::Application, Rc<super::super::App>, std::path::PathBuf) {
        adw::init().unwrap();
        gtk::Settings::default().unwrap().set_gtk_font_name(Some(font));
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let root =
            std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join("location-entry");
        std::fs::create_dir_all(root.join("alpha child")).unwrap();
        let config = glib::user_config_dir().join("filescope");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(config.join("settings"), format!("automatic_updates=false\nwidth={width}\nheight=480\n")).unwrap();
        let application = adw::Application::builder()
            .application_id(format!("dev.filescope.LocationEntryTest{width}"))
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        super::super::open_files(&application, &[gio::File::for_path(&root)]);
        let state = super::super::app_state(&application).unwrap();
        (application, state, root)
    }



    fn press_escape(entry: &gtk::Entry) {
        use glib::translate::IntoGlib;
        let controllers = entry.observe_controllers();
        let keys = (0..controllers.n_items()).find_map(|index| {
            controllers.item(index).and_downcast::<gtk::EventControllerKey>()
                .filter(|keys| keys.name().as_deref() == Some("filescope-location-keys"))
        }).unwrap();
        let key: u32 = gtk::gdk::Key::Escape.into_glib();
        assert!(keys.emit_by_name::<bool>("key-pressed", &[&key, &0u32, &gtk::gdk::ModifierType::empty()]));
    }

    fn in_header(widget: &impl IsA<gtk::Widget>) -> bool {
        let mut parent = widget.as_ref().parent();
        while let Some(widget) = parent {
            if widget.is::<adw::HeaderBar>() { return true; }
            parent = widget.parent();
        }
        false
    }

    fn capture(window: &adw::ApplicationWindow, name: &str) {
        let Some(directory) = std::env::var_os("FILESCOPE_PLACES_SHOTS") else { return; };
        let widget = window.content().unwrap();
        let parent = widget.parent().unwrap();
        let snapshot = gtk::Snapshot::new();
        #[allow(deprecated)]
        let background = widget.style_context().lookup_color("window_bg_color").unwrap();
        snapshot.append_color(&background, &gtk::graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32));
        parent.snapshot_child(&widget, &snapshot);
        window.renderer().unwrap().render_texture(&snapshot.to_node().unwrap(), None)
            .save_to_png(std::path::PathBuf::from(directory).join(format!("{name}.png"))).unwrap();
    }

    async fn settle_layout(window: &adw::ApplicationWindow) {
        // Broadway has no visible client driving frames in this isolated runner.
        for _ in 0..3 {
            window.allocate(window.width(), window.height(), -1, None);
            glib::timeout_future(Duration::from_millis(30)).await;
        }
        window.allocate(window.width(), window.height(), -1, None);
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn location_editor_opens_exact_paths_and_releases_request() {
        let (application, state, root) = fixture();
        glib::MainContext::default().block_on(async {
            until(|| !navigation::active_tab(&state).dir_list.is_loading()).await;
            assert!(!application.accels_for_action("win.location").is_empty());
            state.window.lookup_action("location").unwrap().activate(None);
            let entry = &state.location.entry;
            assert!(state.window.visible_dialog().is_none(), "Ctrl+L opened a dialog");
            assert!(in_header(entry));
            assert_eq!(entry.selection_bounds(), Some((0, entry.text().chars().count() as i32)));
            entry.set_text("alpha child");
            entry.emit_activate();
            until(|| state.location.editing.borrow().is_none()).await;
            assert!(navigation::active_tab(&state).current_dir().unwrap().equal(&gio::File::for_path(root.join("alpha child"))));
            assert!(in_header(&navigation::active_tab(&state).breadcrumb));
            edit(&state);
            entry.set_text(&gio::File::for_path(&root).uri().replacen("file:", "FILE:", 1));
            entry.emit_activate();
            until(|| state.location.editing.borrow().is_none()).await;
            assert!(navigation::active_tab(&state).current_dir().unwrap().equal(&gio::File::for_path(&root)));
            edit(&state);
            entry.set_text("TRASH:///");
            entry.emit_activate();
            assert_eq!(navigation::active_tab(&state).view_stack.visible_child_name().as_deref(), Some("trash"));
            assert_eq!(state.location.widget.visible_child_name().as_deref(), Some("title"));
            edit(&state);
            assert_eq!(entry.text(), "trash:///");
            state.window.close();
            assert!(state.location.editing.borrow().is_none());
            assert!(state.location.pending.borrow().is_none());
        });
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn inline_location_cancels_on_escape_and_tab_switch() {
        let (_application, state, root) = fixture();
        glib::MainContext::default().block_on(async {
            let first = navigation::active_tab(&state);
            until(|| !first.dir_list.is_loading()).await;
            edit(&state);
            state.location.entry.set_text("alpha child");
            press_escape(&state.location.entry);
            assert_eq!(state.location.widget.visible_child_name().as_deref(), Some("path"));
            assert!(!super::super::view::focus_is_text_input(state.window.upcast_ref()));
            assert!(first.current_dir().unwrap().equal(&gio::File::for_path(&root)));
            edit(&state);
            state.location.entry.set_text("alpha child");
            state.location.entry.emit_activate();
            navigation::new_tab(&state, gio::File::for_path(&root), true);
            let second = navigation::active_tab(&state);
            until(|| !second.dir_list.is_loading() && state.location.pending.borrow().is_none()).await;
            assert!(state.location.editing.borrow().is_none());
            assert!(first.current_dir().unwrap().equal(&gio::File::for_path(&root)));
            assert!(second.current_dir().unwrap().equal(&gio::File::for_path(&root)));
            assert!(second.breadcrumb.is_ancestor(&state.location.breadcrumbs));
            assert!(!first.breadcrumb.is_ancestor(&state.location.breadcrumbs));
            edit(&state);
            state.location.entry.set_text("alpha child");
            state.location.entry.emit_activate();
            state.location.entry.set_text("unsubmitted edit");
            glib::timeout_future(Duration::from_millis(80)).await;
            assert!(second.current_dir().unwrap().equal(&gio::File::for_path(&root)));
            press_escape(&state.location.entry);
            state.window.close();
        });
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn inline_location_rejects_invalid_targets() {
        let (_application, state, root) = fixture();
        std::fs::write(root.join("notes.txt"), "not a directory").unwrap();
        glib::MainContext::default().block_on(async {
            for value in ["missing-folder", "notes.txt", "sftp://example.test/folder"] {
                edit(&state);
                state.location.entry.set_text(value);
                state.location.entry.emit_activate();
                until(|| state.location.pending.borrow().is_none()).await;
                assert!(state.location.entry.has_css_class("error"));
                assert_eq!(state.location.widget.visible_child_name().as_deref(), Some("entry"));
                assert!(navigation::active_tab(&state).current_dir().unwrap().equal(&gio::File::for_path(&root)));
                assert!(state.window.visible_dialog().is_none());
            }
            state.location.entry.set_text("alpha child");
            assert!(!state.location.entry.has_css_class("error"));
            state.location.entry.emit_activate();
            until(|| state.location.editing.borrow().is_none()).await;
            state.window.close();
        });
    }

    fn menu_contains(menu: &gio::MenuModel, action: &str) -> bool {
        (0..menu.n_items()).any(|index| {
            menu.item_attribute_value(index, "action", None).and_then(|value| value.get::<String>()).as_deref() == Some(action)
                || ["section", "submenu"].iter().any(|link| menu.item_link(index, link).is_some_and(|child| menu_contains(&child, action)))
        })
    }

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut widgets = vec![widget.clone()];
        let mut child = widget.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            widgets.extend(descendants(&widget));
        }
        widgets
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn compact_header_keeps_controls_and_long_paths_accessible() {
        for (width, font) in [(1100, "Sans 11"), (640, "Sans 11"), (760, "Sans 18")] {
        let (_application, state, root) = fixture_at(width, font);
        let long = (0..12).fold(root.clone(), |path, index| path.join(format!("long-folder-name-{index}")));
        std::fs::create_dir_all(&long).unwrap();
        let tab = navigation::active_tab(&state);
        glib::MainContext::default().block_on(async {
            navigation::navigate(&state, &tab, gio::File::for_path(&long));
            until(|| !tab.dir_list.is_loading()).await;
            let widgets = descendants(state.window.upcast_ref());
            let view = widgets.iter().find(|widget| widget.widget_name() == "filescope-view-options")
                .unwrap().downcast_ref::<gtk::MenuButton>().unwrap();
            let menu = view.menu_model().unwrap();
            for action in ["win.view-mode", "win.sort", "win.sort-descending", "win.zoom-in", "win.zoom-out"] {
                assert!(menu_contains(&menu, action), "missing {action}");
            }
            for action in ["win.new-tab", "win.up", "win.location"] {
                assert!(menu_contains(&super::super::primary_menu().upcast(), action));
            }
            for tooltip in ["Up (Alt+Up)", "New tab (Ctrl+T)", "Zoom out (Ctrl+-)", "Zoom in (Ctrl++)", "Sort by"] {
                assert!(!widgets.iter().any(|widget| widget.tooltip_text().as_deref() == Some(tooltip)), "extra header button: {tooltip}");
            }
                settle_layout(&state.window).await;
                assert!(in_header(&tab.breadcrumb));
                let bounds = state.location.widget.compute_bounds(&state.window).unwrap();
                capture(&state.window, &format!("header-{width}"));
                assert!(bounds.width() >= 100.0 && bounds.x() >= 0.0 && bounds.x() + bounds.width() <= state.window.width() as f32,
                    "requested {width}, actual {}, location {bounds:?}", state.window.width());
                let last = tab.breadcrumb.last_child().unwrap().compute_bounds(&state.location.path_view).unwrap();
                assert!(last.x() >= 0.0 && last.x() + last.width() <= state.location.path_view.width() as f32 + 1.0,
                    "current folder is clipped: {last:?}");
                edit(&state);
                settle_layout(&state.window).await;
                let bounds = state.location.entry.compute_bounds(&state.window).unwrap();
                capture(&state.window, &format!("entry-{width}"));
                assert!(bounds.width() >= 100.0 && bounds.x() + bounds.width() <= state.window.width() as f32,
                    "requested {width}, actual {}, entry {bounds:?}", state.window.width());
                assert!(state.window.visible_dialog().is_none());
                press_escape(&state.location.entry);
            state.window.close();
        });
        }
    }
}
