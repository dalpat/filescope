//! Update UI; network, verification and installation run on the worker pool.
use super::*;
use crate::update;

thread_local! {
    static INSTALLED: Rc<RefCell<Option<update::Installed>>> = Rc::new(RefCell::new(None));
}

pub(super) struct State {
    pub automatic: Cell<bool>,
    busy: Cell<bool>,
    installed: Rc<RefCell<Option<update::Installed>>>,
}

impl State {
    pub fn new(automatic: bool) -> Self {
        Self {
            automatic: Cell::new(automatic),
            busy: Cell::new(false),
            installed: INSTALLED.with(Clone::clone),
        }
    }
}

pub(super) fn setup(app: &Rc<App>) {
    for (name, callback) in [
        ("check-updates", check as fn(&Rc<App>)),
        ("restart-update", restart as fn(&Rc<App>)),
    ] {
        let action = gio::SimpleAction::new(name, None);
        action.set_enabled(name != "restart-update" || app.updates.installed.borrow().is_some());
        let weak = Rc::downgrade(app);
        action.connect_activate(move |_, _| {
            if let Some(app) = weak.upgrade() {
                callback(&app);
            }
        });
        app.window.add_action(&action);
    }
    let auto = gio::SimpleAction::new_stateful(
        "automatic-updates",
        None,
        &app.updates.automatic.get().to_variant(),
    );
    let weak = Rc::downgrade(app);
    auto.connect_activate(move |action, _| {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let enabled = !app.updates.automatic.get();
        app.updates.automatic.set(enabled);
        action.set_state(&enabled.to_variant());
        save_settings(&app);
        if enabled {
            start_check(&app, false);
        }
    });
    app.window.add_action(&auto);
    // Once per application process, after initial navigation and without service-only wakeups.
    static CHECKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !cfg!(test)
        && app.updates.automatic.get()
        && std::env::var_os("FILESCOPE_SHOT").is_none()
        && !CHECKED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        let weak = Rc::downgrade(app);
        glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
            if let Some(app) = weak
                .upgrade()
                .filter(|a| a.window.is_visible() && a.updates.automatic.get())
            {
                start_check(&app, false);
            }
        });
    }
}

fn enabled(app: &App, name: &str, enabled: bool) {
    if let Some(action) = app
        .window
        .lookup_action(name)
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(enabled);
    }
}

fn check(app: &Rc<App>) {
    start_check(app, true);
}

fn start_check(app: &Rc<App>, manual: bool) {
    if app.updates.installed.borrow().is_some() {
        restart(app);
        return;
    }
    if app.updates.busy.replace(true) {
        return;
    }
    enabled(app, "check-updates", false);
    if manual {
        toast(app, "Checking for updates…");
    }
    let weak = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(|| {
            update::check().map(|r| (r, update::installed_binary().err().map(|e| e.to_string())))
        })
        .await;
        let Some(app) = weak.upgrade() else {
            return;
        };
        app.updates.busy.set(false);
        enabled(&app, "check-updates", true);
        if !app.window.is_visible() || (!manual && !app.updates.automatic.get()) {
            return;
        }
        if app.updates.installed.borrow().is_some() {
            if manual {
                restart(&app);
            }
            return;
        }
        match result {
            Ok(Ok((Some(release), reason))) if manual => offer(&app, release, reason),
            Ok(Ok((Some(release), reason))) => {
                let notice = adw::Toast::builder()
                    .title(format!("Filescope {} is available", release.version))
                    .button_label("Review")
                    .timeout(0)
                    .build();
                let weak = Rc::downgrade(&app);
                notice.connect_button_clicked(move |_| {
                    if let Some(app) = weak.upgrade() {
                        offer(&app, release.clone(), reason.clone());
                    }
                });
                app.toasts.add_toast(notice);
            }
            Ok(Ok((None, _))) if manual => {
                toast(&app, "You’re using the latest available stable release.")
            }
            Ok(Err(error)) if manual => {
                message(&app, "Couldn't check for updates", &error.to_string())
            }
            Err(_) if manual => message(
                &app,
                "Couldn't check for updates",
                "The update worker stopped. Try again.",
            ),
            _ => {} // Offline automatic checks remain quiet; manual checks explain failures.
        }
    });
}

fn message(app: &App, title: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(title), Some(body));
    dialog.add_response("close", "Close");
    dialog.set_close_response("close");
    dialog.present(Some(&app.window));
}

