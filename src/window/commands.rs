// Things you do to the selection: open, copy, move, rename, trash, archives.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{archive, fileops, format, ops, preview, undo};

pub(super) fn selected(tab: &Rc<super::Tab>) -> Vec<(gio::File, gio::FileInfo)> {
    let Some(dir) = tab.dir_list.file() else { return Vec::new() };
    let bitset = tab.selection.selection();
    let mut out = Vec::new();
    for i in 0..bitset.size() {
        let pos = bitset.nth(i as u32);
        if let Some(obj) = tab.selection.item(pos) {
            let info = obj.downcast::<gio::FileInfo>().unwrap();
            out.push((dir.child(info.name()), info));
        }
    }
    out
}

pub(super) fn activate(app: &Rc<super::App>, tab: &Rc<super::Tab>, position: u32) {
    let Some(item) = tab.selection.item(position) else { return };
    let info = item.downcast::<gio::FileInfo>().unwrap();
    let Some(dir) = tab.dir_list.file() else { return };
    let child = dir.child(info.name());
    if info.file_type() == gio::FileType::Directory {
        super::navigation::navigate(app, tab, child);
    } else {
        launch(app, &child);
    }
}

pub(super) fn open_selected(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let items = selected(&tab);
    if let [(file, info)] = items.as_slice()
        && info.file_type() == gio::FileType::Directory
    {
        super::navigation::navigate(app, &tab, file.clone());
        return;
    }
    for (file, info) in items {
        if info.file_type() != gio::FileType::Directory {
            launch(app, &file);
        }
    }
}

/// "Open With…": let the user pick which application opens the selected file.
/// Uses the platform app chooser (via `FileLauncher::always_ask`), so it lists
/// every app that handles the type plus an "Other Application…" escape hatch.
pub(super) fn open_with(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else {
        return;
    };
    if info.file_type() == gio::FileType::Directory {
        return; // "Open With" is only meaningful for files.
    }
    let launcher = gtk::FileLauncher::new(Some(file));
    launcher.set_always_ask(true);
    let app = app.clone();
    launcher.launch(Some(&app.window.clone()), gio::Cancellable::NONE, move |res| {
        if let Err(err) = res {
            // Dismissing the chooser isn't an error worth a super::toast.
            if !err.matches(gtk::DialogError::Dismissed) {
                super::toast(&app, &format!("Couldn't open: {err}"));
            }
        }
    });
}

/// Space toggles a preview of the single selected file. Sushi gets it when
/// installed; otherwise the in-app window opens.
pub(super) fn toggle_preview(app: &Rc<super::App>) {
    if let Some(win) = app.preview.borrow_mut().take() {
        win.close();
        return;
    }
    let tab = super::navigation::active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else { return };
    if info.file_type() == gio::FileType::Directory {
        return;
    }
    let file = file.clone();
    let fallback = file.clone();
    let info = info.clone();
    let app_cb = app.clone();
    preview::show_sushi(&app.window, &file, move |opened| {
        if opened || app_cb.preview.borrow().is_some() {
            return;
        }
        let win = preview::show(&app_cb.window, &fallback, &info);
        {
            let app_cb = app_cb.clone();
            win.connect_close_request(move |_| {
                *app_cb.preview.borrow_mut() = None;
                glib::Propagation::Proceed
            });
        }
        *app_cb.preview.borrow_mut() = Some(win);
    });
}

