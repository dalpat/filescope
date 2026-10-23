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
        let state = self.clone();
        glib::spawn_future_local(async move {
            let result = gio::spawn_blocking(move || plan.apply()).await;
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
