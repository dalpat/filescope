mod save_tests {
    use super::*;

    fn texts(root: &gtk::Widget) -> Vec<String> {
        let mut labels = Vec::new();
        if let Some(label) = root.downcast_ref::<gtk::Label>() {
            labels.push(label.label().to_string());
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            labels.extend(texts(&widget));
            child = widget.next_sibling();
        }
        labels
    }

    fn paths(results: &glib::Variant) -> Vec<PathBuf> {
        glib::VariantDict::new(Some(results))
            .lookup::<Vec<String>>("uris")
            .unwrap()
            .unwrap()
            .iter()
            .map(|uri| gio::File::for_uri(uri).path().unwrap())
            .collect()
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_save_name_policy_preserves_explicit_extensions() {
        let app = app();
        let folder = fixture();
        let dict = options(&folder);
        dict.insert("current_name", "report");
        dict.insert(
            "filters",
            vec![("Text".to_owned(), vec![(0u32, "*.txt".to_owned())])],
        );
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFile",
            "test.save",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        assert_eq!(
            picker.name.text(),
            "report",
            "a suggestion must not rewrite the caller's name"
        );
        assert!(
            texts(window.upcast_ref())
                .iter()
                .any(|text| text.contains("report.txt")),
            "missing extension has no suggestion"
        );
        assert!(click(window.upcast_ref(), "Use suggested name"));
        assert_eq!(picker.name.text(), "report.txt");
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(
            paths(&result.borrow().as_ref().unwrap().1),
            [folder.join("report.txt")]
        );

        for (name, pattern) in [
            ("already.txt", "*.txt"),
            ("archive.tar.gz", "*.tar.gz"),
            ("kept.other", "*.txt"),
            (".env", "*.txt"),
            ("name.txt.txt", "*.txt"),
        ] {
            let dict = options(&folder);
            dict.insert("current_name", name);
            dict.insert(
                "filters",
                vec![("Type".to_owned(), vec![(0u32, pattern.to_owned())])],
            );
            let result = Rc::new(RefCell::new(None));
            let output = result.clone();
            let window = show(
                &app,
                "SaveFile",
                "test.save",
                "",
                "Save",
                &dict.end(),
                move |code, values| {
                    output.replace(Some((code, values)));
                },
            );
            let picker = state(&window);
            picker.accept();
            pump_until(|| result.borrow().is_some());
            assert_eq!(
                paths(&result.borrow().as_ref().unwrap().1),
                [folder.join(name)]
            );
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_save_feedback_preserves_exact_destination() {
        let app = app();
        let folder = fixture();
        let dict = options(&folder);
        dict.insert("current_name", "../invalid");
        dict.insert(
            "filters",
            vec![("Images".to_owned(), vec![(0u32, "*.png".to_owned())])],
        );
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFile",
            "test.save",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        assert!(
            texts(window.upcast_ref())
                .iter()
                .any(|text| text.contains("without /")),
            "invalid name feedback must precede acceptance"
        );
        picker.name.set_text("日本語 with space.txt");
        assert!(
            texts(window.upcast_ref())
                .iter()
                .any(|text| text.contains("does not match"))
        );
        picker.accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(
            paths(&result.borrow().as_ref().unwrap().1),
            [folder.join("日本語 with space.txt")]
        );

        let dict = options(&folder);
        dict.insert("current_name", "typed.txt");
        dict.insert(
            "filters",
            vec![(
                "PNG images".to_owned(),
                vec![(1u32, "image/png".to_owned())],
            )],
        );
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFile",
            "test.save",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        assert!(
            texts(window.upcast_ref())
                .iter()
                .any(|text| text.contains("does not match")),
            "MIME-only filter mismatch had no feedback"
        );
        state(&window).accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(
            paths(&result.borrow().as_ref().unwrap().1),
            [folder.join("typed.txt")]
        );

        for pattern in ["document-*.txt", "Makefile"] {
            let dict = options(&folder);
            dict.insert("current_name", "README");
            dict.insert(
                "filters",
                vec![(
                    "Requested names".to_owned(),
                    vec![(0u32, pattern.to_owned())],
                )],
            );
            let result = Rc::new(RefCell::new(None));
            let output = result.clone();
            let window = show(
                &app,
                "SaveFile",
                "test.save",
                "",
                "Save",
                &dict.end(),
                move |code, values| {
                    output.replace(Some((code, values)));
                },
            );
            assert!(
                texts(window.upcast_ref())
                    .iter()
                    .any(|text| text.contains("does not match")),
                "{pattern} did not explain an extensionless mismatch"
            );
            state(&window).accept();
            pump_until(|| result.borrow().is_some());
            assert_eq!(
                paths(&result.borrow().as_ref().unwrap().1),
                [folder.join("README")]
            );
        }

        let raw = folder.join(OsString::from_vec(b"raw-\xff.txt".to_vec()));
        let dict = options(&folder);
        let mut bytes = raw.as_os_str().as_bytes().to_vec();
        bytes.push(0);
        dict.insert("current_file", bytes);
        dict.insert(
            "filters",
            vec![("Text".to_owned(), vec![(0u32, "*.txt".to_owned())])],
        );
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFile",
            "test.save",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        state(&window).accept();
        pump_until(|| result.borrow().is_some());
        assert_eq!(paths(&result.borrow().as_ref().unwrap().1), [raw]);
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_save_rechecks_overwrite_target() {
        let app = app();
        let folder = fixture();
        let dict = options(&folder);
        dict.insert("current_name", "one.txt");
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFile",
            "test.save",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        picker.accept();
        wait_overwrite(&picker);
        let parent = window.downcast_ref::<adw::ApplicationWindow>().unwrap();
        picker.name.set_text("two.txt");
        assert!(click(
            parent.visible_dialog().unwrap().upcast_ref(),
            "Replace"
        ));
        pump_until(|| !picker.overwrite.get());
        assert!(
            result.borrow().is_none(),
            "a decision for one.txt accepted another destination"
        );
        assert!(picker.error.label().contains("changed"));

        picker.accept();
        wait_overwrite(&picker);
        let dialog = parent.visible_dialog().unwrap();
        assert!(click(dialog.upcast_ref(), "Cancel"));
        pump_until(|| !picker.overwrite.get());
        assert!(result.borrow().is_none());
        picker.accept();
        wait_overwrite(&picker);
        std::fs::write(folder.join("two.txt"), "changed since confirmation").unwrap();
        assert!(click(
            parent.visible_dialog().unwrap().upcast_ref(),
            "Replace"
        ));
        pump_until(|| !picker.overwrite.get() && !picker.acceptance.busy.get());
        assert!(
            result.borrow().is_none(),
            "changed target reused an obsolete overwrite decision"
        );
        picker.accept();
        wait_overwrite(&picker);
        assert!(click(
            parent.visible_dialog().unwrap().upcast_ref(),
            "Replace"
        ));
        pump_until(|| result.borrow().is_some());
        assert_eq!(
            paths(&result.borrow().as_ref().unwrap().1),
            [folder.join("two.txt")]
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("one.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(folder.join("two.txt")).unwrap(),
            "changed since confirmation"
        );
        std::fs::remove_dir_all(folder).unwrap();
    }


    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_save_rechecks_new_targets_after_access_wait() {
        let app = app();
        let folder = fixture();
        for (method, mixed) in [
            ("SaveFile", false),
            ("SaveFiles", false),
            ("SaveFiles", true),
        ] {
            let dict = options(&folder);
            if method == "SaveFile" {
                dict.insert("current_name", "appeared.txt");
            } else {
                let first = if mixed {
                    b"one.txt\0".to_vec()
                } else {
                    b"missing.txt\0".to_vec()
                };
                dict.insert("files", vec![first, b"appeared.txt\0".to_vec()]);
            }
            let result = Rc::new(RefCell::new(None));
            let output = result.clone();
            let window = show(
                &app,
                method,
                "test.save.race",
                "",
                "Save",
                &dict.end(),
                move |code, values| {
                    output.replace(Some((code, values)));
                },
            );
            let picker = state(&window);
            let (entered, reached) = std::sync::mpsc::channel();
            let (resume, release) = std::sync::mpsc::channel();
            picker
                .acceptance
                .grant_gate
                .replace(Some((entered, release)));
            picker.accept();
            if mixed {
                wait_overwrite(&picker);
                let parent = window.downcast_ref::<adw::ApplicationWindow>().unwrap();
                assert!(click(
                    parent.visible_dialog().unwrap().upcast_ref(),
                    "Replace"
                ));
            }
            pump_until(|| reached.try_recv().is_ok());
            std::fs::write(folder.join("appeared.txt"), "created by another client").unwrap();
            resume.send(()).unwrap();
            pump_until(|| !picker.acceptance.busy.get());
            assert!(
                result.borrow().is_none(),
                "{method} returned a destination created during its access check"
            );
            assert!(picker.error.label().contains("changed"));
            assert_eq!(
                std::fs::read_to_string(folder.join("appeared.txt")).unwrap(),
                "created by another client"
            );
            assert_eq!(
                std::fs::read_to_string(folder.join("one.txt")).unwrap(),
                "one"
            );
            picker.accept();
            wait_overwrite(&picker);
            let parent = window.downcast_ref::<adw::ApplicationWindow>().unwrap();
            assert!(click(
                parent.visible_dialog().unwrap().upcast_ref(),
                "Cancel"
            ));
            pump_until(|| !picker.overwrite.get());
            window.close();
            std::fs::remove_file(folder.join("appeared.txt")).unwrap();
        }
        std::fs::remove_dir_all(folder).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn picker_savefiles_and_choices_remain_intact() {
        let app = app();
        let folder = fixture();
        let dict = options(&folder);
        let names = vec![b"a b.txt\0".to_vec(), b"name-\xff.txt\0".to_vec()];
        dict.insert("files", names);
        let filter: FilterSpec = ("Text".into(), vec![(0, "*.txt".into())]);
        dict.insert("filters", vec![filter.clone()]);
        let choices: Vec<ChoiceSpec> = vec![
            (
                "readonly".into(),
                "Read only".into(),
                vec![],
                "false".into(),
            ),
            (
                "encoding".into(),
                "Encoding".into(),
                vec![
                    ("utf8".into(), "UTF-8".into()),
                    ("latin".into(), "Latin".into()),
                ],
                "utf8".into(),
            ),
        ];
        dict.insert("choices", choices);
        let result = Rc::new(RefCell::new(None));
        let output = result.clone();
        let window = show(
            &app,
            "SaveFiles",
            "test.save",
            "",
            "Save",
            &dict.end(),
            move |code, values| {
                output.replace(Some((code, values)));
            },
        );
        let picker = state(&window);
        load(&picker);
        for choice in &picker.choices {
            match choice {
                ChoiceControl::Boolean(_, button) => button.set_active(true),
                ChoiceControl::List(_, _, dropdown) => dropdown.set_selected(1),
            }
        }
        select(&picker, &folder.join("folder"), true);
        picker.accept();
        pump_until(|| result.borrow().is_some());
        let result = result.borrow();
        let (code, values) = result.as_ref().unwrap();
        assert_eq!(*code, 0);
        assert_eq!(
            paths(values),
            [
                folder.join("folder/a b.txt"),
                folder
                    .join("folder")
                    .join(OsString::from_vec(b"name-\xff.txt".to_vec()))
            ]
        );
        let values = glib::VariantDict::new(Some(values));
        assert_eq!(
            values.lookup::<FilterSpec>("current_filter").unwrap(),
            Some(filter)
        );
        assert_eq!(
            values.lookup::<Vec<(String, String)>>("choices").unwrap(),
            Some(vec![
                ("readonly".into(), "true".into()),
                ("encoding".into(), "latin".into())
            ])
        );
        std::fs::remove_dir_all(folder).unwrap();
    }
}