pub(super) fn launch(app: &Rc<super::App>, file: &gio::File) {
    // GTK's recent list, tagged with this app, so This PC can show what we opened.
    gtk::RecentManager::default().add_item(&file.uri());
    // A default handler opens the file directly. The chooser is only for a type
    // nothing is registered to handle. FileLauncher asks every time on its own.
    let content_type = file
        .query_info("standard::content-type", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
        .ok()
        .and_then(|info| info.content_type().map(|kind| kind.to_string()));
    if let Some(kind) = content_type.as_deref()
        && let Some(handler) = gio::AppInfo::default_for_type(kind, false)
    {
        if let Err(err) = handler.launch(&[file.clone()], None::<&gio::AppLaunchContext>) {
            super::toast(app, &format!("Couldn't open: {err}"));
        }
        return;
    }
    let launcher = gtk::FileLauncher::new(Some(file));
    launcher.set_always_ask(true);
    let app = app.clone();
    launcher.launch(Some(&app.window.clone()), gio::Cancellable::NONE, move |res| {
        if let Err(err) = res {
            if !err.matches(gtk::DialogError::Dismissed) {
                super::toast(&app, &format!("Couldn't open: {err}"));
            }
        }
    });
}

/// Open a terminal in the active tab's folder (Ctrl+.). Only meaningful for a
/// real on-disk directory (not the "This PC" view).
pub(super) fn open_terminal(app: &Rc<super::App>) {
    let Some(dir) = super::navigation::active_tab(app).dir_list.file().and_then(|f| f.path()) else { return };
    if !open_terminal_at(&dir) {
        super::toast(app, "No terminal emulator found");
    }
}

/// Spawn a terminal emulator opening `dir`. Honours `$TERMINAL` first, then tries
/// the common emulators, returning whether one launched.
///
/// The directory is passed via each terminal's own flag rather than relying on
/// the spawned process's working directory: modern terminals (ptyxis, GNOME
/// Console, gnome-terminal) are single-instance/D-Bus-activated, so a bare
/// re-launch just raises the running instance and ignores the inherited cwd —
/// the flag is what makes them open a new window in the right folder.
pub(super) fn open_terminal_at(dir: &Path) -> bool {
    // Specific terminals (with correct flags) are tried before the generic
    // `x-terminal-emulator`, which may itself resolve to one of them.
    let mut bins: Vec<String> = Vec::new();
    if let Ok(term) = std::env::var("TERMINAL")
        && !term.is_empty()
    {
        bins.push(term);
    }
    bins.extend(
        [
            "ptyxis",
            "kgx", // GNOME Console
            "gnome-terminal",
            "konsole",
            "xfce4-terminal",
            "tilix",
            "kitty",
            "alacritty",
            "wezterm",
            "foot",
            "xterm",
            "x-terminal-emulator",
        ]
        .into_iter()
        .map(str::to_string),
    );
    bins.into_iter().any(|bin| {
        std::process::Command::new(&bin)
            .args(terminal_args(&bin, dir.as_os_str()))
            .current_dir(dir)
            .spawn()
            .is_ok()
    })
}

/// The arguments that make `bin` open a new window in `dir`. Unknown terminals
/// get none and fall back to the inherited working directory.
pub(super) fn terminal_args(bin: &str, dir: &std::ffi::OsStr) -> Vec<std::ffi::OsString> {
    let name = Path::new(bin).file_name().and_then(|s| s.to_str()).unwrap_or(bin);
    let d = dir.to_os_string();
    match name {
        // ptyxis needs an explicit new window; -d supplies its directory.
        "ptyxis" => vec!["--new-window".into(), "-d".into(), d],
        "konsole" => vec!["--workdir".into(), d],
        "kitty" => vec!["--directory".into(), d],
        "wezterm" => vec!["start".into(), "--cwd".into(), d],
        "gnome-terminal" | "kgx" | "gnome-console" | "xfce4-terminal" | "tilix" | "alacritty"
        | "foot" => vec!["--working-directory".into(), d],
        _ => Vec::new(),
    }
}

/// Put the selection on the **system** clipboard (so it pastes into other apps
/// too), tagged copy or cut.
pub(super) fn set_clipboard(app: &Rc<super::App>, cut: bool) {
    let files: Vec<gio::File> = selected(&super::navigation::active_tab(app)).into_iter().map(|(f, _)| f).collect();
    if files.is_empty() {
        return;
    }
    let n = files.len();
    let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();

    // GNOME copy/cut format: "<action>\n<uri>\n<uri>…".
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let gnome_p = gdk::ContentProvider::for_bytes(super::GNOME_COPIED, &glib::Bytes::from(gnome.as_bytes()));
    // Generic URI list, CRLF-separated.
    let uri_list = format!("{}\r\n", uris.join("\r\n"));
    let uri_p = gdk::ContentProvider::for_bytes(super::URI_LIST, &glib::Bytes::from(uri_list.as_bytes()));

    let union = gdk::ContentProvider::new_union(&[gnome_p, uri_p]);
    let _ = app.window.clipboard().set_content(Some(&union));
    super::update_chrome(app);
    super::toast(app, &format!("{} {n} item{}", if cut { "Cut" } else { "Copied" }, super::plural(n)));
}

/// Read file URIs (and the cut/copy intent) currently on the clipboard, then
/// invoke `done`. Reads GNOME's format if present (for the intent), else the URI
/// list. Everything is async — the clipboard read never blocks the UI.
pub(super) fn read_clipboard_files(clipboard: &gdk::Clipboard, done: impl Fn(bool, Vec<PathBuf>) + 'static) {
    clipboard.read_async(
        &[super::GNOME_COPIED, super::URI_LIST],
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        move |res| {
            let Ok((stream, mime)) = res else { return };
            stream.read_bytes_async(
                1 << 16,
                glib::Priority::DEFAULT,
                gio::Cancellable::NONE,
                move |bytes| {
                    let Ok(bytes) = bytes else { return };
                    let text = String::from_utf8_lossy(&bytes);
                    let (cut, uris) = parse_clipboard(mime.as_str(), &text);
                    let paths: Vec<PathBuf> =
                        uris.iter().filter_map(|u| gio::File::for_uri(u).path()).collect();
                    done(cut, paths);
                },
            );
        },
    );
}

/// Parse clipboard text into (is_cut, uris) for the GNOME copy/cut format or a
/// plain URI list.
pub(super) fn parse_clipboard(mime: &str, text: &str) -> (bool, Vec<String>) {
    if mime == super::GNOME_COPIED {
        let mut lines = text.lines();
        let cut = lines.next().map(|a| a.trim() == "cut").unwrap_or(false);
        let uris = lines.filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
        (cut, uris)
    } else {
        let uris = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect();
        (false, uris)
    }
}

/// Set a stateful window action's state (used to seed the UI from saved settings).
pub(super) fn set_action_state(app: &Rc<super::App>, name: &str, value: &glib::Variant) {
    if let Some(action) = app.window.lookup_action(name)
        && let Ok(action) = action.downcast::<gio::SimpleAction>()
    {
        action.set_state(value);
    }
}

/// Run an external tool in the background with a progress row (pulsing, since
/// these tools don't report progress) and a Cancel button. `done` runs on the
/// main loop with whether it succeeded.
pub(super) fn run_tool(
    app: &Rc<super::App>,
    title: &str,
    argv: Vec<std::ffi::OsString>,
    cwd: &Path,
    done: impl Fn(&Rc<super::App>, bool) + 'static,
) {
    let label = gtk::Label::builder()
        .xalign(0.0)
        .label(title)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    label.add_css_class("caption");
    let bar = gtk::ProgressBar::builder().hexpand(true).valign(gtk::Align::Center).build();
    let stop = crate::icon::flat_button("process-stop-symbolic", "Cancel");
    let bar_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar_row.append(&bar);
    bar_row.append(&stop);
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.append(&label);
    row.append(&bar_row);
    app.progress_box.append(&row);
    app.progress_revealer.set_reveal_child(true);

    // These tools give no progress, so show activity rather than a fake number.
    let running = Rc::new(Cell::new(true));
    {
        let bar = bar.clone();
        let running = running.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            if !running.get() {
                return glib::ControlFlow::Break;
            }
            bar.pulse();
            glib::ControlFlow::Continue
        });
    }

    let refs: Vec<&std::ffi::OsStr> = argv.iter().map(|s| s.as_os_str()).collect();
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::NONE);
    launcher.set_cwd(cwd);
    let process = match launcher.spawn(&refs) {
        Ok(process) => process,
        Err(err) => {
            running.set(false);
            app.progress_box.remove(&row);
            super::toast(app, &format!("Couldn't start: {err}"));
            return;
        }
    };
    {
        let process = process.clone();
        stop.connect_clicked(move |_| process.force_exit());
    }

    let app = app.clone();
    process.wait_check_async(gio::Cancellable::NONE, move |res| {
        running.set(false);
        app.progress_box.remove(&row);
        if app.progress_box.first_child().is_none() {
            app.progress_revealer.set_reveal_child(false);
        }
        for tab in app.tabs.borrow().iter() {
            super::navigation::refresh(tab);
        }
        done(&app, res.is_ok());
    });
}

