//! Failed or pending navigation must never expose the underlying local directory.
use super::*;
use std::time::{Duration, Instant};

async fn until(condition: impl Fn() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "capability check did not settle"
        );
        glib::timeout_future(Duration::from_millis(20)).await;
    }
}

fn fixture() -> (adw::Application, Rc<App>, Rc<Tab>, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "filescope-capabilities-{}",
        glib::uuid_string_random()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("notes.txt"), "keep this file").unwrap();
    let config = glib::user_config_dir().join("filescope");
    assert!(config.starts_with(std::env::temp_dir()));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("settings"), "automatic_updates=false\n").unwrap();
    adw::init().unwrap();
    gtk::Settings::default()
        .unwrap()
        .set_gtk_enable_animations(false);
    let application = adw::Application::builder()
        .application_id("dev.filescope.CapabilityTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    open_files(&application, &[gio::File::for_path(&root)]);
    let app = app_state(&application).unwrap();
    let tab = navigation::active_tab(&app);
    (application, app, tab, root)
}

fn unavailable(tab: &Tab, root: &std::path::Path, name: &str) {
    tab.begin_navigation();
    tab.dir_list.set_file(gio::File::NONE);
    *tab.location.borrow_mut() = Some(gio::File::for_path(root));
    if tab.view_stack.child_by_name(name).is_none() {
        tab.view_stack
            .add_named(&gtk::Label::new(Some("Unavailable fixture")), Some(name));
    }
    tab.view_stack.set_visible_child_name(name);
}

fn terminal_visible(tab: &Tab) -> bool {
    tab.terminal
        .widget()
        .end_child()
        .is_some_and(|child| child.is_visible())
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn unavailable_folder_actions_do_not_target_backing_paths() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 1).await;
        tab.selection.select_item(0, true);
        commands::set_clipboard(&app, false);
        for name in ["unavailable"] {
            unavailable(&tab, &root, name);
            commands::new_folder(&app);
            assert!(
                !root.join("New Folder").exists(),
                "unavailable view wrote to its backing directory"
            );
            update_chrome(&app);
            for action in [
                "new-folder",
                "paste",
                "rename",
                "trash",
                "properties",
                "find",
                "open-terminal",
            ] {
                assert!(
                    !app.window.lookup_action(action).unwrap().is_enabled(),
                    "{name} enabled {action}"
                );
            }
            assert!(
                app.window
                    .lookup_action("copy-folder-path")
                    .unwrap()
                    .is_enabled()
            );
            assert!(
                app.window.lookup_action("up").unwrap().is_enabled(),
                "unavailable identity lost navigation"
            );
        }
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            "keep this file"
        );
        app.window.close();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn pending_navigation_rejects_old_selection_and_editors() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 1).await;
        tab.selection.select_item(0, true);
        tab.begin_navigation();
        *tab.pending_location.borrow_mut() = Some(gio::File::for_path(root.join("pending")));
        assert!(
            commands::selected(&tab).is_empty(),
            "old model was resolved against a pending location"
        );
        assert!(!view::start_inline_rename(&app, &tab));
        commands::rename_with_dialog(&app);
        commands::show_properties(&app);
        assert!(
            app.window.visible_dialog().is_none(),
            "pending location opened a stale file editor"
        );
        update_chrome(&app);
        for action in ["open", "copy", "new-folder", "find", "open-terminal"] {
            assert!(
                !app.window.lookup_action(action).unwrap().is_enabled(),
                "pending location enabled {action}"
            );
        }
        app.window.close();
    });
}


#[test]
#[ignore = "requires isolated GTK, SHELL=/bin/sh, and temporary XDG directories"]
fn unavailable_navigation_can_hide_but_not_start_terminal() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        until(|| !tab.dir_list.is_loading()).await;
        tab.terminal.toggle(Some(&root));
        until(|| tab.terminal.has_live_session()).await;
        *tab.pending_location.borrow_mut() = Some(gio::File::for_path(root.join("pending")));
        update_chrome(&app);
        assert!(
            app.window
                .lookup_action("terminal-pane")
                .unwrap()
                .is_enabled()
        );
        toggle_terminal_pane(&app);
        assert!(!terminal_visible(&tab));
        assert!(
            tab.terminal.has_live_session(),
            "hiding stopped the owned shell"
        );
        update_chrome(&app);
        assert!(
            !app.window
                .lookup_action("terminal-pane")
                .unwrap()
                .is_enabled()
        );
        toggle_terminal_pane(&app);
        assert!(
            !terminal_visible(&tab),
            "unavailable location reopened the terminal"
        );
        app.window.close();
    });
}


