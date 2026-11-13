mod search_tests {
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
    fn search(window: &gtk::Window) -> gtk::SearchEntry {
        widgets(window.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast().ok())
            .expect("picker has no scoped search entry")
    }
    fn query(entry: &gtk::SearchEntry, text: &str) {
        entry.set_text(text);
        entry.emit_by_name::<()>("search-changed", &[]);
    }
    fn visible(picker: &Picker) -> Vec<PathBuf> {
        (0..picker.selection.n_items())
            .filter_map(|i| {
                picker
                    .selection
                    .item(i)
                    .and_downcast::<gio::FileInfo>()?
                    .attribute_object("standard::file")
                    .and_downcast::<gio::File>()?
                    .path()
            })
            .collect()
    }
    fn preview_text(window: &gtk::Window) -> String {
        widgets(window.upcast_ref())
            .iter()
            .filter_map(|widget| widget.downcast_ref::<gtk::TextView>())
            .map(|view| {
                let buffer = view.buffer();
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), true)
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
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

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_search_respects_scope_and_request_filters() {
        let app = app();
        let folder = fixture();
        let outside = fixture();
        std::fs::write(folder.join("folder/match.txt"), "inside").unwrap();
        std::fs::write(folder.join("folder/match.png"), "filtered").unwrap();
        std::fs::create_dir_all(folder.join("folder/match-directory")).unwrap();
        std::fs::write(outside.join("match-outside.txt"), "outside").unwrap();
        for directory in [false, true] {
            let dict = options(&folder);
            dict.insert("directory", directory);
            let filter: FilterSpec = ("Text".into(), vec![(0, "*.txt".into())]);
            dict.insert("filters", vec![filter.clone()]);
            let result = Rc::new(RefCell::new(None));
            let output = result.clone();
            let window = show(
                &app,
                "OpenFile",
                "test.search",
                "",
                "Open",
                &dict.end(),
                move |code, values| {
                    output.replace(Some((code, values)));
                },
            );
            let picker = state(&window);
            load(&picker);
            let entry = search(&window);
            query(&entry, "match");
            let target = folder.join(if directory {
                "folder/match-directory"
            } else {
                "folder/match.txt"
            });
            pump_until(|| visible(&picker).contains(&target));
            let files = visible(&picker);
            assert!(files.iter().all(|path| path.starts_with(&folder)));
            assert!(!files.contains(&folder.join("folder/match.png")));
            if directory {
                assert!(files.iter().all(|path| path.is_dir()));
            }
            assert!(
                widgets(window.upcast_ref())
                    .iter()
                    .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                    .any(|label| label.label().contains(folder.to_str().unwrap())
                        && label.label().contains("results"))
            );
            select(&picker, &target, true);
            picker.accept();
            pump_until(|| result.borrow().is_some());
            let result = result.borrow();
            let (code, values) = result.as_ref().unwrap();
            assert_eq!(*code, 0);
            assert_eq!(paths(values), [target]);
            assert_eq!(
                glib::VariantDict::new(Some(values))
                    .lookup::<FilterSpec>("current_filter")
                    .unwrap(),
                Some(filter)
            );
        }
        let dict = options(&folder);
        dict.insert("current_name", "untouched.txt");
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFile",
            "test.search-save",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        let entry = search(&window);
        query(&entry, "match.txt");
        let target = folder.join("folder/match.txt");
        pump_until(|| visible(&picker).contains(&target));
        select(&picker, &target, true);
        picker.accept();
        wait_overwrite(&picker);
        assert!(picker.overwrite.get());
        assert!(click(
            window
                .downcast_ref::<adw::ApplicationWindow>()
                .unwrap()
                .visible_dialog()
                .unwrap()
                .upcast_ref(),
            "Replace"
        ));
        pump_until(|| result.borrow().is_some());
        assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [target]);
        let reopened = show(
            &app,
            "SaveFile",
            "test.search-save",
            "",
            "Save",
            &glib::VariantDict::new(None).end(),
            |code, _| assert_eq!(code, 1),
        );
        assert_eq!(
            state(&reopened).folder(),
            Some(folder.join("folder")),
            "remember the actual accepted save folder"
        );
        reopened.destroy();
        std::fs::remove_dir_all(folder).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_clear_search_restores_folder_context() {
        let app = app();
        let folder = fixture();
        let window = show(
            &app,
            "OpenFile",
            "test.clear-search",
            "",
            "Open",
            &options(&folder).end(),
            |code, _| assert_eq!(code, 1),
        );
        let picker = state(&window);
        load(&picker);
        select(&picker, &folder.join("one.txt"), true);
        let entry = search(&window);
        query(&entry, "two");
        pump_until(|| visible(&picker) == [folder.join("two.txt")]);
        query(&entry, "");
        pump_until(|| visible(&picker).contains(&folder.join("one.txt")));
        assert_eq!(picker.folder(), Some(folder.clone()));
        assert_eq!(picker.selected(), [folder.join("one.txt")]);
        query(&entry, "obsolete");
        query(&entry, "");
        assert!(visible(&picker).contains(&folder.join("one.txt")));
        entry.set_text("two");
        picker.navigate(&folder);
        assert!(
            entry.text().is_empty(),
            "a queued search survived navigation to a new context"
        );
        load(&picker);
        window.destroy();
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_preview_rejects_stale_results() {
        let app = app();
        let folder = fixture();
        std::fs::write(folder.join("one.txt"), "ONE OBSOLETE MARKER".repeat(5000)).unwrap();
        std::fs::write(folder.join("two.txt"), "TWO CURRENT MARKER").unwrap();
        let window = show(
            &app,
            "OpenFile",
            "test.preview",
            "",
            "Open",
            &options(&folder).end(),
            |code, _| assert_eq!(code, 1),
        );
        let picker = state(&window);
        load(&picker);
        assert!(
            click(window.upcast_ref(), "Preview"),
            "picker has no shared preview pane"
        );
        select(&picker, &folder.join("one.txt"), true);
        select(&picker, &folder.join("two.txt"), true);
        pump_until(|| preview_text(&window).contains("TWO CURRENT MARKER"));
        assert!(!preview_text(&window).contains("ONE OBSOLETE"));
        let entry = search(&window);
        query(&entry, "one");
        query(&entry, "two");
        pump_until(|| visible(&picker) == [folder.join("two.txt")]);
        select(&picker, &folder.join("two.txt"), true);
        pump_until(|| preview_text(&window).contains("TWO CURRENT MARKER"));
        query(&entry, "one");
        let weak = Rc::downgrade(&picker);
        window.destroy();
        drop(picker);
        drop(window);
        pump_until(|| weak.upgrade().is_none());
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_search_preview_preserves_entry_keys() {
        let app = app();
        let folder = fixture();
        let dict = options(&folder);
        dict.insert("current_name", "draft.txt");
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFile",
            "test.keys",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        let entry = search(&window);
        entry.grab_focus();
        query(&entry, "two");
        entry.emit_by_name::<()>("activate", &[]);
        assert!(
            result.borrow().is_none(),
            "search Enter accepted the request"
        );
        let keys = (0..window.observe_controllers().n_items())
            .find_map(|i| {
                window
                    .observe_controllers()
                    .item(i)
                    .and_downcast::<gtk::EventControllerKey>()
            })
            .unwrap();
        assert!(keys.emit_by_name::<bool>(
            "key-pressed",
            &[
                &gtk::gdk::Key::Escape,
                &0u32,
                &gtk::gdk::ModifierType::empty()
            ]
        ));
        assert!(result.borrow().is_none());
        assert!(entry.text().is_empty());
        picker.name.grab_focus();
        assert!(!keys.emit_by_name::<bool>(
            "key-pressed",
            &[
                &gtk::gdk::Key::BackSpace,
                &0u32,
                &gtk::gdk::ModifierType::empty()
            ]
        ));
        assert_eq!(picker.name.text(), "draft.txt");
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(
            paths(&result.borrow().as_ref().unwrap().1),
            [folder.join("draft.txt")]
        );
        std::fs::remove_dir_all(folder).unwrap();
    }
}
