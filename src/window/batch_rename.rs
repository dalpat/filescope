use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::batch_rename::{self as batch, Item, Plan, Row, Rule};

struct State {
    window: glib::WeakRef<adw::Window>,
    app: Weak<super::App>,
    items: RefCell<Vec<Item>>,
    plan: RefCell<Option<Plan>>,
    generation: Cell<u64>,
    busy: Cell<bool>,
    closed: Cell<bool>,
    controls: gtk::Box,
    template_mode: gtk::CheckButton,
    template: gtk::Entry,
    find: gtk::Entry,
    replacement: gtk::Entry,
    extensions: gtk::CheckButton,
    order: gtk::DropDown,
    model: gio::ListStore,
    status: gtk::Label,
    apply: gtk::Button,
    cancel: gtk::Button,
}

pub(super) fn show(app: &Rc<super::App>, paths: Vec<PathBuf>) {
    build(app, paths);
}

fn build(app: &Rc<super::App>, paths: Vec<PathBuf>) -> Rc<State> {
    let window = adw::Window::builder()
        .title(format!("Rename {} Items", paths.len()))
        .transient_for(&app.window)
        .modal(true)
        .default_width(780)
        .default_height(560)
        .build();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    let cancel = gtk::Button::with_label("Cancel");
    let apply = gtk::Button::with_label("Rename");
    apply.add_css_class("suggested-action");
    apply.set_sensitive(false);
    header.pack_start(&cancel);
    header.pack_end(&apply);
    window.set_default_widget(Some(&apply));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_start(18)
        .margin_end(18)
        .margin_top(12)
        .margin_bottom(18)
        .vexpand(true)
        .build();
    content.append(&body);
    window.set_content(Some(&content));

    let controls = gtk::Box::new(gtk::Orientation::Vertical, 10);
    let modes = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let template_mode = gtk::CheckButton::with_label("Template");
    let replace_mode = gtk::CheckButton::with_label("Find and replace");
    replace_mode.set_group(Some(&template_mode));
    template_mode.set_active(true);
    modes.append(&template_mode);
    modes.append(&replace_mode);
    controls.append(&modes);
    let stack = gtk::Stack::new();
    let template_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let template = gtk::Entry::builder()
        .text(batch::NAME)
        .activates_default(true)
        .hexpand(true)
        .build();
    template.update_property(&[gtk::accessible::Property::Label("Name template")]);
    template_row.append(&template);
    let add = gtk::MenuButton::builder().label("Add…").build();
    let popover = gtk::Popover::new();
    let tokens = gtk::Box::new(gtk::Orientation::Vertical, 4);
    for token in [
        batch::NAME,
        batch::NUMBER_1,
        batch::NUMBER_2,
        batch::NUMBER_3,
    ] {
        let button = gtk::Button::with_label(token);
        let entry = template.clone();
        let popover = popover.downgrade();
        button.connect_clicked(move |_| {
            entry.delete_selection();
            let mut position = entry.position();
            entry.insert_text(token, &mut position);
            entry.set_position(position);
            entry.grab_focus();
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
        });
        tokens.append(&button);
    }
    popover.set_child(Some(&tokens));
    add.set_popover(Some(&popover));
    template_row.append(&add);
    stack.add_named(&template_row, Some("template"));
    let replace_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let find = gtk::Entry::builder()
        .placeholder_text("Find")
        .activates_default(true)
        .hexpand(true)
        .build();
    let replacement = gtk::Entry::builder()
        .placeholder_text("Replace with")
        .activates_default(true)
        .hexpand(true)
        .build();
    find.update_property(&[gtk::accessible::Property::Label("Find")]);
    replacement.update_property(&[gtk::accessible::Property::Label("Replace with")]);
    replace_row.append(&find);
    replace_row.append(&replacement);
    stack.add_named(&replace_row, Some("replace"));
    stack.set_visible_child_name("template");
    controls.append(&stack);
    let options = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let extensions = gtk::CheckButton::with_label("Preserve file extensions");
    extensions.set_active(true);
    extensions.set_hexpand(true);
    options.append(&extensions);
    options.append(&gtk::Label::new(Some("Order")));
    let order = gtk::DropDown::from_strings(&[
        "Current order",
        "Name (A–Z)",
        "Name (Z–A)",
        "First modified",
        "Last modified",
    ]);
    order.update_property(&[gtk::accessible::Property::Label("Numbering order")]);
    options.append(&order);
    controls.append(&options);
    body.append(&controls);
    let model = gio::ListStore::new::<glib::BoxedAnyObject>();
    let preview = gtk::ColumnView::new(Some(gtk::NoSelection::new(Some(model.clone()))));
    for (title, field) in [("Original name", 0), ("New name", 1), ("Problem", 2)] {
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, object| {
            let item = object.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(Some(
                &gtk::Label::builder()
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .build(),
            ));
        });
        factory.connect_bind(move |_, object| {
            let item = object.downcast_ref::<gtk::ListItem>().unwrap();
            let object = item.item().and_downcast::<glib::BoxedAnyObject>().unwrap();
            let row = object.borrow::<Row>();
            let label = item.child().and_downcast::<gtk::Label>().unwrap();
            label.set_text(match field {
                0 => &row.item.name,
                1 => &row.name,
                _ => row.error.as_deref().unwrap_or(""),
            });
            let path = if field == 1 {
                row.item.path.with_file_name(&row.name)
            } else {
                row.item.path.clone()
            };
            label.set_tooltip_text(Some(&format!(
                "{}{}",
                path.display(),
                row.error
                    .as_ref()
                    .map(|e| format!("\n{e}"))
                    .unwrap_or_default()
            )));
            if row.error.is_some() {
                label.add_css_class("error");
            } else {
                label.remove_css_class("error");
            }
        });
        let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
        column.set_expand(true);
        column.set_resizable(true);
        preview.append_column(&column);
    }
    body.append(
        &gtk::ScrolledWindow::builder()
            .child(&preview)
            .vexpand(true)
            .min_content_height(180)
            .build(),
    );
    let status = gtk::Label::builder()
        .label("Reading selected files…")
        .xalign(0.0)
        .wrap(true)
        .build();
    body.append(&status);
    let state = Rc::new(State {
        window: window.downgrade(),
        app: Rc::downgrade(app),
        items: RefCell::new(Vec::new()),
        plan: RefCell::new(None),
        generation: Cell::new(0),
        busy: Cell::new(false),
        closed: Cell::new(false),
        controls,
        template_mode,
        template,
        find,
        replacement,
        extensions,
        order,
        model,
        status,
        apply,
        cancel,
    });
    let keep = state.clone();
    window.connect_close_request(move |_| {
        if keep.busy.get() {
            return glib::Propagation::Stop;
        }
        keep.closed.set(true);
        glib::Propagation::Proceed
    });
    let weak = window.downgrade();
    state.cancel.connect_clicked(move |_| {
        if let Some(window) = weak.upgrade() {
            window.close();
        }
    });
    let weak = Rc::downgrade(&state);
    state.template_mode.connect_toggled(move |button| {
        stack.set_visible_child_name(if button.is_active() {
            "template"
        } else {
            "replace"
        });
        if let Some(state) = weak.upgrade() {
            state.preview();
        }
    });
    for entry in [&state.template, &state.find, &state.replacement] {
        let weak = Rc::downgrade(&state);
        entry.connect_changed(move |_| {
            if let Some(state) = weak.upgrade() {
                state.preview();
            }
        });
    }
    let weak = Rc::downgrade(&state);
    state.extensions.connect_toggled(move |_| {
        if let Some(state) = weak.upgrade() {
            state.preview();
        }
    });
    let weak = Rc::downgrade(&state);
    state.order.connect_selected_notify(move |_| {
        if let Some(state) = weak.upgrade() {
            state.preview();
        }
    });
    let weak = Rc::downgrade(&state);
    state.apply.connect_clicked(move |_| {
        if let Some(state) = weak.upgrade() {
            state.apply();
        }
    });
    let weak = Rc::downgrade(&state);
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || {
            paths
                .into_iter()
                .map(|path| {
                    Item::read(path.clone()).map_err(|e| format!("{}: {e}", path.display()))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .await;
        let Some(state) = weak.upgrade().filter(|s| !s.closed.get()) else {
            return;
        };
        match result {
            Ok(Ok(items)) => {
                *state.items.borrow_mut() = items;
                state.preview();
            }
            _ => state.status.set_text(&match result {
                Ok(Err(e)) => e,
                _ => "Couldn't read the selected files".into(),
            }),
        }
    });
    let keys = gtk::EventControllerKey::new();
    let weak = Rc::downgrade(&state);
    keys.connect_key_pressed(move |_, key, _, _| {
        if key == gdk::Key::Escape {
            if let Some(state) = weak.upgrade().filter(|s| !s.busy.get()) {
                if let Some(window) = state.window.upgrade() {
                    window.close();
                }
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    window.present();
    state.template.grab_focus();
    state.template.select_region(0, -1);
    state
}

fn sort_items(items: &mut [Item], order: u32) {
    match order {
        1 => items.sort_by_cached_key(|item| glib::FilenameCollationKey::from(&item.name)),
        2 => items.sort_by_cached_key(|item| {
            std::cmp::Reverse(glib::FilenameCollationKey::from(&item.name))
        }),
        3 => items.sort_by_key(Item::modified),
        4 => items.sort_by_key(|item| std::cmp::Reverse(item.modified())),
        _ => {}
    }
}

impl State {
    fn preview(self: &Rc<Self>) {
        if self.busy.get() || self.closed.get() || self.items.borrow().is_empty() {
            return;
        }
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        self.apply.set_sensitive(false);
        self.plan.borrow_mut().take();
        self.status.set_text("Checking names…");
        let mut items = self.items.borrow().clone();
        sort_items(&mut items, self.order.selected());
        let rule = if self.template_mode.is_active() {
            Rule::Template(self.template.text().into())
        } else {
            Rule::Replace {
                find: self.find.text().into(),
                replacement: self.replacement.text().into(),
            }
        };
        let extensions = self.extensions.is_active();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            glib::timeout_future(std::time::Duration::from_millis(120)).await;
            if !weak
                .upgrade()
                .is_some_and(|s| !s.closed.get() && s.generation.get() == generation)
            {
                return;
            }
            let result =
                gio::spawn_blocking(move || batch::prepare(&items, &rule, extensions)).await;
            let Some(state) = weak
                .upgrade()
                .filter(|s| !s.closed.get() && s.generation.get() == generation)
            else {
                return;
            };
            let Ok(plan) = result else {
                state.status.set_text("Couldn't prepare the preview");
                return;
            };
            let changed = plan
                .rows
                .iter()
                .filter(|row| row.name != row.item.name)
                .count();
            let errors = plan.rows.iter().filter(|row| row.error.is_some()).count();
            state.status.set_text(&if errors > 0 {
                format!("{errors} items need attention. No files will be changed.")
            } else {
                format!("{changed} of {} items will be renamed", plan.rows.len())
            });
            let rows: Vec<_> = plan
                .rows
                .iter()
                .cloned()
                .map(glib::BoxedAnyObject::new)
                .collect();
            state.model.splice(0, state.model.n_items(), &rows);
            state.apply.set_sensitive(plan.ready());
            *state.plan.borrow_mut() = Some(plan);
        });
    }

    fn apply(self: &Rc<Self>) {
        if self.busy.get() || self.closed.get() {
            return;
        }
        let Some(plan) = self.plan.borrow().clone().filter(Plan::ready) else {
            return;
        };
        let Some(app) = self.app.upgrade() else {
            return;
        };
        self.busy.set(true);
        self.controls.set_sensitive(false);
        self.apply.set_sensitive(false);
        self.cancel.set_sensitive(false);
        if let Some(window) = self.window.upgrade() {
            window.set_deletable(false);
        }
        self.status.set_text("Renaming…");
        let hold = super::navigation::BatchRename::new(&app, &plan.requests());
        let state = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || plan.apply()).await;
            hold.finish(&app, result.as_ref().ok().and_then(|result| result.as_ref().ok()));
            state.busy.set(false);
            state.controls.set_sensitive(true);
            state.cancel.set_sensitive(true);
            if let Some(window) = state.window.upgrade() {
                window.set_deletable(true);
            }
            match result {
                Ok(Ok(outcome)) => {
                    if !outcome.undo.is_empty() {
                        super::commands::push_undo(
                            &app,
                            crate::undo::Action::BatchRename {
                                pending: outcome.undo.clone(),
                            },
                        );
                    }
                    super::commands::refresh_all(&app);
                    if let Some(window) = state.window.upgrade() {
                        window.close();
                    }
                    report(&app, &outcome, "Renamed");
                }
                result => {
                    state.plan.borrow_mut().take();
                    state.status.set_text(&match result {
                        Ok(Err(e)) => format!(
                            "No files changed. {e}. Check the preview or reselect the files."
                        ),
                        _ => "Couldn't finish renaming. Check the files before retrying.".into(),
                    });
                }
            }
        });
    }
}