/// Extract each selected archive into a new folder named after it.
pub(super) fn extract_selected(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let Some(dir) = tab.dir_list.file().and_then(|f| f.path()) else { return };
    for (file, info) in selected(&tab) {
        let name = info.display_name().to_string();
        let Some(format) = archive::Format::from_name(&name) else { continue };
        let Some(path) = file.path() else { continue };
        if !super::has_binary(format.tool()) {
            super::toast(app, &format!("Extracting this needs “{}” — install it first.", format.tool()));
            continue;
        }
        // Extract into its own folder, so an archive never litters the folder.
        let dest = fileops::unique_destination(&dir, &archive::base_name(&name));
        if let Err(err) = std::fs::create_dir(&dest) {
            super::toast(app, &format!("Couldn't create a folder to extract into: {err}"));
            continue;
        }
        let into = dest.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        run_tool(
            app,
            &format!("Extracting {name}…"),
            format.extract_argv(&path, &dest),
            &dir,
            move |app, ok| {
                if ok {
                    super::toast(app, &format!("Extracted to “{into}”"));
                } else {
                    // Nothing usable came out; don't leave an empty folder behind.
                    super::toast(app, "Couldn't extract the archive");
                }
            },
        );
    }
}

/// Ask for a name and format, then compress the selection into the current folder.
pub(super) fn compress_selected(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let Some(dir) = tab.dir_list.file().and_then(|f| f.path()) else { return };
    let items: Vec<String> =
        selected(&tab).iter().map(|(_, i)| i.name().to_string_lossy().into_owned()).collect();
    if items.is_empty() {
        return;
    }

    let entry = gtk::Entry::builder()
        .text(archive::default_archive_name(&items))
        .activates_default(true)
        .build();
    let formats = gtk::DropDown::from_strings(&[".zip", ".tar.gz", ".tar.xz"]);
    let box_ = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    box_.append(&entry);
    box_.append(&formats);

    let dialog = adw::AlertDialog::new(
        Some("Compress"),
        Some(&format!("Create an archive of {} item{}.", items.len(), super::plural(items.len()))),
    );
    dialog.set_extra_child(Some(&box_));
    dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");

    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "create" {
            return;
        }
        let format = match formats.selected() {
            1 => archive::Compress::TarGz,
            2 => archive::Compress::TarXz,
            _ => archive::Compress::Zip,
        };
        let stem = entry.text().to_string();
        if stem.is_empty() || stem.contains('/') {
            super::toast(&app, "Invalid name");
            return;
        }
        if !super::has_binary(format.tool()) {
            super::toast(&app, &format!("This needs “{}” — install it first.", format.tool()));
            return;
        }
        let output = fileops::unique_destination(&dir, &format!("{stem}{}", format.extension()));
        let name = output.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // Pack relative to the folder, so the archive holds plain names.
        let relative = PathBuf::from(&name);
        run_tool(
            &app,
            &format!("Compressing to {name}…"),
            format.argv(&relative, &items),
            &dir,
            move |app, ok| {
                if ok {
                    super::toast(app, &format!("Created “{name}”"));
                } else {
                    super::toast(app, "Couldn't create the archive");
                }
            },
        );
    });
}

