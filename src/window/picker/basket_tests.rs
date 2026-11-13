mod basket_tests {
    use super::*;

    fn multiple(folder: &Path) -> glib::VariantDict {
        let dict = options(folder);
        dict.insert("multiple", true);
        dict
    }
    fn paths(values: &glib::Variant) -> Vec<PathBuf> {
        glib::VariantDict::new(Some(values))
            .lookup::<Vec<String>>("uris")
            .unwrap()
            .unwrap()
            .iter()
            .map(|uri| gio::File::for_uri(uri).path().unwrap())
            .collect()
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
    fn remove_button(window: &gtk::Window, path: &Path) -> Option<gtk::Button> {
        let expected = format!("Remove {}", path.display());
        widgets(window.upcast_ref())
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
            .find(|button| button.tooltip_text().as_deref() == Some(expected.as_str()))
    }
    fn count(window: &gtk::Window, expected: usize) -> bool {
        let expected = format!("Selected files: {expected}");
        widgets(window.upcast_ref())
            .iter()
            .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
            .any(|label| label.label() == expected)
    }

    #[test]
    #[ignore = "requires isolated GTK and unavailable-index fixture from tests/gtk.py"]
    fn picker_basket_survives_folder_and_search_navigation() {
        let app = app();
        let folder = fixture();
        let child = folder.join("folder/child.txt");
        std::fs::write(&child, "child").unwrap();
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.basket",
            "",
            "Open",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        select(&picker, &folder.join("one.txt"), true);
        picker.navigate(&folder.join("folder"));
        load(&picker);
        select(&picker, &child, true);
        picker.navigate(&folder);
        load(&picker);
        let entry = widgets(window.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::SearchEntry>().ok())
            .unwrap();
        entry.set_text("two");
        entry.emit_by_name::<()>("search-changed", &[]);
        pump_until(|| picker.selection.n_items() == 1);
        select(&picker, &folder.join("two.txt"), true);
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(
            paths(&result.borrow().as_ref().unwrap().1),
            [folder.join("one.txt"), child, folder.join("two.txt")]
        );
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_basket_distinguishes_same_named_locations() {
        let app = app();
        let folder = fixture();
        for parent in ["first", "second"] {
            std::fs::create_dir_all(folder.join(parent)).unwrap();
            std::fs::write(folder.join(parent).join("same.txt"), parent).unwrap();
        }
        let first = folder.join("first/same.txt");
        let second = folder.join("second/same.txt");
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.basket",
            "",
            "Open",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        for path in [&first, &second, &first] {
            picker.navigate(path.parent().unwrap());
            load(&picker);
            select(&picker, path, true);
        }
        assert!(
            count(&window, 2),
            "same-named files were lost or the same URI was duplicated"
        );
        pump_until(|| remove_button(&window, &first).is_some());
        remove_button(&window, &first).unwrap().emit_clicked();
        assert!(count(&window, 1));
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [second]);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "first");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_basket_respects_modes_and_revalidates_entries() {
        let app = app();
        let folder = fixture();
        let child = folder.join("folder/child.txt");
        std::fs::write(&child, "child").unwrap();
        let dict = multiple(&folder);
        let filters: Vec<FilterSpec> = vec![
            ("Text".into(), vec![(0, "*.txt".into())]),
            ("PNG".into(), vec![(0, "*.png".into())]),
        ];
        dict.insert("filters", filters);
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.invalid-basket",
            "",
            "Open",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        let missing = folder.join("one.txt");
        select(&picker, &missing, true);
        picker.navigate(&folder.join("folder"));
        load(&picker);
        std::fs::remove_file(&missing).unwrap();
        picker.accept();
        pump_until(|| !picker.error.label().is_empty());
        assert!(result.borrow().is_none());
        assert!(
            picker.error.label().contains("one.txt"),
            "unavailable basket item was not identified"
        );
        pump_until(|| remove_button(&window, &missing).is_some());
        remove_button(&window, &missing).unwrap().emit_clicked();
        select(&picker, &child, true);
        picker.filters.set_selected(1);
        picker.accept();
        pump_until(|| picker.error.label().contains("child.txt"));
        assert!(result.borrow().is_none());
        picker.filters.set_selected(0);
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [child.clone()]);

        for directory in [false, true] {
            let dict = options(&folder);
            dict.insert("directory", directory);
            dict.insert("multiple", directory);
            let result = Rc::new(RefCell::new(None));
            let output = result.clone();
            let window = show(
                &app,
                "OpenFile",
                "test.other-mode",
                "",
                "Open",
                &dict.end(),
                move |code, values| {
                    output.replace(Some((code, values)));
                },
            );
            let picker = state(&window);
            load(&picker);
            assert!(
                !widgets(window.upcast_ref())
                    .iter()
                    .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                    .any(|label| label.label().starts_with("Selected files:"))
            );
            let target = if directory {
                folder.join("folder")
            } else {
                folder.join("two.txt")
            };
            select(&picker, &target, true);
            picker.accept();
            pump_until(|| result.borrow().is_some());
            assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [target]);
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_baskets_are_request_scoped() {
        let app = app();
        let folder = fixture();
        std::fs::write(folder.join("folder/child.txt"), "child").unwrap();
        let first = Rc::new(RefCell::new(Vec::new()));
        let output = first.clone();
        let a = show(
            &app,
            "OpenFile",
            "test.same-caller",
            "",
            "One",
            &multiple(&folder).end(),
            move |code, values| output.borrow_mut().push((code, values)),
        );
        let second = Rc::new(RefCell::new(None));
        let output = second.clone();
        let b = show(
            &app,
            "OpenFile",
            "test.same-caller",
            "",
            "Two",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let one = state(&a);
        let two = state(&b);
        load(&one);
        load(&two);
        select(&one, &folder.join("one.txt"), true);
        one.navigate(&folder.join("folder"));
        select(&two, &folder.join("two.txt"), true);
        let weak = Rc::downgrade(&one);
        one.accept();
        a.destroy();
        drop(one);
        two.accept();
        pump_until(|| second.borrow().is_some());
        assert_eq!(
            paths(&second.borrow().as_ref().unwrap().1),
            [folder.join("two.txt")]
        );
        assert!(weak.upgrade().is_none());
        assert_eq!(first.borrow().len(), 1);
        assert_eq!(first.borrow()[0].0, 1);
        assert_eq!(first.borrow()[0].1.n_children(), 0);
        let fresh = Rc::new(RefCell::new(None));
        let output = fresh.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.same-caller",
            "",
            "Fresh",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        picker.accept();
        assert!(fresh.borrow().is_none());
        window.destroy();
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_basket_rechecks_replacements_and_keeps_symlink_uris() {
        let app = app();
        let folder = fixture();
        let alias = folder.join("alias.txt");
        std::os::unix::fs::symlink("one.txt", &alias).unwrap();
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.basket-links",
            "",
            "Open",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        select(&picker, &folder.join("one.txt"), true);
        select(&picker, &alias, false);
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(
            paths(&result.borrow().as_ref().unwrap().1),
            [folder.join("one.txt"), alias]
        );

        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.basket-replaced",
            "",
            "Open",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        let path = folder.join("two.txt");
        select(&picker, &path, true);
        std::fs::rename(&path, folder.join("old-two.txt")).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        glib::MainContext::default()
            .block_on(glib::timeout_future(std::time::Duration::from_millis(150)));
        assert!(
            count(&window, 1),
            "refresh collected an item the user did not select"
        );
        picker.accept();
        pump_until(|| !picker.error.label().is_empty() || result.borrow().is_some());
        assert!(
            result.borrow().is_none(),
            "a replacement file inherited a retained selection"
        );
        assert!(picker.error.label().contains("changed"));
        window.destroy();
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_basket_rechecks_symlink_retarget_after_final_access() {
        let app = app();
        let folder = fixture();
        let original = folder.join("one.txt");
        let other = folder.join("two.txt");
        let alias = folder.join("alias.txt");
        std::os::unix::fs::symlink("one.txt", &alias).unwrap();
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.basket-retarget",
            "",
            "Open",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        select(&picker, &original, true);
        select(&picker, &alias, false);
        let (entered, reached) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        picker
            .acceptance
            .grant_gate
            .replace(Some((entered, release)));
        picker.accept();
        pump_until(|| reached.try_recv().is_ok());
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink("two.txt", &alias).unwrap();
        resume.send(()).unwrap();
        pump_until(|| !picker.acceptance.busy.get() || result.borrow().is_some());
        assert!(
            result.borrow().is_none(),
            "a retargeted symlink was granted: {:?}",
            result.borrow().as_ref().map(|(_, values)| paths(values))
        );
        assert!(
            picker.error.label().contains("alias.txt"),
            "the retargeted symlink was not identified: {}",
            picker.error.label()
        );
        pump_until(|| remove_button(&window, &alias).is_some());
        remove_button(&window, &alias).unwrap().emit_clicked();
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [original.clone()]);
        assert_eq!(std::fs::read_to_string(&original).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "two");
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_basket_removal_invalidates_pending_grant() {
        let app = app();
        let folder = fixture();
        let first = folder.join("first/one.txt");
        let second = folder.join("second/two.txt");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&first, "one").unwrap();
        std::fs::write(&second, "two").unwrap();
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.basket-remove",
            "",
            "Open",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        picker.navigate(first.parent().unwrap());
        load(&picker);
        select(&picker, &first, true);
        picker.navigate(second.parent().unwrap());
        load(&picker);
        select(&picker, &second, true);
        assert!(count(&window, 2));
        let (entered, reached) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        picker
            .acceptance
            .grant_gate
            .replace(Some((entered, release)));
        picker.accept();
        pump_until(|| reached.try_recv().is_ok());
        pump_until(|| remove_button(&window, &first).is_some());
        remove_button(&window, &first).unwrap().emit_clicked();
        assert!(count(&window, 1));
        resume.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while result.borrow().is_none() && std::time::Instant::now() < deadline {
            while glib::MainContext::default().pending() {
                glib::MainContext::default().iteration(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            result.borrow().is_none(),
            "caller received a file removed during the final access check: {:?}",
            result.borrow().as_ref().map(|(_, values)| paths(values))
        );
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [second]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_basket_rechecks_identity_after_final_access() {
        let app = app();
        let folder = fixture();
        let first = folder.join("first/one.txt");
        let second = folder.join("second/two.txt");
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::create_dir_all(second.parent().unwrap()).unwrap();
        std::fs::write(&first, "selected").unwrap();
        std::fs::write(&second, "kept").unwrap();
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "OpenFile",
            "test.basket-identity",
            "",
            "Open",
            &multiple(&folder).end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        picker.navigate(first.parent().unwrap());
        load(&picker);
        select(&picker, &first, true);
        picker.navigate(second.parent().unwrap());
        load(&picker);
        select(&picker, &second, true);
        let (entered, reached) = std::sync::mpsc::channel();
        let (resume, release) = std::sync::mpsc::channel();
        picker
            .acceptance
            .grant_gate
            .replace(Some((entered, release)));
        picker.accept();
        pump_until(|| reached.try_recv().is_ok());
        let kept = folder.join("first/original-one.txt");
        std::fs::rename(&first, &kept).unwrap();
        std::fs::write(&first, "replacement that was never selected").unwrap();
        resume.send(()).unwrap();
        pump_until(|| !picker.acceptance.busy.get() || result.borrow().is_some());
        assert!(
            result.borrow().is_none(),
            "a substituted earlier-folder file was granted: {:?}",
            result.borrow().as_ref().map(|(_, values)| paths(values))
        );
        assert!(
            picker.error.label().contains("one.txt"),
            "the changed item was not identified: {}",
            picker.error.label()
        );
        assert!(picker.error.label().contains("changed"));
        pump_until(|| remove_button(&window, &first).is_some());
        remove_button(&window, &first).unwrap().emit_clicked();
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [second.clone()]);
        assert_eq!(
            std::fs::read_to_string(&first).unwrap(),
            "replacement that was never selected"
        );
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), "selected");
        std::fs::remove_dir_all(folder).unwrap();
    }
}
