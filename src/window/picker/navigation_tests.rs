mod navigation_tests {
    use super::*;

    fn widgets(root: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut result = vec![root.clone()];
        let mut child = root.first_child();
        while let Some(widget) = child {
            result.extend(widgets(&widget));
            child = widget.next_sibling();
        }
        result
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_path_completion_submits_exact_folder() {
        let app = app();
        let folder = fixture();
        let target = folder.join("alpha with space");
        std::fs::create_dir_all(&target).unwrap();
        let window = show(
            &app,
            "OpenFile",
            "test.completion",
            "",
            "Open",
            &options(&folder).end(),
            |code, _| assert_eq!(code, 1),
        );
        let picker = state(&window);
        picker.navigate(&folder);
        load(&picker);
        assert!(
            !picker.navigation.back.is_sensitive(),
            "reopening the same folder duplicated history"
        );
        picker.path.grab_focus();
        picker.path.set_text(&format!("{}/al", folder.display()));
        pump_until(|| {
            widgets(picker.path.upcast_ref())
                .iter()
                .any(|widget| widget.is::<gtk::ListBox>())
        });
        let list = widgets(picker.path.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::ListBox>().ok())
            .unwrap();
        list.emit_by_name::<()>("row-activated", &[&list.row_at_index(0).unwrap()]);
        assert_eq!(picker.path.text(), format!("{}/", target.display()));
        assert_eq!(
            picker.folder(),
            Some(folder.clone()),
            "completion must leave submission to the picker"
        );
        picker.path.emit_activate();
        pump_until(|| !picker.location.unresolved.get() && picker.folder() == Some(target.clone()));
        assert_eq!(picker.folder(), Some(target.clone()));
        picker.path.set_text("..");
        picker.path.emit_activate();
        pump_until(|| !picker.location.unresolved.get() && picker.folder() == Some(folder.clone()));
        assert_eq!(picker.folder(), Some(folder.clone()));
        picker.path.set_text("~");
        picker.path.emit_activate();
        pump_until(|| !picker.location.unresolved.get() && picker.folder() == Some(glib::home_dir()));
        assert_eq!(picker.folder(), Some(glib::home_dir()));
        picker
            .path
            .set_text(gio::File::for_path(&target).uri().as_str());
        picker.path.emit_activate();
        pump_until(|| !picker.location.unresolved.get() && picker.folder() == Some(target.clone()));
        assert_eq!(picker.folder(), Some(target));
        window.destroy();
        std::fs::remove_dir_all(folder).unwrap();
    }

    fn remember(app: &adw::Application, app_id: &str, folder: &Path) {
        let completed = Rc::new(Cell::new(false));
        let done = completed.clone();
        let window = show(
            app,
            "OpenFile",
            app_id,
            "",
            "Open",
            &options(folder).end(),
            move |code, _| {
                assert_eq!(code, 0);
                done.set(true);
            },
        );
        let picker = state(&window);
        load(&picker);
        select(&picker, &folder.join("one.txt"), true);
        picker.accept();
        pump_until(|| completed.get());
    }

    fn open_places(root: &gtk::Widget) -> bool {
        if let Some(button) = root.downcast_ref::<gtk::MenuButton>()
            && button.label().as_deref() == Some("Places")
        {
            button.popup();
            return true;
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if open_places(&widget) {
                return true;
            }
            child = widget.next_sibling();
        }
        false
    }

    fn key(window: &gtk::Window, key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType) -> bool {
        (0..window.observe_controllers().n_items())
            .find_map(|i| {
                window
                    .observe_controllers()
                    .item(i)
                    .and_downcast::<gtk::EventControllerKey>()
            })
            .unwrap()
            .emit_by_name("key-pressed", &[&key, &0u32, &modifiers])
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_requested_location_precedes_remembered_folder() {
        let app = app();
        let remembered = fixture();
        let requested = fixture();
        remember(&app, "test.requested", &remembered);
        let window = show(
            &app,
            "OpenFile",
            "test.requested",
            "",
            "Open",
            &glib::VariantDict::new(None).end(),
            |_, _| {},
        );
        assert_eq!(
            state(&window).folder(),
            Some(remembered.clone()),
            "no-hint request did not use the caller's last accepted folder"
        );
        window.destroy();
        let window = show(
            &app,
            "OpenFile",
            "test.requested",
            "",
            "Open",
            &options(&requested).end(),
            |_, _| {},
        );
        assert_eq!(state(&window).folder(), Some(requested.clone()));
        window.destroy();
        let window = show(
            &app,
            "OpenFile",
            "test.requested",
            "",
            "Open",
            &options(&requested.join("missing")).end(),
            |_, _| {},
        );
        let picker = state(&window);
        assert_eq!(picker.folder(), Some(remembered.clone()));
        assert!(picker.error.label().contains("unavailable"));
        window.destroy();
        std::fs::remove_dir_all(remembered).unwrap();
        std::fs::remove_dir_all(requested).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_remembers_folders_per_application() {
        let app = app();
        let one = fixture();
        let two = fixture();
        remember(&app, "test.one", &one);
        remember(&app, "test.two", &two);
        let window = show(
            &app,
            "OpenFile",
            "test.one",
            "",
            "Open",
            &options(&one).end(),
            |code, _| assert_eq!(code, 1),
        );
        let picker = state(&window);
        picker.navigate(&two);
        picker.finish(1, empty_results());
        for (app_id, expected) in [
            ("test.one", one.clone()),
            ("test.two", two.clone()),
            ("", glib::home_dir()),
        ] {
            let window = show(
                &app,
                "OpenFile",
                app_id,
                "",
                "Open",
                &glib::VariantDict::new(None).end(),
                |_, _| {},
            );
            assert_eq!(state(&window).folder(), Some(expected));
            window.destroy();
        }
        remember(&app, "", &one);
        let window = show(
            &app,
            "OpenFile",
            "",
            "",
            "Open",
            &glib::VariantDict::new(None).end(),
            |_, _| {},
        );
        assert_eq!(state(&window).folder(), Some(glib::home_dir()));
        window.destroy();
        let memory = glib::user_config_dir().join("filescope/picker-locations.json");
        pump_until(|| {
            let Ok(bytes) = std::fs::read(&memory) else {
                return false;
            };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                return false;
            };
            let path = |id: &str| {
                value[id]
                    .as_array()
                    .and_then(|values| values.first())
                    .and_then(|value| value.as_str())
                    .and_then(|uri| gio::File::for_uri(uri).path())
            };
            path("test.one") == Some(one.clone())
                && path("test.two") == Some(two.clone())
                && value.get("").is_none()
        });
        std::fs::remove_dir_all(one).unwrap();
        std::fs::remove_dir_all(two).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_places_and_keyboard_preserve_request_modes() {
        let app = app();
        let folder = fixture();
        let favorite = folder.join("favorite-folder");
        std::fs::create_dir_all(favorite.join("nested")).unwrap();
        std::fs::write(favorite.join("not-a-folder.txt"), "mode must hide this").unwrap();
        assert!(glib::user_config_dir().starts_with(std::env::temp_dir()));
        crate::bookmarks::save(std::slice::from_ref(&favorite));
        let dict = options(&folder);
        dict.insert("directory", true);
        let filter: FilterSpec = ("Text".into(), vec![(0, "*.txt".into())]);
        dict.insert("filters", vec![filter.clone()]);
        let choices: Vec<ChoiceSpec> =
            vec![("readonly".into(), "Read only".into(), vec![], "true".into())];
        dict.insert("choices", choices);
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.places",
            "",
            "Choose folder",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        assert!(
            open_places(window.upcast_ref()),
            "picker has no favorites/recent places control"
        );
        assert!(click(window.upcast_ref(), "favorite-folder"));
        pump_until(|| !picker.source.is_loading() && picker.selection.n_items() == 1);
        assert_eq!(picker.folder(), Some(favorite.clone()));
        picker.path.grab_focus();
        picker.path.set_text("editing stays here");
        assert!(!key(
            &window,
            gtk::gdk::Key::Left,
            gtk::gdk::ModifierType::empty()
        ));
        assert_eq!(picker.path.text(), "editing stays here");
        assert!(key(
            &window,
            gtk::gdk::Key::Escape,
            gtk::gdk::ModifierType::empty()
        ));
        assert!(
            result.borrow().is_none(),
            "Escape while editing cancelled the request"
        );
        assert!(key(
            &window,
            gtk::gdk::Key::Left,
            gtk::gdk::ModifierType::ALT_MASK
        ));
        pump_until(|| !picker.location.unresolved.get() && picker.folder() == Some(folder.clone()));
        assert_eq!(picker.folder(), Some(folder.clone()));
        assert!(key(
            &window,
            gtk::gdk::Key::Right,
            gtk::gdk::ModifierType::ALT_MASK
        ));
        pump_until(|| !picker.location.unresolved.get() && picker.folder() == Some(favorite.clone()));
        assert_eq!(picker.folder(), Some(favorite.clone()));
        picker.accept();
        pump_until(|| result.borrow().is_some());
        let result = result.borrow();
        let (code, values) = result.as_ref().unwrap();
        assert_eq!(*code, 0);
        let values = glib::VariantDict::new(Some(values));
        let uris = values.lookup::<Vec<String>>("uris").unwrap().unwrap();
        assert_eq!(gio::File::for_uri(&uris[0]).path(), Some(favorite));
        assert_eq!(
            values.lookup::<FilterSpec>("current_filter").unwrap(),
            Some(filter)
        );
        assert_eq!(
            values.lookup::<Vec<(String, String)>>("choices").unwrap(),
            Some(vec![("readonly".into(), "true".into())])
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_navigation_releases_cancelled_request() {
        let app = app();
        let one = fixture();
        let two = fixture();
        let a = show(
            &app,
            "OpenFile",
            "test.cancelled",
            "",
            "One",
            &options(&one).end(),
            |code, _| assert_eq!(code, 1),
        );
        let b = show(
            &app,
            "OpenFile",
            "test.accepted",
            "",
            "Two",
            &options(&two).end(),
            |code, _| assert_eq!(code, 0),
        );
        let first = state(&a);
        let weak = Rc::downgrade(&first);
        let second = state(&b);
        load(&second);
        first.navigate(&two);
        a.destroy();
        drop(first);
        assert!(weak.upgrade().is_none());
        assert_eq!(second.folder(), Some(two.clone()));
        select(&second, &two.join("one.txt"), true);
        second.accept();
        for (app_id, expected) in [
            ("test.cancelled", glib::home_dir()),
            ("test.accepted", two.clone()),
        ] {
            let window = show(
                &app,
                "OpenFile",
                app_id,
                "",
                "Open",
                &glib::VariantDict::new(None).end(),
                |_, _| {},
            );
            assert_eq!(state(&window).folder(), Some(expected));
            window.destroy();
        }
        std::fs::remove_dir_all(one).unwrap();
        std::fs::remove_dir_all(two).unwrap();
    }
}