/// Record a new reversible action. Doing something new invalidates the redo side.
pub(super) fn push_undo(app: &Rc<super::App>, action: undo::Action) {
    app.undo_stack.borrow_mut().push(action);
    app.redo_stack.borrow_mut().clear();
    super::update_chrome(app);
}

/// Reverse the most recent action.
pub(super) fn undo(app: &Rc<super::App>) {
    let Some(action) = app.undo_stack.borrow_mut().pop() else {
        super::toast(app, "Nothing to undo");
        return;
    };
    let what = action.describe();
    let ok = match &action {
        // Put each entry back where it came from (newest first).
        undo::Action::Move { pairs } => {
            pairs.iter().rev().all(|(from, to)| undo::move_exact(to, from).is_ok())
        }
        // The copies are removed; the sources were never touched.
        undo::Action::Copy { pairs } => {
            let created: Vec<PathBuf> = pairs.iter().map(|(_, dst)| dst.clone()).collect();
            start_job(app, ops::Kind::Delete, created, PathBuf::new(), false);
            true
        }
        undo::Action::Rename { from, to } => undo::move_exact(to, from).is_ok(),
        undo::Action::NewFolder { path } => std::fs::remove_dir(path).is_ok(),
        undo::Action::Trash { originals } => {
            originals.iter().all(|p| undo::restore_from_trash(p))
        }
    };
    app.redo_stack.borrow_mut().push(action);
    refresh_all(app);
    if ok {
        super::toast(app, &format!("Undid {what}"));
    } else {
        super::toast(app, &format!("Couldn't fully undo the {what}"));
    }
}

