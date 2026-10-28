//! User file operations must keep the same live view, focus and surrounding rows.
use super::*;
use std::time::{Duration, Instant};

async fn settle() {
    glib::timeout_future(Duration::from_millis(100)).await;
    for widget in gtk::Window::list_toplevels() {
        if widget.is_visible() && widget.width() > 0 {
            widget.queue_allocate();
            widget.allocate(widget.width(), widget.height(), -1, None);
        }
    }
}
async fn until(condition: impl Fn() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "operation did not settle"
        );
        settle().await;
    }
    settle().await;
}
fn info(model: &gtk::MultiSelection, name: &str) -> Option<gio::FileInfo> {
    (0..model.n_items())
        .filter_map(|i| model.item(i).and_downcast::<gio::FileInfo>())
        .find(|i| i.name() == std::path::Path::new(name))
}
fn pos(model: &gtk::MultiSelection, name: &str) -> u32 {
    (0..model.n_items())
        .find(|&i| {
            model
                .item(i)
                .and_downcast::<gio::FileInfo>()
                .is_some_and(|i| i.name() == std::path::Path::new(name))
        })
        .unwrap()
}
fn children(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut result = vec![widget.clone()];
    let mut child = widget.first_child();
    while let Some(w) = child {
        result.extend(children(&w));
        child = w.next_sibling();
    }
    result
}
fn editor(tab: &Tab) -> gtk::Entry {
    children(tab.view_stack.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Stack>().ok())
        .find(|s| s.visible_child_name().as_deref() == Some("edit"))
        .and_then(|s| s.child_by_name("edit").and_downcast::<gtk::Entry>())
        .expect("inline editor")
}
fn file_view(tab: &Tab, mode: ViewMode) -> gtk::Widget {
    match mode {
        ViewMode::Grid => tab.grid_view.clone().upcast(),
        ViewMode::List => tab.column_view.clone().upcast(),
        ViewMode::Columns => tab.columns.view().unwrap().upcast(),
    }
}
fn scroll(view: &gtk::Widget) -> gtk::Adjustment {
    let mut parent = view.parent();
    while let Some(widget) = parent {
        if let Some(scroller) = widget.downcast_ref::<gtk::ScrolledWindow>() {
            return scroller.vadjustment();
        }
        parent = widget.parent();
    }
    panic!("file view scroller")
}
fn focus_is_in(app: &App, view: &gtk::Widget) -> bool {
    gtk::prelude::RootExt::focus(&app.window).is_some_and(|w| w == *view || w.is_ancestor(view))
}
fn show_row(tab: &Tab, mode: ViewMode, position: u32) {
    match mode {
        ViewMode::Grid => tab
            .grid_view
            .scroll_to(position, gtk::ListScrollFlags::FOCUS, None),
        ViewMode::List => {
            tab.column_view
                .scroll_to(position, None, gtk::ListScrollFlags::FOCUS, None)
        }
        ViewMode::Columns => {
            tab.columns
                .view()
                .unwrap()
                .scroll_to(position, None, gtk::ListScrollFlags::FOCUS, None)
        }
    }
}
fn fixture(mode: ViewMode) -> (adw::Application, Rc<App>, Rc<Tab>, PathBuf) {
    assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
    adw::init().unwrap();
    gtk::Settings::default()
        .unwrap()
        .set_gtk_enable_animations(false);
    let application = adw::Application::builder()
        .application_id("dev.filescope.OperationTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    let root =
        std::env::temp_dir().join(format!("filescope-operation-test-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..350 {
        std::fs::create_dir(root.join(format!("folder{i:03}"))).unwrap();
    }
    open_files(&application, &[gio::File::for_path(&root)]);
    let app = app_state(&application).unwrap();
    let tab = navigation::active_tab(&app);
    set_view_mode(&app, mode);
    (application, app, tab, root)
}
fn reset_counter(model: &gtk::MultiSelection) -> Rc<Cell<u32>> {
    let resets = Rc::new(Cell::new(0));
    let r = resets.clone();
    model.connect_items_changed(move |model, _, removed, _| {
        if removed > 1 && model.n_items() == 0 {
            r.set(r.get() + 1);
        }
    });
    resets
}
async fn operation_continuity(mode: ViewMode) {
    let (_application, app, tab, root) = fixture(mode);
    let model = visible_selection(&tab);
    until(|| model.n_items() == 350).await;
    let stable = info(&model, "folder100").unwrap();
    let resets = reset_counter(&model);
    let view = file_view(&tab, mode);
    let selected = pos(&model, "folder250");
    show_row(&tab, mode, selected);
    model.select_item(selected, true);
    settle().await;
    let adjustment = scroll(&view);
    let before_scroll = adjustment.value();
    let before_cell = rendered_name_cell(&view, &info(&model, "folder250").unwrap()).0;
    assert!(before_scroll > 100.0);
    commands::rename_selected(&app);
    settle().await;
    let entry = editor(&tab);
    entry.set_text("folder250-renamed");
    entry.emit_activate();
    until(|| info(&model, "folder250-renamed").is_some()).await;
    assert_eq!(resets.get(), 0, "rename cleared the entire view");
    assert_eq!(
        info(&model, "folder100"),
        Some(stable.clone()),
        "unaffected row was replaced"
    );
    assert_eq!(
        commands::selected(&tab)
            .iter()
            .map(|(_, i)| i.name())
            .collect::<Vec<_>>(),
        [PathBuf::from("folder250-renamed")]
    );
    assert_rendered_name(&view, &model, "folder250-renamed");
    assert_eq!(
        rendered_name_cell(&view, &info(&model, "folder250-renamed").unwrap()).0,
        before_cell,
        "rename replaced the rendered cell"
    );
    assert!(focus_is_in(&app, &view), "rename lost keyboard focus");
    assert!(
        (adjustment.value() - before_scroll).abs() < 200.0,
        "rename jumped from {before_scroll} to {}",
        adjustment.value()
    );

    commands::delete_selected(&app);
    settle().await;
    let dialog = app
        .window
        .visible_dialog()
        .and_downcast::<adw::AlertDialog>()
        .unwrap();
    dialog.emit_by_name::<()>("response", &[&"delete"]);
    dialog.close();
    until(|| info(&model, "folder250-renamed").is_none()).await;
    assert_eq!(resets.get(), 0, "delete cleared the entire view");
    assert_eq!(info(&model, "folder100"), Some(stable.clone()));
    assert_eq!(
        commands::selected(&tab)
            .iter()
            .map(|(_, i)| i.name())
            .collect::<Vec<_>>(),
        [PathBuf::from("folder251")]
    );
    assert!(focus_is_in(&app, &view), "delete lost keyboard focus");
    assert!(
        (adjustment.value() - before_scroll).abs() < 200.0,
        "delete jumped to {}",
        adjustment.value()
    );

    commands::new_folder(&app);
    until(|| info(&model, "New Folder").is_some()).await;
    assert_eq!(resets.get(), 0, "new folder cleared the entire view");
    assert_eq!(info(&model, "folder100"), Some(stable));
    assert_eq!(
        commands::selected(&tab)
            .iter()
            .map(|(_, i)| i.name())
            .collect::<Vec<_>>(),
        [PathBuf::from("New Folder")]
    );
    assert!(gtk::prelude::WidgetExt::is_visible(&editor(&tab)));
    app.window.close();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn grid_operations_preserve_position_focus_and_selection() {
    glib::MainContext::default().block_on(operation_continuity(ViewMode::Grid));
}
#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn list_operations_preserve_position_focus_and_selection() {
    glib::MainContext::default().block_on(operation_continuity(ViewMode::List));
}
#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn column_operations_preserve_position_focus_and_selection() {
    glib::MainContext::default().block_on(operation_continuity(ViewMode::Columns));
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn operation_completion_preserves_newer_user_choices() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        let model = visible_selection(&tab);
        until(|| model.n_items() == 350).await;
        for (name, clear) in [("folder250", false), ("folder251", true)] {
            model.select_item(pos(&model, name), true);
            let pending = navigation::RemovalSelection::capture(&app, &[root.join(name)]).unwrap();
            if clear {
                model.unselect_all();
            } else {
                model.select_item(pos(&model, "folder100"), true);
            }
            std::fs::remove_dir(root.join(name)).unwrap();
            navigation::refresh(&tab);
            pending.finish(&app);
            until(|| info(&model, name).is_none()).await;
            if clear {
                assert!(commands::selected(&tab).is_empty());
            } else {
                assert_eq!(
                    commands::selected(&tab)[0].1.name(),
                    std::path::Path::new("folder100")
                );
            }
        }
        model.select_item(pos(&model, "folder252"), true);
        let pending =
            navigation::RemovalSelection::capture(&app, &[root.join("folder252")]).unwrap();
        navigation::new_tab(&app, gio::File::for_path(root.join("folder100")), true);
        let other = navigation::active_tab(&app);
        std::fs::remove_dir(root.join("folder252")).unwrap();
        navigation::refresh(&tab);
        pending.finish(&app);
        until(|| info(&model, "folder252").is_none()).await;
        assert!(Rc::ptr_eq(&navigation::active_tab(&app), &other));
        assert!(commands::selected(&other).is_empty());
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}

fn column_for(tab: &Tab, directory: &std::path::Path) -> gtk::ColumnView {
    children(tab.columns.widget.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::ColumnView>().ok())
        .find(|v| {
            v.model().is_some_and(|m| {
                (0..m.n_items())
                    .filter_map(|i| m.item(i).and_downcast::<gio::FileInfo>())
                    .any(|i| {
                        i.attribute_object("standard::file")
                            .and_downcast::<gio::File>()
                            .and_then(|f| f.parent())
                            .and_then(|f| f.path())
                            .as_deref()
                            == Some(directory)
                    })
            })
        })
        .expect("column for directory")
}
#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn renamed_ancestor_preserves_columns_and_descendant_selection() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::Columns);
        std::fs::create_dir_all(root.join("folder250/nested")).unwrap();
        std::fs::write(root.join("folder250/nested/keep.txt"), "keep").unwrap();
        navigation::navigate(
            &app,
            &tab,
            gio::File::for_path(root.join("folder250/nested")),
        );
        until(|| visible_selection(&tab).n_items() == 1).await;
        let descendant = tab.columns.view().unwrap();
        let descendant_model = descendant
            .model()
            .unwrap()
            .downcast::<gtk::MultiSelection>()
            .unwrap();
        descendant_model.select_item(0, true);
        let stable = info(&descendant_model, "keep.txt").unwrap();
        let resets = reset_counter(&descendant_model);
        let parent = column_for(&tab, &root);
        let parent_model = parent
            .model()
            .unwrap()
            .downcast::<gtk::MultiSelection>()
            .unwrap();
        parent.grab_focus();
        columns::focus_widget(&app, &tab, parent.upcast_ref());
        parent_model.select_item(pos(&parent_model, "folder250"), true);
        parent.scroll_to(
            pos(&parent_model, "folder250"),
            None,
            gtk::ListScrollFlags::FOCUS,
            None,
        );
        settle().await;
        commands::rename_selected(&app);
        settle().await;
        let entry = editor(&tab);
        entry.set_text("folder250-renamed");
        entry.emit_activate();
        until(|| root.join("folder250-renamed/nested").exists()).await;
        settle().await;
        assert_eq!(
            column_for(&tab, &root.join("folder250-renamed/nested")),
            descendant
        );
        assert_eq!(info(&descendant_model, "keep.txt"), Some(stable));
        assert_eq!(resets.get(), 0);
        descendant.grab_focus();
        columns::focus_widget(&app, &tab, descendant.upcast_ref());
        assert_eq!(
            tab.current_dir().unwrap().path(),
            Some(root.join("folder250-renamed/nested"))
        );
        assert_eq!(
            commands::selected(&tab)[0].0.path(),
            Some(root.join("folder250-renamed/nested/keep.txt"))
        );
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}
#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn deleted_open_ancestor_returns_to_surviving_column() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::Columns);
        std::fs::create_dir_all(root.join("folder250/nested")).unwrap();
        std::fs::write(root.join("folder250/nested/keep.txt"), "keep").unwrap();
        navigation::navigate(
            &app,
            &tab,
            gio::File::for_path(root.join("folder250/nested")),
        );
        until(|| visible_selection(&tab).n_items() == 1).await;
        let parent = column_for(&tab, &root);
        let parent_model = parent
            .model()
            .unwrap()
            .downcast::<gtk::MultiSelection>()
            .unwrap();
        let stable = info(&parent_model, "folder100").unwrap();
        std::fs::remove_dir_all(root.join("folder250")).unwrap();
        navigation::refresh(&tab);
        until(|| info(&parent_model, "folder250").is_none()).await;
        assert_eq!(
            tab.current_dir().unwrap().path(),
            Some(root.clone()),
            "kept browsing a deleted ancestor"
        );
        assert_eq!(info(&parent_model, "folder100"), Some(stable));
        assert!(focus_is_in(&app, parent.upcast_ref()));
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}

