// filescope — a simple, fast, native file manager for Linux.
//
// Built on GTK4 + libadwaita. The directory view is virtualized
// (GtkDirectoryList → GtkColumnView) so even a folder with 100k entries
// realizes only the rows on screen. Icon and (via libadwaita) light/dark +
// accent theming follow the user's GNOME settings automatically.

mod archive;
mod batch_rename;
mod bookmarks;
mod file_manager;
mod fileops;
mod format;
mod icon;
mod ops;
mod picker;
mod preview;
mod settings;
mod search_provider;
mod thumbnails;
mod trash;
mod undo;
mod update;
mod window;

use adw::prelude::*;

const APP_ID: &str = "dev.filescope.Filescope";

fn main() -> gtk::glib::ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--update-probe")) {
        return if update::print_probe() { gtk::glib::ExitCode::SUCCESS } else { gtk::glib::ExitCode::FAILURE };
    }
    match std::env::args_os().nth(1).as_deref().and_then(std::ffi::OsStr::to_str) {
        Some("--file-picker-portal") => return picker::run(),
        Some(option @ ("--enable-file-picker" | "--disable-file-picker")) => {
            return match gtk::glib::MainContext::default().block_on(picker::config::set_enabled(option == "--enable-file-picker")) {
                Ok(note) => { println!("{note}"); gtk::glib::ExitCode::SUCCESS }
                Err(error) => { eprintln!("filescope: {error}"); gtk::glib::ExitCode::FAILURE }
            };
        }
        _ => {}
    }
    // Single instance: a second launch is delivered here as `open` (with the
    // folder) or `activate` (with nothing). Screenshot mode must stay its own
    // process, or it would raise the running window and capture nothing.
    let mut flags = gtk::gio::ApplicationFlags::HANDLES_OPEN;
    if std::env::var_os("FILESCOPE_SHOT").is_some() {
        flags |= gtk::gio::ApplicationFlags::NON_UNIQUE;
    }

    let app = search_provider::new_application(flags);
    app.connect_startup(|app| file_manager::own(app));
    app.connect_activate(|app| window::open_files(app, &[]));
    app.connect_open(|app, files, _| window::open_files(app, files));
    app.run()
}