#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn rename_keeps_an_existing_destination_unchanged() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 1).await;
        tab.selection.select_item(0, true);
        std::fs::write(root.join("occupied.txt"), "existing contents").unwrap();
        commands::rename_with_dialog(&app);
        let dialog = app
            .window
            .visible_dialog()
            .and_downcast::<adw::AlertDialog>()
            .unwrap();
        dialog
            .extra_child()
            .and_downcast::<gtk::Entry>()
            .unwrap()
            .set_text("occupied.txt");
        dialog.emit_by_name::<()>("response", &[&"rename"]);
        dialog.close();
        glib::timeout_future(Duration::from_millis(80)).await;
        until(|| !BackgroundWork::active()).await;
        assert_eq!(
            std::fs::read_to_string(root.join("occupied.txt")).unwrap(),
            "existing contents"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("notes.txt")).unwrap(),
            "keep this file"
        );
        app.window.close();
    });
}


#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn rename_completion_does_not_replace_a_newer_selection() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        std::fs::write(root.join("other.txt"), "newer choice").unwrap();
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 2).await;
        let position = |name: &str| {
            (0..tab.selection.n_items())
                .find(|index| {
                    tab.selection
                        .item(*index)
                        .and_downcast::<gio::FileInfo>()
                        .unwrap()
                        .name()
                        == std::path::Path::new(name)
                })
                .unwrap()
        };
        tab.selection.select_item(position("notes.txt"), true);
        commands::rename_file(
            &app,
            &tab,
            &gio::File::for_path(root.join("notes.txt")),
            "renamed.txt",
        );
        tab.selection.select_item(position("other.txt"), true);
        until(|| !BackgroundWork::active() && root.join("renamed.txt").exists()).await;
        glib::timeout_future(Duration::from_millis(100)).await;
        assert_eq!(
            commands::selected(&tab)[0].1.name(),
            std::path::Path::new("other.txt"),
            "rename completion replaced the user's newer selection"
        );
        app.window.close();
    });
}