fn seed_search(tab: &Tab, file: &gio::File, query: &str) -> gio::FileInfo {
    tab.search_bar.set_search_mode(true);
    tab.search_entry.set_text(query);
    tab.search.cancel();
    tab.search.active.set(true);
    *tab.search.query.borrow_mut() = query.into();
    let row = file
        .query_info(ATTRS, gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
        .unwrap();
    row.set_attribute_object("standard::file", file);
    tab.search.model.remove_all();
    tab.search.model.append(&row);
    tab.selection.set_model(Some(&tab.search.model));
    tab.selection.select_item(0, true);
    tab.view_stack.set_visible_child_name("search");
    row
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn rename_refreshes_affected_search_results_only() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        until(|| tab.selection.n_items() == 350).await;
        let original = root.join("folder250/search-file.txt");
        let renamed = root.join("folder250/renamed-file.txt");
        std::fs::write(&original, "matching result").unwrap();
        std::fs::write(root.join("folder100/search-file.txt"), "unrelated result").unwrap();
        let file = gio::File::for_path(&original);
        seed_search(&tab, &file, "search-file");
        navigation::new_tab(&app, gio::File::for_path(root.join("folder250")), true);
        let nested = navigation::active_tab(&app);
        until(|| !nested.dir_list.is_loading()).await;
        seed_search(&nested, &file, "file");
        navigation::new_tab(&app, gio::File::for_path(root.join("folder100")), true);
        let unrelated = navigation::active_tab(&app);
        until(|| !unrelated.dir_list.is_loading()).await;
        let untouched = seed_search(
            &unrelated,
            &gio::File::for_path(root.join("folder100/search-file.txt")),
            "search-file",
        );
        app.tab_view
            .set_selected_page(tab.page.borrow().as_ref().unwrap());
        settle().await;
        commands::rename_selected(&app);
        let dialog = app
            .window
            .visible_dialog()
            .and_downcast::<adw::AlertDialog>()
            .unwrap();
        let entry = dialog.extra_child().and_downcast::<gtk::Entry>().unwrap();
        entry.set_text("renamed-file.txt");
        dialog.emit_by_name::<()>("response", &[&"rename"]);
        dialog.close();
        until(|| renamed.exists()).await;
        let old_uri = file.uri();
        for search in [&tab, &nested] {
            assert!(
                (0..search.search.model.n_items()).all(|i| search
                    .search
                    .model
                    .item(i)
                    .and_downcast::<gio::FileInfo>()
                    .and_then(|row| commands::file_from_info(&row, None))
                    .is_none_or(|f| f.uri() != old_uri)),
                "search still exposes the renamed path"
            );
        }
        assert_eq!(tab.search_entry.text(), "search-file");
        assert_eq!(nested.search_entry.text(), "file");
        assert_eq!(unrelated.search.model.item(0), Some(untouched.upcast()));
        assert_eq!(unrelated.selection.selection().size(), 1);
        assert!(
            commands::selected(&tab).is_empty(),
            "renamed file no longer matches search-file"
        );
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn new_folder_does_not_replace_a_newer_selection() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        until(|| tab.selection.n_items() == 350).await;
        tab.selection
            .select_item(pos(&tab.selection, "folder250"), true);
        commands::new_folder(&app);
        tab.selection
            .select_item(pos(&tab.selection, "folder100"), true);
        until(|| info(&tab.selection, "New Folder").is_some()).await;
        assert_eq!(
            commands::selected(&tab)[0].1.name(),
            std::path::Path::new("folder100")
        );
        assert!(
            !children(tab.view_stack.upcast_ref())
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk::Stack>())
                .any(|s| s.visible_child_name().as_deref() == Some("edit")),
            "stale new-folder editor opened"
        );
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn pending_selection_releases_callback_after_tab_destruction() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        until(|| tab.selection.n_items() == 350).await;
        navigation::new_tab(&app, gio::File::for_path(root.join("folder100")), true);
        let token = Rc::new(Cell::new(false));
        let weak_token = Rc::downgrade(&token);
        let callback_token = token.clone();
        navigation::select_names(&tab, &["never-arrives".into()], move || {
            callback_token.set(true)
        });
        drop(token);
        let weak_tab = Rc::downgrade(&tab);
        app.tab_view.close_page(tab.page.borrow().as_ref().unwrap());
        drop(tab);
        settle().await;
        assert!(
            weak_tab.upgrade().is_none(),
            "closed tab retained by pending selection"
        );
        assert!(
            weak_token.upgrade().is_none(),
            "expired-tab timer retained its callback"
        );
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn new_folder_pending_selection_respects_tab_and_input_focus() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        until(|| tab.selection.n_items() == 350).await;
        tab.selection
            .select_item(pos(&tab.selection, "folder250"), true);
        commands::new_folder(&app);
        tab.search_bar.set_search_mode(true);
        tab.search_entry.grab_focus();
        until(|| info(&tab.selection, "New Folder").is_some()).await;
        assert_eq!(
            commands::selected(&tab)[0].1.name(),
            std::path::Path::new("folder250")
        );
        assert!(view::focus_is_text_input(tab.search_entry.upcast_ref()));
        tab.search_bar.set_search_mode(false);
        commands::new_folder(&app);
        navigation::new_tab(&app, gio::File::for_path(root.join("folder100")), true);
        let other = navigation::active_tab(&app);
        until(|| info(&tab.selection, "New Folder 2").is_some()).await;
        assert!(Rc::ptr_eq(&navigation::active_tab(&app), &other));
        assert_eq!(
            commands::selected(&tab)[0].1.name(),
            std::path::Path::new("folder250")
        );
        assert!(
            !children(tab.view_stack.upcast_ref())
                .iter()
                .filter_map(|w| w.downcast_ref::<gtk::Stack>())
                .any(|s| s.visible_child_name().as_deref() == Some("edit"))
        );
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn pending_selection_survives_index_shifts_for_the_same_files() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        until(|| tab.selection.n_items() == 350).await;
        tab.selection
            .select_item(pos(&tab.selection, "folder250"), true);
        let selected = Rc::new(Cell::new(false));
        let done = selected.clone();
        navigation::select_names(&tab, &["later".into()], move || done.set(true));
        app.sort_desc.set(true);
        apply_sort_to(&app, &tab);
        assert_eq!(
            commands::selected(&tab)[0].1.name(),
            std::path::Path::new("folder250")
        );
        std::fs::create_dir(root.join("later")).unwrap();
        until(|| selected.get()).await;
        assert_eq!(
            commands::selected(&tab)[0].1.name(),
            std::path::Path::new("later")
        );
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}

