//! This PC checks use real GTK containers with isolated recent-history fixtures.
use super::super::{App, Tab, ViewMode, app_state, navigation, open_files, set_view_mode};
use super::*;
use std::time::{Duration, Instant};

async fn settle() {
    glib::timeout_future(Duration::from_millis(100)).await;
    for _ in 0..3 {
        for widget in gtk::Window::list_toplevels() {
            if widget.is_visible() && widget.width() > 0 {
                widget.queue_allocate();
                widget.allocate(widget.width(), widget.height(), -1, None);
            }
        }
        glib::timeout_future(Duration::from_millis(10)).await;
    }
}

async fn until(condition: impl Fn() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "This PC did not settle"
        );
        settle().await;
    }
    settle().await;
}

fn widgets(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut result = vec![root.clone()];
    let mut child = root.first_child();
    while let Some(widget) = child {
        result.extend(widgets(&widget));
        child = widget.next_sibling();
    }
    result
}

fn fixture() -> (adw::Application, Rc<App>, Rc<Tab>, PathBuf) {
    let config =
        PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").expect("isolated configuration"));
    let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").expect("isolated recent history"));
    assert!(config.starts_with(std::env::temp_dir()) && data.starts_with(std::env::temp_dir()));
    std::fs::create_dir_all(config.join("filescope")).unwrap();
    std::fs::write(
        config.join("filescope/settings"),
        "automatic_updates=false\n",
    )
    .unwrap();
    adw::init().unwrap();
    glib::set_application_name("Filescope This PC Tests");
    gtk::Settings::default()
        .unwrap()
        .set_gtk_enable_animations(false);
    let application = adw::Application::builder()
        .application_id("dev.filescope.ThisPcTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    let root = std::env::temp_dir().join(format!("filescope-this-pc-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    open_files(&application, &[gio::File::for_path(&root)]);
    let app = app_state(&application).unwrap();
    app.window.set_default_size(1120, 760);
    let tab = navigation::active_tab(&app);
    (application, app, tab, root)
}

fn seed_recent(root: &std::path::Path, available: &[bool]) -> Vec<String> {
    let manager = gtk::RecentManager::default();
    manager.purge_items().unwrap();
    for (i, exists) in available.iter().enumerate() {
        let name = format!("recent-{i:02}.txt");
        let path = root.join(&name);
        if *exists {
            std::fs::write(&path, "fixture").unwrap();
        }
        let data = gtk::RecentData::new(
            Some(&name),
            None,
            "text/plain",
            "Filescope This PC Tests",
            "filescope %u",
            &[],
            false,
        );
        assert!(manager.add_full(&gio::File::for_path(path).uri(), &data));
    }
    files_we_opened()
        .iter()
        .filter_map(|info| {
            gio::File::for_uri(&info.uri())
                .path()
                .filter(|p| p.exists())
                .map(|_| info.display_name().to_string())
        })
        .collect()
}

fn recent_section(tab: &Tab) -> gtk::Expander {
    widgets(tab.computer_box.upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Expander>().ok())
        .find(|e| e.label().as_deref() == Some("Recent files"))
        .unwrap()
}

fn recent_flow(tab: &Tab) -> gtk::FlowBox {
    widgets(recent_section(tab).upcast_ref())
        .into_iter()
        .find_map(|w| w.downcast().ok())
        .unwrap()
}

fn flow_children(flow: &gtk::FlowBox) -> Vec<gtk::FlowBoxChild> {
    let mut result = Vec::new();
    let mut child = flow.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        result.push(widget.downcast().unwrap());
    }
    result
}

fn tile_names(flow: &gtk::FlowBox) -> Vec<String> {
    flow_children(flow)
        .iter()
        .filter_map(|child| {
            let card = child.child()?;
            if !child.is_visible() || !card.is_visible() {
                return None;
            }
            widgets(&card)
                .into_iter()
                .find_map(|w| w.downcast::<gtk::Label>().ok())
                .map(|l| l.text().to_string())
        })
        .collect()
}