fn select_named(tab: &Rc<Tab>, name: &str) {
    let position = (0..tab.selection.n_items()).find(|position| {
        tab.selection.item(*position).and_downcast::<gio::FileInfo>()
            .is_some_and(|info| info.name() == std::path::Path::new(name))
    }).unwrap();
    tab.selection.select_item(position, true);
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn compress_accepts_dangling_symlink_entries() {
    let (_application, app, tab, root) = fixture();
    std::os::unix::fs::symlink("missing-target", root.join("link")).unwrap();
    glib::MainContext::default().block_on(async {
        until(|| tab.selection.n_items() == 2 && location_access(&tab) == LocationAccess::Local).await;
        tab.selection.select_all();
        commands::compress_selected(&app);
        until(|| !BackgroundWork::active()).await;
        assert!(app.window.visible_dialog().is_some(), "Compress rejected an existing symlink entry");
        assert!(root.join("link").is_symlink());
        app.window.close();
    });
}


fn contains_text(widget: &gtk::Widget, text: &str) -> bool {
    if widget.downcast_ref::<gtk::Label>().is_some_and(|label| label.text().contains(text)) { return true; }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if contains_text(&widget, text) { return true; }
    }
    false
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn local_symlink_entry_keeps_rename_and_history_local() {
    let (_application, app, tab, root) = fixture();
    let link = local_link(&root, "link.txt");
    let target = std::fs::read_link(&link).unwrap();
    let renamed = root.join("renamed-link.txt");
    glib::MainContext::default().block_on(async {
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 2 && location_access(&tab) == LocationAccess::Local).await;
        select_named(&tab, "link.txt");
        commands::rename_file(&app, &tab, &gio::File::for_path(&link), "renamed-link.txt");
        until(|| !BackgroundWork::active()).await;
        assert!(renamed.is_symlink(), "renaming a local link tried to connect to its target file");
        assert_eq!(std::fs::read_link(&renamed).unwrap(), target);
        assert_eq!(app.undo_stack.borrow().len(), 1, "local link rename lost Undo");
        commands::undo(&app);
        until(|| !BackgroundWork::active()).await;
        assert!(link.is_symlink(), "Undo followed the link target instead of its local entry");
        commands::redo(&app);
        until(|| !BackgroundWork::active()).await;
        assert!(renamed.is_symlink());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "target file stays unchanged");
        app.window.close();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn local_symlink_entry_can_be_trashed_and_restored() {
    let (_application, app, tab, root) = fixture();
    let link = local_link(&root, "link.txt");
    let target = std::fs::read_link(&link).unwrap();
    let probe = root.join("native-trash-probe");
    std::fs::write(&probe, "native capability probe").unwrap();
    let native_trash = gio::File::for_path(&probe).trash(gio::Cancellable::NONE).is_ok();
    if !native_trash { std::fs::remove_file(probe).unwrap(); }
    glib::MainContext::default().block_on(async {
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 2 && location_access(&tab) == LocationAccess::Local).await;
        select_named(&tab, "link.txt");
        commands::trash_selected(&app);
        until(|| !BackgroundWork::active()).await;
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "target file stays unchanged");
        if native_trash {
            assert!(!link.is_symlink(), "Trash treated the local link as target data");
            commands::undo(&app);
            until(|| !BackgroundWork::active()).await;
            assert_eq!(std::fs::read_link(&link).unwrap(), target);
        } else {
            until(|| contains_text(app.toasts.upcast_ref(), "Moved 0 items to Trash; 1 failed")).await;
            assert!(link.is_symlink(), "a refused native Trash changed the link");
            assert!(app.undo_stack.borrow().is_empty());
            println!("Native Trash refuses this internal test mount; local entry dispatch and failure preservation verified");
        }
        app.window.close();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn local_symlink_entries_keep_batch_rename() {
    let (_application, app, tab, root) = fixture();
    local_link(&root, "link.txt");
    glib::MainContext::default().block_on(async {
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 2 && location_access(&tab) == LocationAccess::Local).await;
        tab.selection.select_all();
        commands::rename_selected(&app);
        until(|| !BackgroundWork::active()).await;
        let dialog = gtk::Window::list_toplevels().into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
            .find(|window| window.title().is_some_and(|title| title.starts_with("Rename ")));
        assert!(dialog.is_some(), "batch rename followed a local link's target file");
        dialog.unwrap().close();
        app.window.close();
    });
}


#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn local_directory_alias_keeps_its_location_and_actions() {
    let (_application, app, tab, root) = fixture();
    let target = root.join("local-target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("local.txt"), "local alias").unwrap();
    let alias = root.join("local-alias");
    std::os::unix::fs::symlink(&target, &alias).unwrap();
    glib::MainContext::default().block_on(async {
        navigation::navigate(&app, &tab, gio::File::for_path(&alias));
        until(|| !tab.dir_list.is_loading() && tab.selection.n_items() == 1 && !BackgroundWork::active()).await;
        assert_eq!(location_access(&tab), LocationAccess::Local);
        assert_eq!(tab.current_dir().and_then(|file| file.path()), Some(alias));
        update_chrome(&app);
        assert!(app.window.lookup_action("find").unwrap().is_enabled());
        assert!(app.window.lookup_action("new-folder").unwrap().is_enabled());
        commands::new_folder(&app);
        until(|| !BackgroundWork::active()).await;
        assert!(target.join("New Folder").is_dir());
        app.window.close();
    });
}


fn local_link(root: &std::path::Path, name: &str) -> PathBuf {
    let backing = std::env::temp_dir().join("symlink-target");
    std::fs::create_dir_all(&backing).unwrap();
    std::fs::write(backing.join("target.txt"), "target file stays unchanged").unwrap();
    let link = root.join(name);
    std::os::unix::fs::symlink(backing.join("target.txt"), &link).unwrap();
    link
}
