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
mod preview;
mod settings;
mod search_provider;
mod thumbnails;
mod trash;
mod undo;
mod window;

use adw::prelude::*;

const APP_ID: &str = "dev.filescope.Filescope";

fn main() -> gtk::glib::ExitCode {
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