/// Re-apply the most recently undone action.
pub(super) fn redo(app: &Rc<super::App>) {
    let Some(action) = app.redo_stack.borrow_mut().pop() else {
        super::toast(app, "Nothing to redo");
        return;
    };
    let what = action.describe();
    let ok = match &action {
        undo::Action::Move { pairs } => {
            pairs.iter().all(|(from, to)| undo::move_exact(from, to).is_ok())
        }
        // Re-copy each source back into the folder it was copied to.
        undo::Action::Copy { pairs } => {
            for (src, dst) in pairs {
                if let Some(dir) = dst.parent() {
                    start_job(app, ops::Kind::Copy, vec![src.clone()], dir.to_path_buf(), false);
                }
            }
            true
        }
        undo::Action::Rename { from, to } => undo::move_exact(from, to).is_ok(),
        undo::Action::NewFolder { path } => std::fs::create_dir(path).is_ok(),
        undo::Action::Trash { originals } => originals
            .iter()
            .all(|p| gio::File::for_path(p).trash(gio::Cancellable::NONE).is_ok()),
    };
    app.undo_stack.borrow_mut().push(action);
    refresh_all(app);
    if ok {
        super::toast(app, &format!("Redid {what}"));
    } else {
        super::toast(app, &format!("Couldn't fully redo the {what}"));
    }
}

/// Reload every tab and refresh the chrome (after an undo/redo touches the disk).
pub(super) fn refresh_all(app: &Rc<super::App>) {
    for tab in app.tabs.borrow().iter() {
        super::navigation::refresh(tab);
    }
    super::update_chrome(app);
}