fn rendered_name_cell(view: &gtk::Widget, row: &gio::FileInfo) -> (gtk::Widget, gtk::Label) {
    let image = children(view)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Image>().ok())
        .find(|image| {
            image.is_mapped()
                && unsafe { image.data::<gio::FileInfo>("fs-file-info") }
                    .is_some_and(|bound| unsafe { bound.as_ref() } == row)
        })
        .expect("rendered image for selected row");
    let cell = image.parent().unwrap();
    let label = children(&cell)
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Label>().ok())
        .find(|label| label.has_css_class("fs-name"))
        .unwrap();
    (cell, label)
}
fn assert_rendered_name(view: &gtk::Widget, model: &gtk::MultiSelection, name: &str) {
    let row = info(model, name).unwrap();
    assert_eq!(row.display_name(), name);
    let (cell, label) = rendered_name_cell(view, &row);
    assert_eq!(
        label.text(),
        name,
        "on-screen label still shows the old name"
    );
    assert_eq!(
        cell.tooltip_text().as_deref(),
        Some(name),
        "tooltip still shows the old name"
    );
    assert_eq!(
        unsafe { cell.data::<u32>("fs-position").map(|p| *p.as_ref()) },
        Some(pos(model, name)),
        "cell position tag is stale"
    );
    if row.file_type() == gio::FileType::Directory {
        assert_eq!(
            unsafe {
                cell.data::<String>("fs-dir-name")
                    .map(|p| p.as_ref().clone())
            }
            .as_deref(),
            Some(name),
            "drop target still names the old folder"
        );
    }
}