fn card_named(flow: &gtk::FlowBox, name: &str) -> gtk::Button {
    flow_children(flow)
        .into_iter()
        .filter_map(|child| child.child().and_downcast::<gtk::Button>())
        .find(|card| {
            widgets(card.upcast_ref())
                .into_iter()
                .filter_map(|w| w.downcast::<gtk::Label>().ok())
                .any(|l| l.text() == name)
        })
        .unwrap()
}

fn empty_is_visible(tab: &Tab) -> bool {
    widgets(recent_section(tab).upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Label>().ok())
        .any(|label| {
            label.text() == "Files you've previously accessed will show up here"
                && label.is_visible()
        })
}

fn empty_label(tab: &Tab) -> gtk::Label {
    widgets(recent_section(tab).upcast_ref())
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Label>().ok())
        .find(|label| label.text() == "Files you've previously accessed will show up here")
        .unwrap()
}

fn capture(widget: &impl IsA<gtk::Widget>, name: &str) {
    let Some(directory) = std::env::var_os("FILESCOPE_PLACES_SHOTS") else {
        return;
    };
    let widget = widget.upcast_ref();
    let snapshot = gtk::Snapshot::new();
    let parent = widget.parent().expect("mounted widget");
    #[allow(deprecated)]
    let background = widget
        .style_context()
        .lookup_color("window_bg_color")
        .unwrap();
    snapshot.append_color(
        &background,
        &gtk::graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32),
    );
    let bounds = widget.compute_bounds(&parent).unwrap();
    snapshot.translate(&gtk::graphene::Point::new(-bounds.x(), -bounds.y()));
    parent.snapshot_child(widget, &snapshot);
    let node = snapshot.to_node().expect("rendered This PC widget");
    let renderer = widget.native().unwrap().renderer().unwrap();
    renderer
        .render_texture(&node, None)
        .save_to_png(PathBuf::from(directory).join(format!("{name}.png")))
        .unwrap();
}

fn sidebar_fixture() -> (adw::Application, Rc<App>, Rc<Tab>, PathBuf) {
    let (application, app, tab, root) = fixture();
    unsafe {
        std::env::set_var("FILESCOPE_DEMO", "1");
        std::env::set_var("FILESCOPE_SHOT_DIR", &root);
    }
    let paths = [root.join("bookmark-a"), root.join("bookmark-b")];
    for path in &paths {
        std::fs::create_dir(path).unwrap();
    }
    *app.bookmarks.borrow_mut() = paths.to_vec();
    populate_sidebar(&app);
    (application, app, tab, root)
}

fn sidebar_rows(app: &App) -> Vec<gtk::ListBoxRow> {
    let mut rows = Vec::new();
    let mut child = app.sidebar_list.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if let Ok(row) = widget.downcast() {
            rows.push(row);
        }
    }
    rows
}

fn row_name(row: &gtk::ListBoxRow) -> String {
    widgets(row.child().unwrap().upcast_ref())
        .into_iter()
        .find_map(|w| w.downcast::<gtk::Label>().ok())
        .unwrap()
        .text()
        .to_string()
}

fn sidebar_row(app: &App, name: &str) -> gtk::ListBoxRow {
    sidebar_rows(app)
        .into_iter()
        .find(|row| row_name(row) == name)
        .unwrap()
}

fn sidebar_scroller(app: &App) -> gtk::ScrolledWindow {
    let mut widget = app.sidebar_list.parent();
    while let Some(parent) = widget {
        if let Some(scroller) = parent.downcast_ref::<gtk::ScrolledWindow>() {
            return scroller.clone();
        }
        widget = parent.parent();
    }
    panic!("sidebar scroller");
}