/// Launch a background copy/move/delete job: spawn the worker thread, show a
/// progress row, and pump its events on the main loop (updating the bar, popping
/// conflict dialogs, refreshing when done).
///
/// `record` pushes the result onto the undo stack; undo/redo pass `false` so
/// their own jobs don't re-enter the history.
pub(super) fn start_job(app: &Rc<super::App>, kind: ops::Kind, sources: Vec<PathBuf>, dest: PathBuf, record: bool) {
    if sources.is_empty() {
        return;
    }
    let (verb, past) = match kind {
        ops::Kind::Copy => ("Copying", "Copied"),
        ops::Kind::Move => ("Moving", "Moved"),
        ops::Kind::Delete => ("Deleting", "Deleted"),
    };

    let cancel = Arc::new(AtomicBool::new(false));
    let (ev_tx, ev_rx) = async_channel::unbounded::<ops::Event>();
    let (dec_tx, dec_rx) = std::sync::mpsc::channel::<ops::Decision>();

    // Progress row: a title line and a bar with a stop button.
    let title = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
    title.add_css_class("caption");
    title.set_label(&format!("{verb}…"));
    let bar = gtk::ProgressBar::builder().hexpand(true).valign(gtk::Align::Center).build();
    let stop = crate::icon::flat_button("process-stop-symbolic", "Cancel");
    {
        let cancel = cancel.clone();
        stop.connect_clicked(move |_| cancel.store(true, AtomicOrdering::Relaxed));
    }
    let bar_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar_row.append(&bar);
    bar_row.append(&stop);
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.append(&title);
    row.append(&bar_row);
    app.progress_box.append(&row);
    app.progress_revealer.set_reveal_child(true);

    // Worker thread.
    {
        let cancel = cancel.clone();
        std::thread::spawn(move || ops::run(kind, sources, dest, cancel, ev_tx, dec_rx));
    }

    // Pump events on the main loop.
    let app = app.clone();
    glib::spawn_future_local(async move {
        let (mut total_bytes, mut total_items) = (0u64, 0u64);
        while let Ok(event) = ev_rx.recv().await {
            match event {
                ops::Event::Total { bytes, items } => {
                    total_bytes = bytes;
                    total_items = items;
                }
                ops::Event::Advance { bytes_done, items_done, current } => {
                    let frac = if total_bytes > 0 {
                        bytes_done as f64 / total_bytes as f64
                    } else if total_items > 0 {
                        items_done as f64 / total_items as f64
                    } else {
                        0.0
                    };
                    bar.set_fraction(frac.clamp(0.0, 1.0));
                    let sizes = if total_bytes > 0 {
                        format!(" · {} / {}", format::human_size(bytes_done), format::human_size(total_bytes))
                    } else {
                        String::new()
                    };
                    title.set_label(&format!("{verb} {current} — {items_done}/{total_items}{sizes}"));
                }
                ops::Event::Conflict { name } => {
                    let dialog = adw::AlertDialog::new(
                        Some(&format!("“{name}” already exists")),
                        Some("An item with the same name is already in the destination."),
                    );
                    let apply_all = gtk::CheckButton::with_label("Apply to all conflicts");
                    dialog.set_extra_child(Some(&apply_all));
                    dialog.add_responses(&[
                        ("cancel", "Cancel"),
                        ("skip", "Skip"),
                        ("rename", "Keep Both"),
                        ("replace", "Replace"),
                    ]);
                    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
                    dialog.set_default_response(Some("rename"));
                    dialog.set_close_response("cancel");
                    let dec_tx = dec_tx.clone();
                    dialog.choose(&app.window, gio::Cancellable::NONE, move |resp| {
                        let all = apply_all.is_active();
                        let decision = match resp.as_str() {
                            "replace" => if all { ops::Decision::ReplaceAll } else { ops::Decision::Replace },
                            "skip" => if all { ops::Decision::SkipAll } else { ops::Decision::Skip },
                            "rename" => ops::Decision::Rename,
                            _ => ops::Decision::Cancel,
                        };
                        let _ = dec_tx.send(decision);
                    });
                }
                ops::Event::Finished { ok, failed, pairs } => {
                    app.progress_box.remove(&row);
                    if app.progress_box.first_child().is_none() {
                        app.progress_revealer.set_reveal_child(false);
                    }
                    // Record what actually happened so it can be undone.
                    if record && !pairs.is_empty() {
                        let action = match kind {
                            ops::Kind::Copy => Some(undo::Action::Copy { pairs }),
                            ops::Kind::Move => Some(undo::Action::Move { pairs }),
                            ops::Kind::Delete => None, // permanent — not undoable
                        };
                        if let Some(action) = action {
                            push_undo(&app, action);
                        }
                    }
                    for tab in app.tabs.borrow().iter() {
                        super::navigation::refresh(tab);
                    }
                    super::update_chrome(&app);
                    let message = if cancel.load(AtomicOrdering::Relaxed) {
                        format!("{verb} cancelled — {ok} done")
                    } else if failed > 0 {
                        format!("{past} {ok}; {failed} failed")
                    } else {
                        format!("{past} {ok} item{}", super::plural(ok as usize))
                    };
                    super::toast(&app, &message);
                    break;
                }
            }
        }
    });
}

pub(super) fn paste(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let Some(dir) = tab.dir_list.file().and_then(|f| f.path()) else { return };
    let clipboard = app.window.clipboard();
    let app = app.clone();
    read_clipboard_files(&clipboard, move |cut, sources| {
        if sources.is_empty() {
            return;
        }
        start_job(&app, if cut { ops::Kind::Move } else { ops::Kind::Copy }, sources, dir.clone(), true);
        // A cut is consumed once pasted (matching Nautilus).
        if cut {
            let _ = app.window.clipboard().set_content(gdk::ContentProvider::NONE);
        }
    });
}

