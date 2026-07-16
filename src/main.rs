// filescope — a simple, fast, native file manager for Linux.
//
// Built on GTK4 + libadwaita. The directory view is virtualized
// (GtkDirectoryList → GtkColumnView) so even a folder with 100k entries
// realizes only the rows on screen. Icon and (via libadwaita) light/dark +
// accent theming follow the user's GNOME settings automatically.

mod bookmarks;
mod fileops;
mod format;
mod ops;
mod preview;
mod settings;
mod trash;
mod undo;
mod window;

use adw::prelude::*;

const APP_ID: &str = "dev.filescope.Filescope";

fn main() -> gtk::glib::ExitCode {
    // Optional: a folder to open at launch, e.g. `filescope ~/Downloads`.
    let initial = std::env::args().nth(1);

    // filescope is single-instance: launching it again raises the running window.
    // Screenshot mode must be its own process, or it would hijack that instance
    // (and capture nothing) instead of rendering its own window.
    let flags = if std::env::var_os("FILESCOPE_SHOT").is_some() {
        gtk::gio::ApplicationFlags::NON_UNIQUE
    } else {
        gtk::gio::ApplicationFlags::empty()
    };

    let app = adw::Application::builder().application_id(APP_ID).flags(flags).build();
    app.connect_activate(move |app| window::build(app, initial.clone()));
    app.run_with_args::<&str>(&[])
}