fn offer(app: &Rc<App>, release: update::Release, reason: Option<String>) {
    if app.updates.busy.get() {
        return;
    }
    if app.updates.installed.borrow().is_some() {
        restart(app);
        return;
    }
    let dialog = adw::AlertDialog::new(
        Some(&format!("Filescope {} is available", release.version)),
        Some(reason.as_deref().unwrap_or(
            "Download and install this release? You can keep browsing and restart when ready.",
        )),
    );
    let notes = gtk::Label::builder()
        .label(&release.notes)
        .wrap(true)
        .selectable(true)
        .xalign(0.0)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .child(&notes)
        .min_content_height(160)
        .max_content_height(300)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    dialog.set_extra_child(Some(&scroll));
    dialog.add_responses(&[("later", "Later"), ("install", "Install Update")]);
    dialog.set_close_response("later");
    dialog.set_default_response(Some("later"));
    dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("install", reason.is_none());
    let weak = Rc::downgrade(app);
    dialog.choose(&app.window, gio::Cancellable::NONE, move |response| {
        if response == "install"
            && let Some(app) = weak.upgrade()
        {
            install(&app, release);
        }
    });
}

fn install(app: &Rc<App>, release: update::Release) {
    if app.updates.busy.replace(true) {
        return;
    }
    enabled(app, "check-updates", false);
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let spinner = gtk::Spinner::builder().spinning(true).build();
    row.append(&spinner);
    row.append(&gtk::Label::new(Some("Downloading and verifying update…")));
    app.progress_box.append(&row);
    app.progress_revealer.set_reveal_child(true);
    let weak = Rc::downgrade(app);
    let application = app
        .window
        .application()
        .and_downcast::<adw::Application>()
        .map(|a| a.downgrade());
    let work = BackgroundWork::new(app);
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || update::install(&release)).await;
        drop(work);
        if let Ok(Ok(installed)) = &result {
            INSTALLED.with(|value| *value.borrow_mut() = Some(installed.clone()));
        }
        let Some(app) = weak.upgrade() else {
            return;
        };
        app.progress_box.remove(&row);
        if app.progress_box.first_child().is_none() {
            app.progress_revealer.set_reveal_child(false);
        }
        app.updates.busy.set(false);
        enabled(&app, "check-updates", true);
        let app = application
            .and_then(|a| a.upgrade())
            .and_then(|a| app_state(&a))
            .unwrap_or(app);
        match result {
            Ok(Ok(installed)) => {
                *app.updates.installed.borrow_mut() = Some(installed);
                enabled(&app, "restart-update", true);
                if app.window.is_visible() {
                    let reason = restart_reason(&app)
                        .unwrap_or("Restart when you’re ready to use the new version.");
                    message(&app, "Update installed", reason);
                    let notice = adw::Toast::builder()
                        .title("Update installed")
                        .button_label("Restart…")
                        .timeout(0)
                        .build();
                    let weak = Rc::downgrade(&app);
                    notice.connect_button_clicked(move |_| {
                        if let Some(app) = weak.upgrade() {
                            restart(&app);
                        }
                    });
                    app.toasts.add_toast(notice);
                }
            }
            Ok(Err(error)) if app.window.is_visible() => {
                message(&app, "Couldn't install the update", &error.to_string())
            }
            Err(_) if app.window.is_visible() => message(
                &app,
                "Couldn't install the update",
                "The worker stopped. Your current session is still running.",
            ),
            _ => {}
        }
    });
}

fn restart_reason(app: &App) -> Option<&'static str> {
    update::restart_blocked(
        BackgroundWork::active()
            || app.updates.busy.get()
            || app.progress_box.first_child().is_some()
            || app.history_busy.get()
            || app
                .pending_renames
                .borrow()
                .iter()
                .any(|scope| scope.upgrade().is_some()),
        app.tabs
            .borrow()
            .iter()
            .any(|tab| tab.terminal.has_live_session()),
    )
}