async fn dialog_rename_rendering(mode: ViewMode) {
    let (_application, app, tab, root) = fixture(mode);
    let model = visible_selection(&tab);
    until(|| model.n_items() == 350).await;
    let view = file_view(&tab, mode);
    let position = pos(&model, "folder250");
    let row = info(&model, "folder250").unwrap();
    show_row(&tab, mode, position);
    model.select_item(position, true);
    settle().await;
    assert_rendered_name(&view, &model, "folder250");
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
        .set_text("a-folder");
    dialog.emit_by_name::<()>("response", &[&"rename"]);
    dialog.close();
    until(|| info(&model, "a-folder").is_some()).await;
    assert_eq!(info(&model, "a-folder"), Some(row));
    assert_eq!(
        commands::selected(&tab)[0].1.name(),
        std::path::Path::new("a-folder")
    );
    assert_rendered_name(&view, &model, "a-folder");
    // Exercise recycling and sibling positions after the renamed row moves to the start.
    show_row(&tab, mode, pos(&model, "folder349"));
    settle().await;
    assert_rendered_name(&view, &model, "folder349");
    show_row(&tab, mode, pos(&model, "a-folder"));
    settle().await;
    assert_rendered_name(&view, &model, "a-folder");
    app.window.close();
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn grid_dialog_rename_updates_rendered_cells() {
    glib::MainContext::default().block_on(dialog_rename_rendering(ViewMode::Grid));
}
#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn list_dialog_rename_updates_rendered_cells() {
    glib::MainContext::default().block_on(dialog_rename_rendering(ViewMode::List));
}
#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn column_dialog_rename_updates_rendered_cells() {
    glib::MainContext::default().block_on(dialog_rename_rendering(ViewMode::Columns));
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn list_metadata_updates_rendered_size_and_modified_cells() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        let model = visible_selection(&tab);
        let path = root.join("size-probe.txt");
        std::fs::write(&path, b"a").unwrap();
        until(|| info(&model, "size-probe.txt").is_some()).await;
        let row = info(&model, "size-probe.txt").unwrap();
        show_row(&tab, ViewMode::List, pos(&model, "size-probe.txt")); settle().await;
        let cell_labels = || children(tab.column_view.upcast_ref()).into_iter().filter_map(|w| w.downcast::<gtk::Label>().ok())
            .filter(|w| unsafe { w.data::<u32>("fs-position").map(|p| *p.as_ref()) } == Some(pos(&model, "size-probe.txt"))).collect::<Vec<_>>();
        std::fs::write(&path, vec![b'x'; 8192]).unwrap();
        std::fs::File::open(&path).unwrap().set_times(std::fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000))).unwrap();
        navigation::refresh(&tab);
        until(|| info(&model, "size-probe.txt").is_some_and(|i| i.size() == 8192 && i.modification_date_time().is_some_and(|d| d.to_unix() == 1_600_000_000))).await;
        assert_eq!(info(&model, "size-probe.txt"), Some(row.clone()));
        let labels = cell_labels();
        let size = labels.iter().find(|w| w.has_css_class("numeric")).unwrap();
        let modified = labels.iter().find(|w| !w.has_css_class("numeric")).unwrap();
        assert_eq!(size.text(), format::human_size(8192), "visible size is stale");
        assert_eq!(modified.text(), format::modified(&row.modification_date_time().unwrap()), "visible timestamp is stale");
        app.window.close(); std::fs::remove_dir_all(root).unwrap();
    });
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn metadata_refresh_keeps_inline_edit_text() {
    glib::MainContext::default().block_on(async {
        let (_application, app, tab, root) = fixture(ViewMode::List);
        let model = visible_selection(&tab);
        until(|| model.n_items() == 350).await;
        let position = pos(&model, "folder250");
        show_row(&tab, ViewMode::List, position);
        model.select_item(position, true);
        settle().await;
        commands::rename_selected(&app);
        settle().await;
        let entry = editor(&tab);
        entry.set_text("draft name");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            root.join("folder250"),
            std::fs::Permissions::from_mode(0o750),
        )
        .unwrap();
        navigation::refresh(&tab);
        until(|| {
            info(&model, "folder250")
                .is_some_and(|i| i.attribute_uint32("unix::mode") & 0o777 == 0o750)
        })
        .await;
        assert_eq!(
            editor(&tab),
            entry,
            "metadata update closed the active editor"
        );
        assert_eq!(entry.text(), "draft name");
        app.window.close();
        std::fs::remove_dir_all(root).unwrap();
    });
}