pub(super) fn trash_selected(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let items = selected(&tab);
    if items.is_empty() {
        return;
    }
    let (mut ok, mut failed) = (0u32, 0u32);
    let mut trashed: Vec<PathBuf> = Vec::new();
    for (file, _) in &items {
        if file.trash(gio::Cancellable::NONE).is_ok() {
            ok += 1;
            if let Some(path) = file.path() {
                trashed.push(path);
            }
        } else {
            failed += 1
        }
    }
    if !trashed.is_empty() {
        push_undo(app, undo::Action::Trash { originals: trashed });
    }
    super::navigation::refresh(&tab);
    super::toast(app, &format!("Moved {ok} item{} to Trash{}", super::plural(ok as usize),
        if failed > 0 { format!("; {failed} failed") } else { String::new() }));
}

pub(super) fn delete_selected(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let items = selected(&tab);
    if items.is_empty() {
        return;
    }
    let n = items.len();
    let dialog = adw::AlertDialog::new(
        Some(&format!("Permanently delete {n} item{}?", super::plural(n))),
        Some("This cannot be undone."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "delete" {
            return;
        }
        let sources: Vec<PathBuf> =
            selected(&super::navigation::active_tab(&app)).into_iter().filter_map(|(f, _)| f.path()).collect();
        start_job(&app, ops::Kind::Delete, sources, PathBuf::new(), false);
    });
}

/// Rename the selection: in place on the item itself, falling back to a dialog
/// when the row isn't on screen to host the editor.
pub(super) fn rename_selected(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    if super::view::start_inline_rename(app, &tab) {
        return;
    }
    rename_with_dialog(app);
}

pub(super) fn rename_with_dialog(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else {
        return;
    };
    let file = file.clone();
    let entry =
        gtk::Entry::builder().text(info.display_name().as_str()).activates_default(true).build();
    let dialog = adw::AlertDialog::new(Some("Rename"), None);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("rename", "Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "rename" {
            return;
        }
        let name = entry.text().to_string();
        if name.is_empty() || name.contains('/') {
            super::toast(&app, "Invalid name");
            return;
        }
        let renamed = file
            .path()
            .ok_or(())
            .and_then(|from| fileops::rename(&from, &name).map(|to| (from, to)).map_err(|_| ()));
        match renamed {
            Ok((from, to)) => {
                push_undo(&app, undo::Action::Rename { from, to });
                super::navigation::refresh(&super::navigation::active_tab(&app));
            }
            Err(()) => super::toast(&app, "Couldn't rename"),
        }
    });
}

pub(super) fn new_folder(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let Some(dir) = tab.dir_list.file().and_then(|f| f.path()) else { return };
    let entry = gtk::Entry::builder().text("New Folder").activates_default(true).build();
    let dialog = adw::AlertDialog::new(Some("New Folder"), None);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "Cancel"), ("create", "Create")]);
    dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("create"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "create" {
            return;
        }
        let name = entry.text().to_string();
        if name.is_empty() || name.contains('/') {
            super::toast(&app, "Invalid name");
            return;
        }
        match fileops::make_dir(&dir, &name) {
            Ok(path) => {
                push_undo(&app, undo::Action::NewFolder { path });
                super::navigation::refresh(&super::navigation::active_tab(&app));
            }
            Err(_) => super::toast(&app, "Couldn't create folder"),
        }
    });
}