fn restart(app: &Rc<App>) {
    if app.updates.installed.borrow().is_none() {
        return;
    }
    if let Some(reason) = restart_reason(app) {
        message(app, "Restart waiting", reason);
        return;
    }
    let dialog = adw::AlertDialog::new(
        Some("Restart Filescope?"),
        Some("Open folders will reopen. Selection and Undo history will not survive the restart."),
    );
    dialog.add_responses(&[("later", "Later"), ("restart", "Restart")]);
    dialog.set_close_response("later");
    dialog.set_default_response(Some("later"));
    let weak = Rc::downgrade(app);
    dialog.choose(&app.window, gio::Cancellable::NONE, move |response| {
        let Some(app) = weak.upgrade().filter(|_| response == "restart") else {
            return;
        };
        // Work can start while the confirmation is open; always recheck here.
        if let Some(reason) = restart_reason(&app) {
            message(&app, "Restart waiting", reason);
            return;
        }
        let Some(installed) = app.updates.installed.borrow().clone() else {
            return;
        };
        let folders: Vec<_> = app
            .tabs
            .borrow()
            .iter()
            .filter_map(|tab| tab.current_dir())
            .map(|file| file.uri().to_string())
            .collect();
        save_settings(&app);
        if let Err(error) = update::restart(&installed, &folders) {
            message(&app, "Couldn't restart", &error.to_string());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dismiss(app: &App) {
        if let Some(dialog) = app
            .window
            .visible_dialog()
            .and_downcast::<adw::AlertDialog>()
        {
            dialog.force_close();
        }
    }

    #[test]
    #[ignore = "requires an isolated graphical session, SHELL=/bin/sh and temporary XDG_CONFIG_HOME"]
    fn update_actions_preserve_preferences_and_block_busy_restart() {
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        let application = adw::Application::builder()
            .application_id("dev.filescope.UpdateTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!(
            "filescope-update-ui-{}",
            glib::uuid_string_random()
        ));
        std::fs::create_dir(&root).unwrap();
        open_files(&application, &[gio::File::for_path(&root)]);
        let app = app_state(&application).unwrap();
        let tab = navigation::active_tab(&app);
        assert!(
            app.window
                .lookup_action("check-updates")
                .unwrap()
                .is_enabled()
        );
        assert!(
            !app.window
                .lookup_action("restart-update")
                .unwrap()
                .is_enabled()
        );
        app.window
            .lookup_action("automatic-updates")
            .unwrap()
            .activate(None);
        assert!(!app.updates.automatic.get());
        assert!(!settings::load().automatic_updates);
        let original_view = app.view_mode.get();
        save_settings(&app);
        assert_eq!(settings::load().view_mode, original_view);
        *app.updates.installed.borrow_mut() = Some(update::Installed {
            path: root.join("must-not-execute"),
            digest: "a".repeat(64),
        });
        enabled(&app, "restart-update", true);
        let context = glib::MainContext::default();
        context.block_on(async {
            for state in 0..5 {
                let batch = (state == 4).then(|| navigation::BatchRename::new(&app, &[]));
                let row = gtk::Label::new(Some("Active transfer"));
                if state == 0 {
                    app.progress_box.append(&row);
                }
                if state == 1 {
                    app.history_busy.set(true);
                }
                if state == 2 {
                    app.updates.busy.set(true);
                }
                if state == 3 {
                    tab.terminal.toggle(Some(&root));
                    tab.terminal.toggle(Some(&root));
                }
                assert!(restart_reason(&app).is_some());
                app.window
                    .lookup_action("restart-update")
                    .unwrap()
                    .activate(None);
                glib::timeout_future(std::time::Duration::from_millis(100)).await;
                let dialog = app
                    .window
                    .visible_dialog()
                    .and_downcast::<adw::AlertDialog>()
                    .unwrap();
                assert_eq!(dialog.heading().as_deref(), Some("Restart waiting"));
                dismiss(&app);
                if state == 0 {
                    app.progress_box.remove(&row);
                }
                if let Some(batch) = batch {
                    batch.finish(&app, None);
                }
                app.history_busy.set(false);
                app.updates.busy.set(false);
                if state == 3 {
                    tab.terminal.shutdown();
                }
                glib::timeout_future(std::time::Duration::from_millis(550)).await;
            }
            assert!(restart_reason(&app).is_none());
            restart(&app);
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            let dialog = app
                .window
                .visible_dialog()
                .and_downcast::<adw::AlertDialog>()
                .unwrap();
            assert_eq!(dialog.heading().as_deref(), Some("Restart Filescope?"));
            app.history_busy.set(true);
            dialog.emit_by_name::<()>("response", &[&"restart"]);
            glib::timeout_future(std::time::Duration::from_millis(150)).await;
            let blocked = app
                .window
                .visible_dialog()
                .and_downcast::<adw::AlertDialog>()
                .unwrap();
            assert_eq!(blocked.heading().as_deref(), Some("Restart waiting"));
            dismiss(&app);
            app.history_busy.set(false);
        });
        app.window.close();
        open_files(&application, &[gio::File::for_path(&root)]);
        let reopened = app_state(&application).unwrap();
        assert!(reopened.updates.installed.borrow().is_some());
        assert!(
            reopened
                .window
                .lookup_action("restart-update")
                .unwrap()
                .is_enabled()
        );
        assert!(!reopened.updates.automatic.get());
        reopened.updates.installed.borrow_mut().take();
        reopened.window.close();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod review_probes {
    use super::*;
    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn reopening_window_keeps_live_copy_restart_guard() {
        use std::io::Write;
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        let application = adw::Application::builder()
            .application_id("dev.filescope.UpdateReview")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root = std::env::temp_dir().join(format!(
            "filescope-review-copy-{}",
            glib::uuid_string_random()
        ));
        std::fs::create_dir_all(root.join("dest")).unwrap();
        let fifo = root.join("slow-source");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (writer_ready, ready) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let writer_path = fifo.clone();
        let writer = std::thread::spawn(move || {
            let mut file = std::fs::File::create(writer_path).unwrap();
            file.write_all(b"first chunk").unwrap();
            writer_ready.send(()).unwrap();
            release_rx.recv().unwrap();
            file.write_all(b"final chunk").unwrap();
        });
        open_files(&application, &[gio::File::for_path(&root)]);
        let old = app_state(&application).unwrap();
        *old.updates.installed.borrow_mut() = Some(update::Installed {
            path: root.join("not-executed"),
            digest: "a".repeat(64),
        });
        commands::start_job(
            &old,
            crate::ops::Kind::Copy,
            vec![fifo],
            root.join("dest"),
            true,
        );
        ready
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        glib::MainContext::default().block_on(async {
            glib::timeout_future(std::time::Duration::from_millis(150)).await;
            assert!(restart_reason(&old).is_some());
            old.window.close();
            open_files(&application, &[gio::File::for_path(&root)]);
            let new = app_state(&application).unwrap();
            assert!(!Rc::ptr_eq(&old, &new));
            assert!(old.progress_box.first_child().is_some());
            assert!(new.updates.installed.borrow().is_some());
            let blocked = restart_reason(&new).is_some();
            println!(
                "old window guard: {:?}; new window guard: {:?}; copied bytes so far: {}",
                restart_reason(&old),
                restart_reason(&new),
                std::fs::metadata(root.join("dest/slow-source"))
                    .unwrap()
                    .len()
            );
            restart(&new);
            glib::timeout_future(std::time::Duration::from_millis(150)).await;
            let heading = new
                .window
                .visible_dialog()
                .and_downcast::<adw::AlertDialog>()
                .unwrap()
                .heading();
            println!("reopened window shows: {heading:?}");

            release.send(()).unwrap();
            writer.join().unwrap();
            for _ in 0..50 {
                if old.progress_box.first_child().is_none() {
                    break;
                }
                glib::timeout_future(std::time::Duration::from_millis(50)).await;
            }
            assert_eq!(
                std::fs::read(root.join("dest/slow-source")).unwrap(),
                b"first chunkfinal chunk"
            );
            if let Some(dialog) = new.window.visible_dialog() {
                dialog.force_close();
            }
            assert!(
                restart_reason(&new).is_none(),
                "finished copy must release its guard"
            );
            // Archive callbacks and batch/history scopes must survive the old window too.
            commands::run_tool(
                &new,
                "Test archive job",
                vec![
                    "/usr/bin/timeout".into(),
                    "5".into(),
                    "/bin/sh".into(),
                    "-c".into(),
                    "read line < \"$1\"".into(),
                    "archive-test".into(),
                    root.join("slow-source").into_os_string(),
                ],
                &root,
                |_, _| {},
            );
            let archive_owner = new.clone();
            new.window.close();
            open_files(&application, &[gio::File::for_path(&root)]);
            let latest = app_state(&application).unwrap();
            let archive_blocked = restart_reason(&latest).is_some();
            let batch = navigation::BatchRename::new(&archive_owner, &[]);
            let overlapping = BackgroundWork::new(&archive_owner);
            std::fs::write(root.join("slow-source"), b"done\n").unwrap();
            for _ in 0..50 {
                if archive_owner.progress_box.first_child().is_none() {
                    break;
                }
                glib::timeout_future(std::time::Duration::from_millis(50)).await;
            }
            assert!(archive_owner.progress_box.first_child().is_none());
            assert!(restart_reason(&latest).is_some());
            batch.finish(&archive_owner, None);
            assert!(
                restart_reason(&latest).is_some(),
                "one completed job must not release another guard"
            );
            drop(overlapping);
            assert!(restart_reason(&latest).is_none());
            commands::run_tool(
                &latest,
                "Missing tool",
                vec![root.join("no-such-tool").into_os_string()],
                &root,
                |_, _| {},
            );
            assert!(
                restart_reason(&latest).is_none(),
                "failed spawn must release its guard"
            );
            latest.updates.installed.borrow_mut().take();
            latest.window.close();
            assert!(
                archive_blocked,
                "reopened window forgot the running archive job"
            );
            assert!(blocked, "reopened window forgot the running copy");
            assert_eq!(heading.as_deref(), Some("Restart waiting"));
        });
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod conflict_tests {
    use super::*;

    #[test]
    #[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
    fn closed_owner_conflicts_cancel_without_losing_sources() {
        use std::io::Write;
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        let application = adw::Application::builder()
            .application_id("dev.filescope.ClosedConflict")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let mut failures = Vec::new();
        for mode in ["closed-before", "closed-during", "hidden"] {
            let root = std::env::temp_dir().join(format!(
                "filescope-closed-conflict-{}",
                glib::uuid_string_random()
            ));
            std::fs::create_dir_all(root.join("dest")).unwrap();
            let fifo = root.join("slow-source");
            let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            std::fs::write(root.join("conflict"), b"source").unwrap();
            std::fs::write(root.join("dest/conflict"), b"existing target").unwrap();
            let (ready_tx, ready) = std::sync::mpsc::channel();
            let (release, release_rx) = std::sync::mpsc::channel();
            let path = fifo.clone();
            let writer = std::thread::spawn(move || {
                let mut file = std::fs::File::create(path).unwrap();
                file.write_all(b"first item").unwrap();
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
            open_files(&application, &[gio::File::for_path(&root)]);
            let owner = app_state(&application).unwrap();
            commands::start_job(
                &owner,
                crate::ops::Kind::Copy,
                vec![fifo.clone(), root.join("conflict")],
                root.join("dest"),
                true,
            );
            ready
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            glib::MainContext::default().block_on(async {
                if mode == "closed-before" {
                    owner.window.close();
                }
                if mode == "hidden" {
                    owner.window.set_visible(false);
                }
                release.send(()).unwrap();
                writer.join().unwrap();
                if mode != "closed-before" {
                    for _ in 0..50 {
                        if owner.window.visible_dialog().is_some() {
                            break;
                        }
                        glib::timeout_future(std::time::Duration::from_millis(20)).await;
                    }
                    assert!(
                        owner.window.visible_dialog().is_some(),
                        "{mode}: expected accessible conflict prompt"
                    );
                    if mode == "closed-during" {
                        owner.window.close();
                        // Adwaita dismisses the active dialog on the first close request.
                        for _ in 0..50 {
                            if owner.window.visible_dialog().is_none() {
                                break;
                            }
                            glib::timeout_future(std::time::Duration::from_millis(20)).await;
                        }
                        owner.window.close();
                    }
                }
                let current = if mode == "hidden" {
                    owner.clone()
                } else {
                    assert!(
                        !app_state(&application)
                            .as_ref()
                            .is_some_and(|state| Rc::ptr_eq(state, &owner)),
                        "{mode}: closed owner must leave current application state"
                    );
                    open_files(&application, &[gio::File::for_path(&root)]);
                    app_state(&application).unwrap()
                };
                if mode == "hidden" {
                    assert!(owner.window.application().is_some());
                    assert!(
                        restart_reason(&current).is_some(),
                        "hiding must not cancel an active prompt"
                    );
                    owner.window.visible_dialog().unwrap().force_close();
                }
                for _ in 0..50 {
                    if owner.progress_box.first_child().is_none() {
                        break;
                    }
                    glib::timeout_future(std::time::Duration::from_millis(20)).await;
                }
                if owner.progress_box.first_child().is_some() || restart_reason(&current).is_some()
                {
                    failures.push(mode);
                }
                // Clean up even against the broken version, where the prompt is on a destroyed window.
                if let Some(dialog) = owner.window.visible_dialog() {
                    dialog.force_close();
                }
                for _ in 0..50 {
                    if owner.progress_box.first_child().is_none() {
                        break;
                    }
                    glib::timeout_future(std::time::Duration::from_millis(20)).await;
                }
                assert!(
                    owner.progress_box.first_child().is_none(),
                    "{mode}: cleanup did not finish"
                );
                assert!(
                    !BackgroundWork::active(),
                    "{mode}: background hold was leaked"
                );
                assert!(fifo.exists());
                assert_eq!(std::fs::read(root.join("conflict")).unwrap(), b"source");
                assert_eq!(
                    std::fs::read(root.join("dest/conflict")).unwrap(),
                    b"existing target"
                );
                assert_eq!(
                    std::fs::read(root.join("dest/slow-source")).unwrap(),
                    b"first item"
                );
                current.window.close();
            });
            std::fs::remove_dir_all(root).unwrap();
        }
        assert!(
            failures.is_empty(),
            "closed owner left inaccessible conflicts and guards: {failures:?}"
        );
    }
}