pub(super) fn report(app: &Rc<super::App>, outcome: &batch::Outcome, verb: &str) {
    let summary = format!("{verb} {} items", outcome.undo.len());
    if outcome.failed.is_empty() {
        super::toast(app, &summary);
        return;
    }
    let errors = outcome
        .failed
        .iter()
        .map(|(request, error)| format!("{}: {error}", request.from.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let dialog = adw::AlertDialog::new(
        Some(&format!("{summary}; {} failed", outcome.failed.len())),
        Some(&errors),
    );
    dialog.set_body_use_markup(false);
    dialog.add_response("close", "Close");
    dialog.set_close_response("close");
    dialog.present(Some(&app.window));
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn preview_ready(state: &Rc<State>) {
        for _ in 0..100 {
            if state.plan.borrow().is_some() {
                return;
            }
            glib::timeout_future(std::time::Duration::from_millis(25)).await;
        }
        panic!("preview never completed: {}", state.status.text());
    }

    async fn capture(state: &Rc<State>, name: &str) {
        let Some(dir) = std::env::var_os("FILESCOPE_BATCH_SHOTS") else {
            return;
        };
        glib::timeout_future(std::time::Duration::from_millis(200)).await;
        let window = state.window.upgrade().unwrap();
        let snapshot = gtk::Snapshot::new();
        #[allow(deprecated)] // GTK 4.16 exposes named theme colors through StyleContext.
        let background = window
            .style_context()
            .lookup_color("window_bg_color")
            .unwrap();
        snapshot.append_color(
            &background,
            &gtk::graphene::Rect::new(0.0, 0.0, window.width() as f32, window.height() as f32),
        );
        let child = window.content().unwrap();
        // An unattended Broadway session may not deliver frame-clock ticks.
        child.allocate(window.width(), window.height(), -1, None);
        child.parent().unwrap().snapshot_child(&child, &snapshot);
        let node = snapshot.to_node().expect("dialog has no rendered content");
        let renderer = window.native().unwrap().renderer().unwrap();
        renderer
            .render_texture(&node, None)
            .save_to_png(PathBuf::from(dir).join(format!("{name}.png")))
            .unwrap();
    }

    async fn history_ready(app: &Rc<super::super::App>) {
        for _ in 0..200 {
            if ["undo", "redo"]
                .iter()
                .any(|name| app.window.lookup_action(name).unwrap().is_enabled())
            {
                return;
            }
            glib::timeout_future(std::time::Duration::from_millis(25)).await;
        }
        panic!("batch history did not complete");
    }

    fn find_label(widget: &gtk::Widget, text: &str) -> Option<gtk::Label> {
        if let Some(label) = widget
            .downcast_ref::<gtk::Label>()
            .filter(|label| label.text() == text)
        {
            return Some(label.clone());
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if let Some(label) = find_label(&widget, text) {
                return Some(label);
            }
            child = widget.next_sibling();
        }
        None
    }

    async fn batch_settle() {
        glib::timeout_future(std::time::Duration::from_millis(200)).await;
    }

    async fn batch_continuity(mode: super::super::ViewMode) {
        use super::super::{self as window, commands, navigation};
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        gtk::Settings::default().unwrap().set_gtk_enable_animations(false);
        let application = adw::Application::builder().application_id("dev.filescope.BatchContinuity")
            .flags(gio::ApplicationFlags::NON_UNIQUE).build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!("filescope-batch-continuity-{}", std::process::id()));
        for name in ["A", "B"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("keep.txt"), name).unwrap();
        }
        window::open_files(&application, &[gio::File::for_path(&root)]);
        let app = window::app_state(&application).unwrap();
        window::set_view_mode(&app, mode);
        let parent = navigation::active_tab(&app);
        batch_settle().await;
        window::visible_selection(&parent).select_all();
        navigation::new_tab(&app, gio::File::for_path(root.join("A")), true);
        let child = navigation::active_tab(&app);
        batch_settle().await;
        navigation::select_names(&child, &["keep.txt".into()], || {});
        let model = window::visible_selection(&child);
        let row = model.item(0).unwrap();
        let child_widget = if mode == window::ViewMode::Columns { child.columns.view().unwrap().upcast::<gtk::Widget>() }
            else if mode == window::ViewMode::List { child.column_view.clone().upcast() } else { child.grid_view.clone().upcast() };
        app.tab_view.set_selected_page(parent.page.borrow().as_ref().unwrap());
        let state = build(&app, vec![root.join("A"), root.join("B")]);
        state.template.set_text(&format!("renamed{}", batch::NUMBER_3));
        preview_ready(&state).await;
        assert!(state.plan.borrow().as_ref().unwrap().ready());
        state.apply();
        while state.busy.get() { batch_settle().await; }
        batch_settle().await;
        let check = |name: &str| {
            assert_eq!(child.current_dir().unwrap().path().unwrap(), root.join(name), "open descendant did not follow batch rename");
            assert_eq!(window::visible_selection(&child), model);
            assert_eq!(model.item(0).unwrap(), row, "unchanged child row was replaced");
            assert_eq!(commands::selected(&child)[0].0.path().unwrap(), root.join(name).join("keep.txt"));
            if mode == window::ViewMode::Columns { assert_eq!(child.columns.view().unwrap().upcast::<gtk::Widget>(), child_widget); }
            assert!(Rc::ptr_eq(&navigation::active_tab(&app), &parent), "batch changed active tab");
        };
        check("renamed001");
        assert!(find_label(parent.view_stack.upcast_ref(), "renamed001").is_some(), "batch row label stayed stale");
        assert!(find_label(parent.view_stack.upcast_ref(), "renamed002").is_some());
        commands::undo(&app);
        while app.history_busy.get() { batch_settle().await; }
        batch_settle().await;
        check("A");
        commands::redo(&app);
        while app.history_busy.get() { batch_settle().await; }
        batch_settle().await;
        check("renamed001");
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_grid_apply_undo_redo_preserve_open_descendants() {
        glib::MainContext::default().block_on(batch_continuity(super::super::ViewMode::Grid));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_list_apply_undo_redo_preserve_open_descendants() {
        glib::MainContext::default().block_on(batch_continuity(super::super::ViewMode::List));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_columns_apply_undo_redo_preserve_open_descendants() {
        glib::MainContext::default().block_on(batch_continuity(super::super::ViewMode::Columns));
    }

    async fn batch_boundary_fixture() -> (adw::Application, Rc<super::super::App>, Rc<super::super::Tab>, PathBuf) {
        use super::super::{self as window, navigation};
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        gtk::Settings::default().unwrap().set_gtk_enable_animations(false);
        let application = adw::Application::builder().application_id("dev.filescope.BatchBoundary")
            .flags(gio::ApplicationFlags::NON_UNIQUE).build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!("filescope-batch-boundary-{}", std::process::id()));
        for name in ["A", "B", "Other"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("keep.txt"), name).unwrap();
        }
        std::os::unix::fs::symlink(&root, root.join("alias")).unwrap();
        window::open_files(&application, &[gio::File::for_path(&root)]);
        let app = window::app_state(&application).unwrap();
        window::set_view_mode(&app, window::ViewMode::Columns);
        let parent = navigation::active_tab(&app);
        batch_settle().await;
        (application, app, parent, root)
    }

    async fn open_selected_child(app: &Rc<super::super::App>, path: &std::path::Path) -> Rc<super::super::Tab> {
        use super::super::navigation;
        navigation::new_tab(app, gio::File::for_path(path), true);
        let tab = navigation::active_tab(app);
        batch_settle().await;
        navigation::select_names(&tab, &["keep.txt".into()], || {});
        tab
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_history_maps_chains_once_and_preserves_partial_failures() {
        use super::super::{self as window, commands, navigation};
        glib::MainContext::default().block_on(async {
            let (_application, app, parent, root) = batch_boundary_fixture().await;
            let a = open_selected_child(&app, &root.join("alias/A")).await;
            let b = open_selected_child(&app, &root.join("alias/B")).await;
            let a_row = window::visible_selection(&a).item(0).unwrap();
            let b_row = window::visible_selection(&b).item(0).unwrap();
            app.tab_view.set_selected_page(parent.page.borrow().as_ref().unwrap());
            navigation::select_names(&parent, &["A".into(), "B".into()], || {});
            let items = ["B", "A"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
            let mut plan = batch::prepare(&items, &Rule::Template(format!("unused{}", batch::NUMBER_3)), false);
            plan.rows[0].name = "C".into();
            plan.rows[1].name = "B".into();
            // The existing history worker can execute a vacancy chain with no replacement.
            commands::push_undo(&app, crate::undo::Action::BatchRename { pending: plan.requests() });
            commands::undo(&app);
            while app.history_busy.get() { batch_settle().await; }
            batch_settle().await;
            assert_eq!(a.current_dir().unwrap().path().unwrap(), root.join("alias/B"));
            assert_eq!(b.current_dir().unwrap().path().unwrap(), root.join("alias/C"));
            assert_eq!(window::visible_selection(&a).item(0).unwrap(), a_row);
            assert_eq!(window::visible_selection(&b).item(0).unwrap(), b_row);
            assert_eq!(commands::selected(&a)[0].0.path().unwrap(), root.join("alias/B/keep.txt"));
            assert_eq!(std::fs::read_to_string(root.join("B/keep.txt")).unwrap(), "A");
            assert_eq!(std::fs::read_to_string(root.join("C/keep.txt")).unwrap(), "B");
            assert_eq!(commands::selected(&parent).len(), 2);
            // C->B fails while B exists; B->A succeeds. Only that success may move the UI.
            commands::redo(&app);
            while app.history_busy.get() { batch_settle().await; }
            batch_settle().await;
            assert_eq!(a.current_dir().unwrap().path().unwrap(), root.join("alias/A"));
            assert_eq!(b.current_dir().unwrap().path().unwrap(), root.join("alias/C"));
            assert!(!app.redo_stack.borrow().is_empty(), "failed history work must remain retryable");
            commands::redo(&app);
            while app.history_busy.get() { batch_settle().await; }
            batch_settle().await;
            assert_eq!(b.current_dir().unwrap().path().unwrap(), root.join("alias/B"));
            assert_eq!(commands::selected(&b)[0].0.path().unwrap(), root.join("alias/B/keep.txt"));
            app.window.close();
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_rejected_swaps_release_held_views_without_changes() {
        use super::super::{self as window, commands};
        glib::MainContext::default().block_on(async {
            let (_application, app, parent, root) = batch_boundary_fixture().await;
            for destinations in [["B", "A"], ["B", "Other"]] {
                let state = build(&app, vec![root.join("A"), root.join("B")]);
                state.template.set_text(&format!("unused{}", batch::NUMBER_3));
                preview_ready(&state).await;
                for (row, destination) in state.plan.borrow_mut().as_mut().unwrap().rows.iter_mut().zip(destinations) {
                    row.name = destination.into();
                }
                state.apply();
                while state.busy.get() { batch_settle().await; }
                batch_settle().await;
                assert!(state.status.text().contains("No files changed"));
                assert!(!window::navigation::batch_busy(&parent));
                assert!(app.undo_stack.borrow().is_empty());
                assert_eq!(std::fs::read_to_string(root.join("A/keep.txt")).unwrap(), "A");
                assert_eq!(std::fs::read_to_string(root.join("B/keep.txt")).unwrap(), "B");
                window::navigation::select_names(&parent, &["A".into()], || {});
                assert_eq!(commands::selected(&parent)[0].1.name(), std::path::Path::new("A"));
                state.window.upgrade().unwrap().close();
            }
            app.window.close();
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_holds_preserve_newer_navigation_and_rebase_history() {
        use super::super::{self as window, commands, navigation};
        glib::MainContext::default().block_on(async {
            let (_application, app, parent, root) = batch_boundary_fixture().await;
            let child = open_selected_child(&app, &root.join("A")).await;
            let items = ["A", "B"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
            let plan = batch::prepare(&items, &Rule::Template(format!("renamed{}", batch::NUMBER_3)), false);
            let hold = navigation::BatchRename::new(&app, &plan.requests());
            assert!(navigation::batch_busy(&child));
            assert!(commands::selected(&child).is_empty(), "held paths remain actionable");
            assert!(!app.window.lookup_action("new-folder").unwrap().is_enabled());
            let nested = navigation::BatchRename::new(&app, &plan.requests());
            nested.finish(&app, None);
            assert!(navigation::batch_busy(&child), "one completion released another batch's hold");
            navigation::navigate(&app, &child, gio::File::for_path(root.join("Other")));
            batch_settle().await;
            assert!(!navigation::batch_busy(&child));
            navigation::select_names(&child, &["keep.txt".into()], || {});
            let keep = commands::selected(&child)[0].0.clone();
            let outcome = gio::spawn_blocking(move || plan.apply().unwrap()).await.unwrap();
            hold.finish(&app, Some(&outcome));
            batch_settle().await;
            assert_eq!(child.current_dir().unwrap().path().unwrap(), root.join("Other"));
            assert!(commands::selected(&child)[0].0.equal(&keep));
            assert!(matches!(child.back.borrow().last(), Some(window::Place::Folder(file)) if file.path() == Some(root.join("renamed001"))));
            assert!(!navigation::batch_busy(&parent));
            navigation::go_back(&app, &child);
            batch_settle().await;
            assert_eq!(child.current_dir().unwrap().path().unwrap(), root.join("renamed001"));
            let items = ["renamed001", "renamed002"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
            let plan = batch::prepare(&items, &Rule::Template(format!("next{}", batch::NUMBER_3)), false);
            let hold = navigation::BatchRename::new(&app, &plan.requests());
            let weak = Rc::downgrade(&child);
            app.tab_view.close_page(child.page.borrow().as_ref().unwrap());
            drop(child);
            hold.finish(&app, None);
            batch_settle().await;
            assert!(weak.upgrade().is_none(), "batch hold retained a closed tab");
            assert!(!navigation::batch_busy(&parent));
            app.window.close();
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_alias_navigation_and_search_scope_follow_successful_renames() {
        use super::super::{self as window, commands, navigation};
        glib::MainContext::default().block_on(async {
            let (_application, app, parent, root) = batch_boundary_fixture().await;
            std::os::unix::fs::symlink(root.join("A"), root.join("shortcut")).unwrap();
            let alias = open_selected_child(&app, &root.join("alias/A")).await;
            let shortcut = open_selected_child(&app, &root.join("shortcut")).await;
            navigation::new_tab(&app, gio::File::for_path(root.join("A")), true);
            let search = navigation::active_tab(&app);
            batch_settle().await;
            search.search_bar.set_search_mode(true);
            search.search_entry.set_text("keep");
            window::search::changed(&app, &search, true);
            app.tab_view.set_selected_page(parent.page.borrow().as_ref().unwrap());
            let state = build(&app, vec![root.join("alias/A"), root.join("alias/B")]);
            state.template.set_text(&format!("renamed{}", batch::NUMBER_3));
            preview_ready(&state).await;
            state.apply();
            while state.busy.get() { batch_settle().await; }
            batch_settle().await;
            assert_eq!(alias.current_dir().unwrap().path().unwrap(), root.join("alias/renamed001"));
            assert_eq!(commands::selected(&alias)[0].0.path().unwrap(), root.join("alias/renamed001/keep.txt"));
            assert_eq!(shortcut.current_dir().unwrap().path().unwrap(), root.join("renamed001"));
            assert_eq!(commands::selected(&shortcut)[0].0.path().unwrap(), root.join("renamed001/keep.txt"));
            assert_eq!(search.dir_list.file().unwrap().path().unwrap(), root.join("renamed001"));
            assert!(search.search.active.get());
            assert_eq!(&*search.search.query.borrow(), "keep");
            assert!(std::fs::symlink_metadata(root.join("shortcut")).unwrap().file_type().is_symlink());
            let names: Vec<_> = (0..window::visible_selection(&parent).n_items()).filter_map(|i| window::visible_selection(&parent).item(i)
                .and_downcast::<gio::FileInfo>().map(|info| info.name())).collect();
            assert_eq!(names.iter().filter(|name| **name == PathBuf::from("shortcut")).count(), 1, "the symlink itself was renamed in the model");
            assert_eq!(names.iter().filter(|name| **name == PathBuf::from("renamed001")).count(), 1, "alias mapping duplicated a directory row");
            commands::undo(&app);
            while app.history_busy.get() { batch_settle().await; }
            batch_settle().await;
            assert_eq!(alias.current_dir().unwrap().path().unwrap(), root.join("alias/A"));
            assert_eq!(search.dir_list.file().unwrap().path().unwrap(), root.join("A"));
            app.window.close();
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_completion_keeps_newer_selection_and_rendered_rows() {
        use super::super::{self as window, commands, navigation};
        glib::MainContext::default().block_on(async {
            let (_application, app, parent, root) = batch_boundary_fixture().await;
            navigation::select_names(&parent, &["A".into(), "B".into()], || {});
            let model = window::visible_selection(&parent);
            let row_a = (0..model.n_items()).filter_map(|i| model.item(i).and_downcast::<gio::FileInfo>())
                .find(|i| i.name() == std::path::Path::new("A")).unwrap();
            let items = ["A", "B"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
            let plan = batch::prepare(&items, &Rule::Template(format!("renamed{}", batch::NUMBER_3)), false);
            let hold = navigation::BatchRename::new(&app, &plan.requests());
            let other = (0..model.n_items()).find(|&i| model.item(i).and_downcast::<gio::FileInfo>()
                .is_some_and(|info| info.name() == std::path::Path::new("Other"))).unwrap();
            model.select_item(other, true);
            batch_settle().await;
            let outcome = gio::spawn_blocking(move || plan.apply().unwrap()).await.unwrap();
            hold.finish(&app, Some(&outcome));
            batch_settle().await;
            assert_eq!(commands::selected(&parent)[0].0.path().unwrap(), root.join("Other"));
            let renamed = (0..model.n_items()).filter_map(|i| model.item(i).and_downcast::<gio::FileInfo>())
                .find(|i| i.name() == std::path::Path::new("renamed001")).unwrap();
            assert_eq!(renamed, row_a, "batch replaced a selected item's identity");
            assert!(find_label(parent.view_stack.upcast_ref(), "renamed001").is_some());
            assert!(find_label(parent.view_stack.upcast_ref(), "renamed002").is_some());
            assert!(Rc::ptr_eq(&navigation::active_tab(&app), &parent));
            app.window.close();
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_renamed_symlink_keeps_open_descendants_through_history() {
        use super::super::{self as window, commands};
        glib::MainContext::default().block_on(async {
            let (_application, app, parent, root) = batch_boundary_fixture().await;
            std::os::unix::fs::symlink(root.join("A"), root.join("shortcut")).unwrap();
            let child = open_selected_child(&app, &root.join("shortcut")).await;
            let alias = open_selected_child(&app, &root.join("alias/shortcut")).await;
            app.tab_view.set_selected_page(parent.page.borrow().as_ref().unwrap());
            let state = build(&app, vec![root.join("shortcut"), root.join("B")]);
            state.template.set_text(&format!("renamed{}", batch::NUMBER_3));
            preview_ready(&state).await;
            state.apply();
            while state.busy.get() { batch_settle().await; }
            batch_settle().await;
            let check = |name: &str| {
                assert_eq!(child.current_dir().unwrap().path().unwrap(), root.join(name));
                assert_eq!(commands::selected(&child)[0].0.path().unwrap(), root.join(name).join("keep.txt"));
                assert_eq!(alias.current_dir().unwrap().path().unwrap(), root.join("alias").join(name));
                assert_eq!(commands::selected(&alias)[0].0.path().unwrap(), root.join("alias").join(name).join("keep.txt"));
                assert_eq!(std::fs::read_link(root.join(name)).unwrap(), root.join("A"));
                assert!(root.join("A/keep.txt").exists());
            };
            check("renamed001");
            commands::undo(&app);
            while app.history_busy.get() { batch_settle().await; }
            batch_settle().await;
            check("shortcut");
            commands::redo(&app);
            while app.history_busy.get() { batch_settle().await; }
            batch_settle().await;
            check("renamed001");
            assert_eq!(window::visible_selection(&child).selection().size(), 1);
            app.window.close();
            std::fs::remove_dir_all(root).unwrap();
        });
    }

    async fn batch_new_descendant(mode: super::super::ViewMode) {
        use super::super::{self as window, commands, navigation};
        let (_application, app, _parent, root) = batch_boundary_fixture().await;
        window::set_view_mode(&app, mode);
        std::fs::create_dir(root.join("A/deep")).unwrap();
        std::fs::write(root.join("A/deep/keep.txt"), "deep").unwrap();
        let child = open_selected_child(&app, &root.join("A")).await;
        let items = ["A", "B"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
        let plan = batch::prepare(&items, &Rule::Template(format!("renamed{}", batch::NUMBER_3)), false);
        let hold = navigation::BatchRename::new(&app, &plan.requests());
        navigation::go_up(&app, &child);
        batch_settle().await;
        navigation::navigate(&app, &child, gio::File::for_path(root.join("A/deep")));
        batch_settle().await;
        navigation::select_names(&child, &["keep.txt".into()], || {});
        let outcome = gio::spawn_blocking(move || plan.apply().unwrap()).await.unwrap();
        batch_settle().await; // Let monitor events arrive before applying the successful map.
        hold.finish(&app, Some(&outcome));
        batch_settle().await;
        assert_eq!(child.current_dir().unwrap().path().unwrap(), root.join("renamed001/deep"));
        assert_eq!(commands::selected(&child)[0].0.path().unwrap(), root.join("renamed001/deep/keep.txt"));
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_grid_new_descendant_navigation_follows_rename() {
        glib::MainContext::default().block_on(batch_new_descendant(super::super::ViewMode::Grid));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_list_new_descendant_navigation_follows_rename() {
        glib::MainContext::default().block_on(batch_new_descendant(super::super::ViewMode::List));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_columns_new_descendant_navigation_follows_rename() {
        glib::MainContext::default().block_on(batch_new_descendant(super::super::ViewMode::Columns));
    }

    async fn batch_replacement_navigation(mode: super::super::ViewMode) {
        use super::super::{self as window, commands, navigation};
        let (_application, app, _parent, root) = batch_boundary_fixture().await;
        window::set_view_mode(&app, mode);
        std::fs::create_dir(root.join("A/deep")).unwrap();
        std::fs::write(root.join("A/deep/keep.txt"), "original").unwrap();
        let child = open_selected_child(&app, &root.join("A")).await;
        let items = ["A", "B"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
        let plan = batch::prepare(&items, &Rule::Template(format!("renamed{}", batch::NUMBER_3)), false);
        let hold = navigation::BatchRename::new(&app, &plan.requests());
        let outcome = gio::spawn_blocking(move || plan.apply().unwrap()).await.unwrap();
        std::fs::create_dir_all(root.join("A/deep")).unwrap();
        std::fs::write(root.join("A/deep/replacement.txt"), "replacement").unwrap();
        navigation::navigate(&app, &child, gio::File::for_path(root.join("A/deep")));
        batch_settle().await;
        navigation::select_names(&child, &["replacement.txt".into()], || {});
        hold.finish(&app, Some(&outcome));
        batch_settle().await;
        assert_eq!(child.current_dir().unwrap().path().unwrap(), root.join("A/deep"), "new replacement location was remapped");
        assert_eq!(commands::selected(&child)[0].0.path().unwrap(), root.join("A/deep/replacement.txt"));
        if mode == window::ViewMode::Columns {
            let mut pending = vec![child.columns.widget.clone().upcast::<gtk::Widget>()];
            let mut headers = Vec::new();
            while let Some(widget) = pending.pop() {
                if let Some(view) = widget.downcast_ref::<gtk::ColumnView>() {
                    let title = view.columns().item(0).and_downcast::<gtk::ColumnViewColumn>().unwrap().title().unwrap().to_string();
                    if title == root.file_name().unwrap().to_string_lossy() {
                        let model = view.model().unwrap();
                        assert_eq!(model.selection().size(), 1);
                        assert_eq!(model.item(model.selection().minimum()).and_downcast::<gio::FileInfo>().unwrap().name(), std::path::Path::new("A"), "ancestor highlight followed the moved original");
                    }
                    headers.push(title);
                }
                let mut next = widget.first_child();
                while let Some(widget) = next { next = widget.next_sibling(); pending.push(widget); }
            }
            assert!(headers.contains(&"A".to_string()) && !headers.contains(&"renamed001".to_string()), "replacement ancestors point at original folder: {headers:?}");
        }
        assert_eq!(std::fs::read_to_string(root.join("renamed001/deep/keep.txt")).unwrap(), "original");
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_grid_keeps_newly_navigated_replacement_directory() {
        glib::MainContext::default().block_on(batch_replacement_navigation(super::super::ViewMode::Grid));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_list_keeps_newly_navigated_replacement_directory() {
        glib::MainContext::default().block_on(batch_replacement_navigation(super::super::ViewMode::List));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_columns_keeps_newly_navigated_replacement_directory() {
        glib::MainContext::default().block_on(batch_replacement_navigation(super::super::ViewMode::Columns));
    }

    fn bound_name(root: &gtk::Widget, info: &gio::FileInfo) -> Option<String> {
        if let Some(image) = root.downcast_ref::<gtk::Image>() {
            let matches = unsafe { image.data::<gio::FileInfo>("fs-file-info") }.is_some_and(|bound| unsafe { bound.as_ref() } == info);
            if matches {
                let mut child = image.parent()?.first_child();
                while let Some(widget) = child {
                    if let Some(stack) = widget.downcast_ref::<gtk::Stack>() {
                        return stack.child_by_name("view").and_downcast::<gtk::Label>().map(|label| label.text().to_string());
                    }
                    child = widget.next_sibling();
                }
            }
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(text) = bound_name(&widget, info) { return Some(text); }
            child = widget.next_sibling();
        }
        None
    }

    async fn batch_row_identity(mode: super::super::ViewMode, symlink: bool, replacement: bool) {
        use super::super::{self as window, commands, navigation};
        let (_application, app, _parent, root) = batch_boundary_fixture().await;
        window::set_view_mode(&app, mode);
        let source = if symlink {
            std::os::unix::fs::symlink(root.join("A"), root.join("shortcut")).unwrap();
            "shortcut"
        } else { "A" };
        let items = [source, "B"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
        let plan = batch::prepare(&items, &Rule::Template(format!("renamed{}", batch::NUMBER_3)), false);
        let requests = plan.requests();
        let hold = navigation::BatchRename::new(&app, &requests);
        let mut outcome = batch::Outcome { undo: Vec::new(), failed: Vec::new() };
        if replacement {
            let first = vec![requests[0].clone()];
            outcome = gio::spawn_blocking(move || batch::run(&first)).await.unwrap();
            assert_eq!(outcome.undo.len(), 1);
            if symlink { std::os::unix::fs::symlink(root.join("A"), root.join(source)).unwrap(); }
            else {
                std::fs::create_dir(root.join(source)).unwrap();
                std::fs::write(root.join(source).join("replacement.txt"), "replacement").unwrap();
            }
        }
        navigation::new_tab(&app, gio::File::for_path(&root), true);
        let fresh = navigation::active_tab(&app);
        batch_settle().await;
        while fresh.current_source().is_loading() { batch_settle().await; }
        let model = window::visible_selection(&fresh);
        let position = (0..model.n_items()).find(|&i| model.item(i).and_downcast::<gio::FileInfo>().is_some_and(|info| info.name() == std::path::Path::new(source))).unwrap();
        let chosen = model.item(position).and_downcast::<gio::FileInfo>().unwrap();
        model.select_item(position, true);
        let remaining = requests[usize::from(replacement)..].to_vec();
        let later = gio::spawn_blocking(move || batch::run(&remaining)).await.unwrap();
        outcome.undo.extend(later.undo); outcome.failed.extend(later.failed);
        hold.finish(&app, Some(&outcome));
        batch_settle().await; batch_settle().await;
        let expected = if replacement { source } else { "renamed001" };
        let selected: Vec<_> = commands::selected(&fresh).iter().map(|(file, _)| file.path().unwrap()).collect();
        assert_eq!(selected, [root.join(expected)], "batch selected the wrong directory entry");
        assert_eq!(chosen.display_name(), expected);
        assert_eq!(bound_name(fresh.view_stack.upcast_ref(), &chosen).as_deref(), Some(expected));
        commands::set_clipboard(&app, false);
        let copied = Rc::new(RefCell::new(None)); let received = copied.clone();
        commands::read_clipboard_files(&app.window.clipboard(), move |cut, paths| { assert!(!cut); *received.borrow_mut() = Some(paths); });
        batch_settle().await;
        assert_eq!(*copied.borrow(), Some(vec![root.join(expected)]));
        assert!(root.join("renamed001/keep.txt").is_file());
        if replacement {
            if symlink {
                assert!(std::fs::symlink_metadata(root.join(source)).unwrap().file_type().is_symlink());
                assert_eq!(std::fs::read_link(root.join(source)).unwrap(), root.join("A"));
            } else { assert_eq!(std::fs::read_to_string(root.join("A/replacement.txt")).unwrap(), "replacement"); }
        }
        app.window.close(); std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_grid_replacement_row_is_not_retargeted() {
        glib::MainContext::default().block_on(batch_row_identity(super::super::ViewMode::Grid, false, true));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_list_replacement_row_is_not_retargeted() {
        glib::MainContext::default().block_on(batch_row_identity(super::super::ViewMode::List, false, true));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_columns_replacement_row_is_not_retargeted() {
        glib::MainContext::default().block_on(batch_row_identity(super::super::ViewMode::Columns, false, true));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_replacement_symlink_uses_entry_not_target_identity() {
        glib::MainContext::default().block_on(batch_row_identity(super::super::ViewMode::List, true, true));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_late_original_rows_keep_identity_and_labels() {
        glib::MainContext::default().block_on(batch_row_identity(super::super::ViewMode::List, false, false));
    }
    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_late_original_symlink_keeps_identity_and_labels() {
        glib::MainContext::default().block_on(batch_row_identity(super::super::ViewMode::Columns, true, false));
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn batch_unknown_entry_identity_does_not_authorize_remapping() {
        use super::super::{self as window, commands, navigation};
        glib::MainContext::default().block_on(async {
            let (_application, app, _parent, root) = batch_boundary_fixture().await;
            window::set_view_mode(&app, window::ViewMode::List);
            let items = ["A", "B"].iter().map(|name| Item::read(root.join(name)).unwrap()).collect::<Vec<_>>();
            let requests = batch::prepare(&items, &Rule::Template(format!("renamed{}", batch::NUMBER_3)), false).requests();
            let hold = navigation::BatchRename::new(&app, &requests);
            let first = vec![requests[0].clone()];
            let mut outcome = gio::spawn_blocking(move || batch::run(&first)).await.unwrap();
            std::fs::create_dir(root.join("A")).unwrap();
            std::fs::write(root.join("A/replacement.txt"), "replacement").unwrap();
            navigation::new_tab(&app, gio::File::for_path(&root), true);
            let fresh = navigation::active_tab(&app);
            batch_settle().await;
            while fresh.current_source().is_loading() { batch_settle().await; }
            let model = window::visible_selection(&fresh);
            let position = (0..model.n_items()).find(|&i| model.item(i).and_downcast::<gio::FileInfo>().is_some_and(|info| info.name() == std::path::Path::new("A"))).unwrap();
            let unknown = model.item(position).and_downcast::<gio::FileInfo>().unwrap();
            unknown.remove_attribute("unix::device"); unknown.remove_attribute("unix::inode");
            model.select_item(position, true);
            let rest = requests[1..].to_vec();
            let later = gio::spawn_blocking(move || batch::run(&rest)).await.unwrap();
            outcome.undo.extend(later.undo); outcome.failed.extend(later.failed);
            hold.finish(&app, Some(&outcome));
            batch_settle().await;
            assert!(commands::selected(&fresh).is_empty(), "ambiguous identity retained an actionable selection");
            assert_eq!(unknown.display_name(), "A", "unknown row was renamed by URI alone");
            assert_eq!(std::fs::read_to_string(root.join("A/replacement.txt")).unwrap(), "replacement");
            assert!(root.join("renamed001/keep.txt").is_file());
            app.window.close(); std::fs::remove_dir_all(root).unwrap();
        });
    }

    #[test]
    fn numbering_name_order_is_natural_and_current_order_is_stable() {
        let root =
            std::env::temp_dir().join(format!("filescope-batch-order-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut items: Vec<_> = ["img10.jpg", "img2.jpg", "img1.jpg"]
            .iter()
            .map(|name| {
                let path = root.join(name);
                std::fs::write(&path, name).unwrap();
                Item::read(path).unwrap()
            })
            .collect();
        sort_items(&mut items, 0);
        assert_eq!(items[0].name, "img10.jpg");
        sort_items(&mut items, 1);
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["img1.jpg", "img2.jpg", "img10.jpg"]
        );
        sort_items(&mut items, 2);
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["img10.jpg", "img2.jpg", "img1.jpg"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn batch_dialog_previews_applies_undoes_and_releases_state() {
        use super::super::{commands, navigation};
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let application = adw::Application::builder()
            .application_id("dev.filescope.BatchTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!("filescope-batch-ui-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let paths: Vec<_> = ["b.jpg", "a.JPG", "c.png"]
            .iter()
            .map(|name| {
                let path = root.join(name);
                std::fs::write(&path, name).unwrap();
                path
            })
            .collect();
        glib::MainContext::default().block_on(async {
            super::super::open_files(&application, &[gio::File::for_path(&root)]);
            let app = super::super::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            for _ in 0..100 {
                if tab.selection.n_items() == 3 {
                    break;
                }
                glib::timeout_future(std::time::Duration::from_millis(25)).await;
            }
            tab.selection.select_all();
            super::super::update_actions(&app, &tab);
            assert!(app.window.lookup_action("rename").unwrap().is_enabled());
            commands::rename_selected(&app);
            let dialog = gtk::Window::list_toplevels()
                .into_iter()
                .filter_map(|w| w.downcast::<gtk::Window>().ok())
                .find(|w| w.title().as_deref() == Some("Rename 3 Items"))
                .expect("multi selection did not open batch dialog");
            dialog.close();
            drop(dialog);
            let state = build(&app, paths);
            preview_ready(&state).await;
            assert!(!state.apply.is_sensitive());
            state.template.set_text("same");
            preview_ready(&state).await;
            // Ignoring extensions makes all three destinations identical.
            state.extensions.set_active(false);
            preview_ready(&state).await;
            assert!(
                state
                    .plan
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .rows
                    .iter()
                    .all(|row| row.error.is_some())
            );
            assert!(!state.apply.is_sensitive());
            capture(&state, "conflicts").await;
            state.extensions.set_active(true);
            state
                .template
                .set_text(&format!("{}{}", "Long preview ".repeat(8), batch::NUMBER_3));
            preview_ready(&state).await;
            let proposed = state.plan.borrow().as_ref().unwrap().rows[0].name.clone();
            let content = state.window.upgrade().unwrap().content().unwrap();
            content.allocate(780, 560, -1, None);
            let label =
                find_label(&content, &proposed).expect("new-name preview label was not bound");
            assert!(
                label
                    .tooltip_text()
                    .is_some_and(|tooltip| tooltip.contains(&proposed)),
                "new-name tooltip hides the proposed filename"
            );
            drop(label);
            drop(content);
            state
                .template
                .set_text(&format!("Holiday {}", batch::NUMBER_3));
            preview_ready(&state).await;
            assert!(state.apply.is_sensitive());
            assert_eq!(
                state.plan.borrow().as_ref().unwrap().rows[0].name,
                "Holiday 001.jpg"
            );
            state.order.set_selected(1);
            preview_ready(&state).await;
            assert_eq!(
                state.plan.borrow().as_ref().unwrap().rows[0].item.name,
                "a.JPG"
            );
            capture(&state, "template").await;
            assert!(state.template.activates_default());
            assert_eq!(
                state.window.upgrade().unwrap().default_widget(),
                Some(state.apply.clone().upcast())
            );
            state.apply.emit_clicked();
            assert!(!state.controls.is_sensitive());
            for _ in 0..100 {
                if !state.busy.get() {
                    break;
                }
                glib::timeout_future(std::time::Duration::from_millis(25)).await;
            }
            assert!(state.closed.get());
            assert!(root.join("Holiday 001.JPG").exists());
            assert_eq!(app.undo_stack.borrow().len(), 1);
            commands::undo(&app);
            assert!(
                !root.join("a.JPG").exists(),
                "Undo ran synchronously inside the action callback"
            );
            assert!(!app.window.lookup_action("redo").unwrap().is_enabled());
            history_ready(&app).await;
            assert!(root.join("a.JPG").exists());
            assert!(!root.join("Holiday 001.JPG").exists());
            commands::redo(&app);
            assert!(!app.window.lookup_action("undo").unwrap().is_enabled());
            history_ready(&app).await;
            assert!(root.join("Holiday 001.JPG").exists());
            commands::undo(&app);
            history_ready(&app).await;
            let weak = Rc::downgrade(&state);
            let entry_weak = state.template.downgrade();
            drop(state);
            glib::timeout_future(std::time::Duration::from_millis(250)).await;
            assert!(
                weak.upgrade().is_none(),
                "closed batch dialog retained its state"
            );
            assert!(
                entry_weak.upgrade().is_none(),
                "closed dialog retained its template entry"
            );

            let paths = ["a.JPG", "b.jpg", "c.png"]
                .iter()
                .map(|name| root.join(name))
                .collect();
            let state = build(&app, paths);
            preview_ready(&state).await;
            state.find.set_text("a");
            state.replacement.set_text("café");
            state.template_mode.set_active(false);
            preview_ready(&state).await;
            assert_eq!(
                state.plan.borrow().as_ref().unwrap().rows[0].name,
                "café.JPG"
            );
            assert_eq!(state.plan.borrow().as_ref().unwrap().rows[1].name, "b.jpg");
            capture(&state, "replace").await;
            // A new destination after the preview must block the entire batch.
            std::fs::write(root.join("café.JPG"), b"new arrival").unwrap();
            state.apply.emit_clicked();
            for _ in 0..100 {
                if !state.busy.get() {
                    break;
                }
                glib::timeout_future(std::time::Duration::from_millis(25)).await;
            }
            assert!(!state.closed.get());
            assert!(state.status.text().contains("No files changed"));
            assert!(root.join("a.JPG").exists());
            assert_eq!(
                std::fs::read(root.join("café.JPG")).unwrap(),
                b"new arrival"
            );
            capture(&state, "stale").await;
            state.cancel.emit_clicked();
            assert!(state.closed.get());
            drop(state);

            // A hidden folder selection must not enable rename on special views.
            super::super::places::show_computer(&app, &tab);
            super::super::update_actions(&app, &tab);
            assert!(!app.window.lookup_action("rename").unwrap().is_enabled());
            commands::rename_selected(&app);
            assert!(!gtk::Window::list_toplevels().iter().any(|w| {
                w.clone()
                    .downcast::<gtk::Window>()
                    .ok()
                    .is_some_and(|w| w.title().is_some_and(|t| t.starts_with("Rename ")))
            }));
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn batch_history_keeps_new_actions_and_partial_retries() {
        use super::super::commands;
        use crate::undo::Action;
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        let application = adw::Application::builder()
            .application_id("dev.filescope.BatchHistoryTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root =
            std::env::temp_dir().join(format!("filescope-batch-history-ui-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let snapshot = || {
            ["a.txt", "b.txt"]
                .iter()
                .map(|name| Item::read(root.join(name)).unwrap())
                .collect::<Vec<_>>()
        };
        glib::MainContext::default().block_on(async {
            super::super::open_files(&application, &[gio::File::for_path(&root)]);
            let app = super::super::app_state(&application).unwrap();
            let previous = root.join("previous"); std::fs::create_dir(&previous).unwrap();
            commands::push_undo(&app, Action::NewFolder { path: previous.clone() });
            let renamed = batch::prepare(&snapshot(), &Rule::Template(format!("new_{}", batch::NAME)), true).apply().unwrap();
            commands::push_undo(&app, Action::BatchRename { pending: renamed.undo });
            std::fs::write(root.join("a.txt"), b"new arrival").unwrap();
            commands::undo(&app);
            assert!(app.history_busy.get());
            commands::undo(&app); commands::redo(&app);
            assert!(previous.exists(), "a second Undo must not pop an older action during batch history");
            assert_eq!(app.undo_stack.borrow().len(), 1);
            let later = root.join("later"); std::fs::create_dir(&later).unwrap();
            // Simulate a background operation completing before the history worker.
            commands::push_undo(&app, Action::NewFolder { path: later.clone() });
            assert!(!app.window.lookup_action("undo").unwrap().is_enabled());
            assert!(!app.window.lookup_action("redo").unwrap().is_enabled());
            history_ready(&app).await;
            assert!(!app.history_busy.get());
            assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"new arrival");
            assert_eq!(std::fs::read(root.join("b.txt")).unwrap(), b"b.txt");
            assert!(app.redo_stack.borrow().is_empty(), "new actions must invalidate a pending Undo's Redo result");
            assert_eq!(app.undo_stack.borrow().len(), 3);
            assert!(matches!(&app.undo_stack.borrow()[1], Action::BatchRename { pending } if pending.len() == 1));
            assert!(matches!(app.undo_stack.borrow().last(), Some(Action::NewFolder { path }) if path == &later));
            commands::undo(&app);
            assert!(!later.exists(), "the newer operation must remain first in Undo order");
            std::fs::remove_file(root.join("a.txt")).unwrap();
            commands::undo(&app); history_ready(&app).await;
            assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"a.txt");

            app.undo_stack.borrow_mut().clear(); app.redo_stack.borrow_mut().clear();
            let renamed = batch::prepare(&snapshot(), &Rule::Template(format!("new_{}", batch::NAME)), true).apply().unwrap();
            commands::push_undo(&app, Action::BatchRename { pending: renamed.undo });
            commands::undo(&app); history_ready(&app).await;
            std::fs::write(root.join("new_a.txt"), b"occupied during redo").unwrap();
            commands::redo(&app);
            assert!(app.history_busy.get());
            let latest = root.join("latest"); std::fs::create_dir(&latest).unwrap();
            commands::push_undo(&app, Action::NewFolder { path: latest.clone() });
            history_ready(&app).await;
            assert!(app.redo_stack.borrow().is_empty(), "a failed Redo must not revive history cleared by a new action");
            assert_eq!(app.undo_stack.borrow().len(), 2);
            assert!(matches!(&app.undo_stack.borrow()[0], Action::BatchRename { pending } if pending.len() == 1));
            assert!(matches!(app.undo_stack.borrow().last(), Some(Action::NewFolder { path }) if path == &latest));
            assert_eq!(std::fs::read(root.join("new_a.txt")).unwrap(), b"occupied during redo");
            assert_eq!(std::fs::read(root.join("new_b.txt")).unwrap(), b"b.txt");
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }
}
