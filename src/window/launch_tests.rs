//! External launch checks exercise native application signals and a private bus.
use super::*;
use std::time::{Duration, Instant};

fn fixture() -> (adw::Application, PathBuf) {
    let data = PathBuf::from(std::env::var_os("XDG_DATA_HOME").unwrap());
    let config = PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").unwrap());
    let root = data.parent().unwrap().to_path_buf();
    assert!(root.starts_with(std::env::temp_dir()));
    assert!(
        root.file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("filescope-")
    );
    assert!(config.starts_with(&root));
    assert!(std::env::var_os("FILESCOPE_SHOT").is_none());
    std::fs::create_dir_all(root.join("folder")).unwrap();
    adw::init().unwrap();
    gtk::Settings::default()
        .unwrap()
        .set_gtk_enable_animations(false);
    let application = adw::Application::builder()
        .application_id("dev.filescope.TrashLaunchTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE | gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    application.connect_open(|app, files, _| open_files(app, files));
    application.connect_activate(|app| open_files(app, &[]));
    application.register(gio::Cancellable::NONE).unwrap();
    (application, root)
}

async fn until(condition: impl Fn() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "launch did not settle"
        );
        glib::timeout_future(Duration::from_millis(20)).await;
    }
}

fn assert_trash(app: &Rc<App>) -> Rc<Tab> {
    assert!(
        !app.tabs.borrow().is_empty(),
        "external opening left no usable tab"
    );
    assert!(app.tab_view.selected_page().is_some());
    let tab = navigation::active_tab(app);
    assert_eq!(
        tab.view_stack.visible_child_name().as_deref(),
        Some("trash")
    );
    assert!(
        tab.trash_view.borrow().is_some(),
        "virtual Trash pane was not created"
    );
    assert_eq!(tab.page.borrow().as_ref().unwrap().title(), "Trash");
    tab
}

fn labels(root: &gtk::Widget) -> Vec<String> {
    let mut result = Vec::new();
    if let Some(label) = root.downcast_ref::<gtk::Label>() {
        result.push(label.label().to_string());
    }
    let mut child = root.first_child();
    while let Some(widget) = child {
        result.extend(labels(&widget));
        child = widget.next_sibling();
    }
    result
}

#[test]
fn trash_root_uri_variants_exclude_other_locations() {
    for uri in ["trash:///", "trash://", "trash:/", "trash:", "TrAsH:///"] {
        assert!(
            is_trash_root(&gio::File::for_uri(uri)),
            "rejected Trash root {uri}"
        );
    }
    for uri in [
        "trash:///child",
        "trash://host/",
        "trash:///?query",
        "trash:///#fragment",
        "file:///tmp",
        "smb://host/share",
    ] {
        assert!(
            !is_trash_root(&gio::File::for_uri(uri)),
            "accepted non-root location {uri}"
        );
    }
}

#[test]
#[ignore = "requires a private GTK/D-Bus session and temporary XDG directories"]
fn external_trash_launch_opens_virtual_trash() {
    let (application, root) = fixture();
    glib::MainContext::default().block_on(async {
        application.open(&[gio::File::for_uri("trash:///")], "");
        let app = app_state(&application).unwrap();
        let trash = assert_trash(&app);
        assert_eq!(app.tabs.borrow().len(), 1);
        until(|| !trash.dir_list.is_loading()).await;
        assert_trash(&app);
        navigation::go_back(&app, &trash);
        assert_eq!(trash.current_dir().unwrap().path(), Some(glib::home_dir()));
        navigation::go_forward(&app, &trash);
        assert_trash(&app);

        application.open(&[gio::File::for_path(root.join("folder"))], "");
        let folder = navigation::active_tab(&app);
        assert_eq!(
            folder.current_dir().unwrap().path(),
            Some(root.join("folder"))
        );
        application.open(&[gio::File::for_uri("trash://")], "");
        let warm = assert_trash(&app);
        until(|| !warm.dir_list.is_loading()).await;
        assert_trash(&app);
        assert!(!Rc::ptr_eq(&warm, &folder));
        assert_eq!(app.tabs.borrow().len(), 3);
        assert_eq!(
            folder.current_dir().unwrap().path(),
            Some(root.join("folder"))
        );
        app.window.close();
    });
}

#[test]
#[ignore = "requires a private GTK/D-Bus session and temporary XDG directories"]
fn unsupported_launch_keeps_usable_window() {
    let (application, root) = fixture();
    glib::MainContext::default().block_on(async {
        application.open(&[gio::File::for_uri("smb://unsupported.example/share")], "");
        let app = app_state(&application).unwrap();
        assert_eq!(
            app.tabs.borrow().len(),
            1,
            "unsupported opening left an invalid shell"
        );
        assert!(app.tab_view.selected_page().is_some());
        let tab = navigation::active_tab(&app);
        assert_eq!(
            tab.view_stack.visible_child_name().as_deref(),
            Some("computer")
        );
        until(|| {
            labels(app.toasts.upcast_ref())
                .iter()
                .any(|text| text.contains("not supported"))
        })
        .await;

        let path = root.join("folder/with space.txt");
        std::fs::write(&path, "local launch").unwrap();
        application.open(&[gio::File::for_path(&path)], "");
        let local = navigation::active_tab(&app);
        until(|| !local.dir_list.is_loading() && local.selection.selection().size() == 1).await;
        assert_eq!(
            local.current_dir().unwrap().path(),
            Some(root.join("folder"))
        );
        assert_eq!(
            commands::selected(&local)[0].1.name(),
            PathBuf::from("with space.txt")
        );
        let count = app.tabs.borrow().len();
        application.open(&[gio::File::for_uri("trash:///child")], "");
        assert_eq!(app.tabs.borrow().len(), count);
        assert!(Rc::ptr_eq(&navigation::active_tab(&app), &local));
        application.activate();
        assert!(Rc::ptr_eq(&navigation::active_tab(&app), &local));
        app.window.close();
    });
}