fn activate(app: &App, row: &gtk::ListBoxRow) {
    app.sidebar_list.emit_by_name::<()>("row-activated", &[row]);
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn sidebar_this_pc_has_separate_middle_group() {
    let (_application, app, _tab, root) = sidebar_fixture();
    glib::MainContext::default().block_on(async {
        settle().await;
        let rows = sidebar_rows(&app);
        let names: Vec<_> = rows.iter().map(row_name).collect();
        assert!(!names.iter().any(|name| name == "Connections"));
        assert!(app.window.lookup_action("connections").is_none());
        let position = |name: &str| names.iter().position(|n| n == name).unwrap();
        assert!(position("Home") < position("Trash"));
        assert!(
            position("Trash") < position("This PC"),
            "Trash must remain in Places"
        );
        assert!(position("This PC") < position("Devices"));
        assert!(position("Devices") < position("Bookmarks"));
        let computer = sidebar_row(&app, "This PC");
        let header = computer.header().expect("This PC needs its own boundary");
        assert!(header.is::<gtk::Separator>());
        assert!(header.height() > 0 && header.width() > 0);
        assert!(header.margin_top() >= 8 && header.margin_bottom() >= 8);
        assert!(computer.margin_bottom() >= 8);
        let trash = sidebar_row(&app, "Trash")
            .compute_bounds(&app.sidebar_list)
            .unwrap();
        let pc = computer.compute_bounds(&app.sidebar_list).unwrap();
        assert!(
            pc.y() - trash.y() - trash.height() >= 16.0,
            "group boundary has no allocated spacing"
        );
        capture(&sidebar_scroller(&app), "sidebar-normal");
        gtk::Settings::default()
            .unwrap()
            .set_gtk_font_name(Some("Sans 18"));
        app.window.set_default_size(640, 320);
        app.window.allocate(640, 320, -1, None);
        settle().await;
        let scroller = sidebar_scroller(&app);
        let adjustment = scroller.vadjustment();
        assert!(
            adjustment.upper() > adjustment.page_size(),
            "large-text sidebar does not scroll at small heights"
        );
        adjustment.set_value(adjustment.upper() - adjustment.page_size());
        settle().await;
        assert!(adjustment.value() > 0.0);
        assert!(computer.width() > 0 && computer.height() > 0);
        capture(&scroller, "sidebar-small-large-text");
        app.window.close();
    });
    std::fs::remove_dir_all(root).unwrap();
}


#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn sidebar_grouping_preserves_navigation_and_highlight() {
    let (_application, app, tab, root) = sidebar_fixture();
    glib::MainContext::default().block_on(async {
        settle().await;
        let computer = sidebar_row(&app, "This PC");
        let home = sidebar_row(&app, "Home");
        activate(&app, &computer);
        assert_eq!(
            tab.view_stack.visible_child_name().as_deref(),
            Some("computer")
        );
        assert_eq!(app.sidebar_list.selected_row(), Some(computer.clone()));
        assert!(
            widgets(tab.computer_box.upcast_ref())
                .iter()
                .any(|w| w.has_css_class("drive-card")),
            "This PC lost its drives overview"
        );
        activate(&app, &home);
        until(|| tab.current_dir().and_then(|f| f.path()) == Some(glib::home_dir())).await;
        assert_eq!(app.sidebar_list.selected_row(), Some(home.clone()));
        navigation::go_back(&app, &tab);
        assert_eq!(
            tab.view_stack.visible_child_name().as_deref(),
            Some("computer")
        );
        assert_eq!(app.sidebar_list.selected_row(), Some(computer.clone()));
        navigation::go_forward(&app, &tab);
        assert_eq!(app.sidebar_list.selected_row(), Some(home));
        let trash = sidebar_row(&app, "Trash");
        app.sidebar_list.select_row(Some(&trash));
        assert!(trash.grab_focus());
        app.sidebar_list.emit_by_name::<()>(
            "move-cursor",
            &[&gtk::MovementStep::DisplayLines, &1_i32, &false, &false],
        );
        assert_eq!(
            app.sidebar_list.selected_row(),
            Some(computer.clone()),
            "keyboard did not cross the group boundary"
        );
        app.sidebar_list
            .emit_by_name::<()>("activate-cursor-row", &[]);
        assert_eq!(
            tab.view_stack.visible_child_name().as_deref(),
            Some("computer")
        );
        assert_eq!(app.sidebar_list.selected_row(), Some(computer));
        app.window.close();
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn sidebar_grouping_preserves_devices_and_bookmarks() {
    let (_application, app, tab, root) = sidebar_fixture();
    glib::MainContext::default().block_on(async {
        settle().await;
        for (name, path) in [
            ("Filesystem", PathBuf::from("/")),
            ("USB Drive", root.clone()),
            ("bookmark-a", root.join("bookmark-a")),
        ] {
            let row = sidebar_row(&app, name);
            assert_eq!(
                unsafe { row.data::<PathBuf>("path").unwrap().as_ref() },
                &path
            );
            activate(&app, &row);
            until(|| tab.current_dir().and_then(|f| f.path()) == Some(path.clone())).await;
            let selected = app.sidebar_list.selected_row().unwrap();
            assert_eq!(
                unsafe { selected.data::<PathBuf>("path").unwrap().as_ref() },
                &path
            );
        }
        let first = root.join("bookmark-a");
        let second = root.join("bookmark-b");
        assert_eq!(*app.bookmarks.borrow(), [first.clone(), second.clone()]);
        let target_row = sidebar_row(&app, "bookmark-b");
        let controllers = target_row.observe_controllers();
        let target = (0..controllers.n_items())
            .find_map(|i| controllers.item(i).and_downcast::<gtk::DropTarget>())
            .unwrap();
        let value = glib::BoxedValue(first.to_string_lossy().to_string().to_value());
        assert!(target.emit_by_name::<bool>("drop", &[&value, &0.0_f64, &1000.0_f64]));
        until(|| *app.bookmarks.borrow() == [second.clone(), first.clone()]).await;
        let names: Vec<_> = sidebar_rows(&app).iter().map(row_name).collect();
        assert!(
            names.iter().position(|n| n == "bookmark-b")
                < names.iter().position(|n| n == "bookmark-a")
        );
        let row = sidebar_row(&app, "bookmark-a");
        let remove = widgets(row.upcast_ref())
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Button>().ok())
            .find(|b| b.tooltip_text().as_deref() == Some("Remove bookmark"))
            .unwrap();
        remove.emit_clicked();
        assert_eq!(*app.bookmarks.borrow(), [second.clone()]);
        assert_eq!(bookmarks::load(), [second]);
        assert!(
            sidebar_row(&app, "This PC").header().is_some(),
            "rebuilding lost the middle group"
        );
        app.window.close();
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn recent_missing_entries_release_layout_slots() {
    let (_application, app, tab, root) = fixture();
    let expected = seed_recent(&root, &[true, false, true, false, true]);
    glib::MainContext::default().block_on(async {
        for mode in [ViewMode::Grid, ViewMode::List] {
            set_view_mode(&app, mode);
            show_computer(&app, &tab);
            let flow = recent_flow(&tab);
            until(|| tile_names(&flow).len() == expected.len()).await;
            assert_eq!(tile_names(&flow), expected);
            assert_eq!(
                flow_children(&flow).len(),
                expected.len(),
                "missing cards left outer layout slots"
            );
            assert!(
                flow_children(&flow)
                    .iter()
                    .all(|c| c.is_visible() && c.child().unwrap().is_visible())
            );
            assert_eq!(files_we_opened().len(), 5, "offline history was deleted");
        }
        app.window.close();
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn recent_layout_wraps_after_async_removal() {
    let (_application, app, tab, root) = fixture();
    let expected = seed_recent(&root, &[true, false, true, true, false, true, true]);
    glib::MainContext::default().block_on(async {
        show_computer(&app, &tab);
        let flow = recent_flow(&tab);
        until(|| tile_names(&flow).len() == expected.len()).await;
        let mut row_counts = Vec::new();
        for (step, width) in [900, 310, 900].into_iter().enumerate() {
            settle().await;
            flow.allocate(width, 600, -1, None);
            let children = flow_children(&flow);
            assert_eq!(children.len(), expected.len());
            assert_eq!(tile_names(&flow), expected);
            let mut previous: Option<gtk::graphene::Rect> = None;
            let mut rows = 0;
            for child in children {
                let allocation = child.compute_bounds(&flow).unwrap();
                assert!(allocation.width() > 0.0 && allocation.height() > 0.0);
                if let Some(last) = previous {
                    if allocation.y() == last.y() {
                        assert!(
                            allocation.x()
                                <= last.x() + last.width() + flow.column_spacing() as f32 + 1.0,
                            "empty horizontal position"
                        );
                    } else {
                        rows += 1;
                        assert_eq!(
                            allocation.x(),
                            0.0,
                            "new row did not start at the left edge"
                        );
                    }
                } else {
                    rows += 1;
                    assert_eq!(allocation.x(), 0.0);
                }
                previous = Some(allocation);
            }
            row_counts.push(rows);
            capture(&flow, &format!("recent-{step}-{width}"));
        }
        assert!(row_counts[1] > row_counts[0], "narrow layout did not wrap");
        assert_eq!(row_counts[0], row_counts[2], "wide layout did not recover");
        app.window.close();
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn recent_all_missing_shows_empty_state() {
    let (_application, app, tab, root) = fixture();
    seed_recent(&root, &[false, false, false]);
    glib::MainContext::default().block_on(async {
        show_computer(&app, &tab);
        until(|| tile_names(&recent_flow(&tab)).is_empty()).await;
        assert!(
            empty_is_visible(&tab),
            "last missing card did not reveal the empty state"
        );
        assert_eq!(files_we_opened().len(), 3);
        std::fs::write(root.join("recent-01.txt"), "reconnected").unwrap();
        show_computer(&app, &tab);
        until(|| tile_names(&recent_flow(&tab)) == ["recent-01.txt"]).await;
        assert!(!empty_is_visible(&tab));
        app.window.close();
    });
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires isolated GTK and temporary XDG directories"]
fn recent_check_does_not_retain_closed_view() {
    let (_application, app, tab, root) = fixture();
    seed_recent(&root, &[false, true]);
    glib::MainContext::default().block_on(async {
        show_computer(&app, &tab);
        let old_cards: Vec<_> = flow_children(&recent_flow(&tab))
            .iter()
            .map(|c| c.child().unwrap().downgrade())
            .collect();
        let old_card = card_named(&recent_flow(&tab), "recent-01.txt");
        let (release, blocked) = async_channel::bounded(1);
        hide_if_missing(&old_card, &empty_label(&tab), async move {
            blocked.recv().await.unwrap()
        });
        drop(old_card);
        settle().await;
        show_computer(&app, &tab);
        assert!(
            old_cards.iter().all(|weak| weak.upgrade().is_none()),
            "pending checks retained replaced cards"
        );
        release.send(false).await.unwrap();
        until(|| tile_names(&recent_flow(&tab)) == ["recent-01.txt"]).await;
        assert!(
            !empty_is_visible(&tab),
            "old result changed the replacement empty state"
        );
        navigation::new_tab(&app, gio::File::for_path(&root), true);
        let closing = navigation::active_tab(&app);
        show_computer(&app, &closing);
        let card = card_named(&recent_flow(&closing), "recent-01.txt");
        let (release, blocked) = async_channel::bounded(1);
        hide_if_missing(&card, &empty_label(&closing), async move {
            blocked.recv().await.unwrap()
        });
        drop(card);
        settle().await;
        let weak = Rc::downgrade(&closing);
        let page = closing.page.borrow().clone().unwrap();
        drop(closing);
        app.tab_view.close_page(&page);
        drop(page);
        assert!(
            weak.upgrade().is_none(),
            "pending checks retained a closed tab"
        );
        release.send(false).await.unwrap();
        settle().await;
        assert_eq!(tile_names(&recent_flow(&tab)), ["recent-01.txt"]);
        assert_eq!(files_we_opened().len(), 2);
        app.window.close();
    });
    std::fs::remove_dir_all(root).unwrap();
}