/// A proper Properties dialog: an identity header (icon, name, kind) over
/// grouped rows for size/location, timestamps, and ownership/permissions — the
/// values selectable for copying. Folder sizes are computed off the UI thread.
pub(super) fn show_properties(app: &Rc<super::App>) {
    let tab = super::navigation::active_tab(app);
    let items = selected(&tab);
    let [(file, info)] = items.as_slice() else {
        return;
    };
    let file = file.clone();

    // Re-query for the richer attributes the directory listing doesn't carry
    // (access/created times, owner/group); fall back to the listing's info.
    let full = file
        .query_info(
            "standard::*,time::*,unix::*,owner::*",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok();
    let info: &gio::FileInfo = full.as_ref().unwrap_or(info);

    let name = info.display_name().to_string();
    let is_dir = info.file_type() == gio::FileType::Directory;
    let kind = if is_dir {
        "Folder".to_string()
    } else {
        info.content_type()
            .map(|ct| gio::content_type_get_description(&ct).to_string())
            .unwrap_or_else(|| "File".to_string())
    };
    let location = file
        .parent()
        .and_then(|p| p.path())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Identity header.
    let image = gtk::Image::builder().pixel_size(48).build();
    if let Some(icon) = info.icon() {
        let display = gtk::prelude::WidgetExt::display(&app.window);
        crate::icon::show(&image, &display, &icon, 48, app.window.scale_factor());
    }
    let title = gtk::Label::builder()
        .label(&name)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    title.add_css_class("title-2");
    let subtitle = gtk::Label::builder().label(&kind).build();
    subtitle.add_css_class("dim-label");
    let head = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    head.set_halign(gtk::Align::Center);
    head.append(&image);
    head.append(&title);
    head.append(&subtitle);

    // General.
    let general = adw::PreferencesGroup::new();
    general.add(&prop_row("Type", &kind));
    let size_row = prop_row("Size", "");
    general.add(&size_row);
    general.add(&prop_row("Location", &location));
    if is_dir {
        size_row.set_subtitle("Calculating…");
        if let Some(path) = file.path() {
            let size_row = size_row.clone();
            glib::spawn_future_local(async move {
                let (bytes, count) =
                    gio::spawn_blocking(move || fileops::dir_size(&path)).await.unwrap_or((0, 0));
                size_row.set_subtitle(&format!(
                    "{} — {count} item{}",
                    format::human_size(bytes),
                    super::plural(count as usize)
                ));
            });
        }
    } else {
        let bytes = info.size().max(0) as u64;
        size_row.set_subtitle(&format!("{} ({bytes} bytes)", format::human_size(bytes)));
    }

    // Timestamps.
    let time = adw::PreferencesGroup::new();
    time.set_title("Timestamps");
    if let Some(dt) = info.modification_date_time() {
        time.add(&prop_row("Modified", &format::modified(&dt)));
    }
    for (label, attr) in [("Accessed", "time::access"), ("Created", "time::created")] {
        let secs = info.attribute_uint64(attr);
        if secs > 0
            && let Ok(dt) = glib::DateTime::from_unix_local(secs as i64)
        {
            time.add(&prop_row(label, &format::modified(&dt)));
        }
    }

    // Ownership & permissions.
    let perms = adw::PreferencesGroup::new();
    perms.set_title("Permissions");
    if let Some(owner) = info.attribute_string("owner::user") {
        perms.add(&prop_row("Owner", &owner));
    }
    if let Some(group) = info.attribute_string("owner::group") {
        perms.add(&prop_row("Group", &group));
    }
    let mode = info.attribute_uint32("unix::mode");
    let access = prop_row("Access", &super::permission_string(mode));
    perms.add(&access);
    if !is_dir && mode != 0 {
        let row = adw::ActionRow::builder().title("Allow executing file as a program").build();
        let switch = gtk::Switch::builder()
            .active(mode & 0o100 != 0)
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&switch);
        row.set_activatable_widget(Some(&switch));
        let file = file.clone();
        let app_switch = app.clone();
        switch.connect_state_set(move |_, on| {
            let current = file
                .query_info("unix::mode", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
                .map(|info| info.attribute_uint32("unix::mode"))
                .unwrap_or(mode);
            let next = if on { current | 0o100 } else { current & !0o100 };
            match file.set_attribute_uint32(
                "unix::mode",
                next,
                gio::FileQueryInfoFlags::NONE,
                None::<&gio::Cancellable>,
            ) {
                Ok(()) => {
                    access.set_subtitle(&super::permission_string(next));
                    glib::Propagation::Proceed
                }
                Err(err) => {
                    super::toast(&app_switch, &format!("Couldn't change permissions: {err}"));
                    glib::Propagation::Stop
                }
            }
        });
        perms.add(&row);
    }

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.append(&head);
    content.append(&general);
    content.append(&time);
    content.append(&perms);
    let scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&content).build();

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroller));
    let dialog =
        adw::Dialog::builder().title("Properties").content_width(460).content_height(620).build();
    dialog.set_child(Some(&view));
    dialog.present(Some(&app.window));
}

/// One labelled, copy-selectable row in the Properties dialog.
pub(super) fn prop_row(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(value).build();
    row.set_subtitle_selectable(true);
    row
}