#[test]
#[ignore = "requires a private GTK/D-Bus session and temporary XDG directories"]
fn file_manager_showfolders_routes_trash() {
    let address = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap();
    assert!(
        address.contains("/tmp/"),
        "FileManager1 test needs a private session bus"
    );
    let (application, root) = fixture();
    let service_dir = glib::user_data_dir().join("dbus-1/services");
    std::fs::create_dir_all(&service_dir).unwrap();
    std::fs::write(
        service_dir.join("org.freedesktop.FileManager1.service"),
        "[D-BUS Service]\nName=org.freedesktop.FileManager1\n",
    )
    .unwrap();
    crate::file_manager::own(&application);
    let bus = application.dbus_connection().unwrap();
    let destination = bus.unique_name().unwrap();
    glib::MainContext::default().block_on(async {
        let request = |method: &'static str, uris: Vec<String>| {
            let bus = bus.clone();
            let destination = destination.clone();
            async move {
                let reply = bus
                    .call_future(
                        Some(&destination),
                        "/org/freedesktop/FileManager1",
                        crate::file_manager::BUS_NAME,
                        method,
                        Some(&(uris, String::new()).to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        5000,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    reply,
                    ().to_variant(),
                    "FileManager1 did not complete its reply"
                );
                glib::timeout_future(Duration::from_millis(30)).await;
            }
        };
        request("ShowFolders", vec!["trash:///".into()]).await;
        until(|| app_state(&application).is_some()).await;
        let app = app_state(&application).unwrap();
        assert_trash(&app);
        assert_eq!(app.tabs.borrow().len(), 1);
        request(
            "ShowFolders",
            vec![gio::File::for_path(root.join("folder")).uri().to_string()],
        )
        .await;
        let folder = navigation::active_tab(&app);
        assert_eq!(
            folder.current_dir().unwrap().path(),
            Some(root.join("folder"))
        );
        request("ShowFolders", vec!["trash://".into()]).await;
        assert_trash(&app);
        assert_eq!(app.tabs.borrow().len(), 3);

        let local = root.join("folder/selected file.txt");
        std::fs::write(&local, "selected through FileManager1").unwrap();
        request(
            "ShowItems",
            vec![gio::File::for_path(&local).uri().to_string()],
        )
        .await;
        let selected = navigation::active_tab(&app);
        until(|| selected.selection.selection().size() == 1).await;
        assert_eq!(
            selected.current_dir().unwrap().path(),
            Some(root.join("folder"))
        );
        assert_eq!(
            commands::selected(&selected)[0].1.name(),
            PathBuf::from("selected file.txt")
        );
        request(
            "ShowItemProperties",
            vec![gio::File::for_path(&local).uri().to_string()],
        )
        .await;
        until(|| app.window.visible_dialog().is_some()).await;
        app.window.visible_dialog().unwrap().close();
        crate::file_manager::release(&application);
        let owned = bus
            .call_future(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                Some(&(crate::file_manager::BUS_NAME,).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                5000,
            )
            .await
            .unwrap();
        assert_eq!(owned.child_value(0).get::<bool>(), Some(false));
        app.window.close();
    });
}

#[test]
#[ignore = "requires a private GTK/D-Bus session and temporary XDG directories"]
fn external_trash_preserves_restore_identity() {
    let (application, root) = fixture();
    let trash = crate::trash::home_trash();
    std::fs::create_dir_all(trash.join("files")).unwrap();
    std::fs::create_dir_all(trash.join("info")).unwrap();
    for (storage, parent, content) in [
        ("storage-a", "original-a", "first"),
        ("storage-b", "original-b", "second"),
    ] {
        let original = root.join(parent).join("same.txt");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(trash.join("files").join(storage), content).unwrap();
        std::fs::write(
            trash.join("info").join(format!("{storage}.trashinfo")),
            format!(
                "[Trash Info]\nPath={}\nDeletionDate=2026-10-30T10:00:00\n",
                original.display()
            ),
        )
        .unwrap();
    }
    glib::MainContext::default().block_on(async {
        application.open(&[gio::File::for_uri("trash:///")], "");
        let app = app_state(&application).unwrap();
        let tab = assert_trash(&app);
        let pane = tab.trash_view.borrow().clone().unwrap();
        assert_eq!(pane.selection.n_items(), 2);
        for pos in 0..pane.selection.n_items() {
            let info = pane
                .selection
                .item(pos)
                .and_downcast::<gio::FileInfo>()
                .unwrap();
            assert_eq!(info.display_name(), "same.txt");
            if info.name() == std::path::Path::new("storage-a") {
                assert_eq!(
                    info.attribute_string("trash::orig-path").as_deref(),
                    root.join("original-a/same.txt").to_str()
                );
                pane.selection.select_item(pos, true);
            }
        }
        assert_eq!(pane.selection.selection().size(), 1);
        pane.widget.activate_action("trash.restore", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("original-a/same.txt")).unwrap(),
            "first"
        );
        assert!(!root.join("original-b/same.txt").exists());
        assert_eq!(
            std::fs::read_to_string(trash.join("files/storage-b")).unwrap(),
            "second"
        );
        assert_eq!(pane.selection.n_items(), 1);
        assert_eq!(
            pane.selection
                .item(0)
                .and_downcast::<gio::FileInfo>()
                .unwrap()
                .name(),
            PathBuf::from("storage-b")
        );
        app.window.close();
    });
}
