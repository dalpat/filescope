// filescope — a simple, fast, native file manager for Linux.
//
// Built on GTK4 + libadwaita. The directory view is virtualized
// (GtkDirectoryList → GtkColumnView) so even a folder with 100k entries
// realizes only the rows on screen. Icon and (via libadwaita) light/dark +
// accent theming follow the user's GNOME settings automatically.

mod bookmarks;
mod fileops;
mod format;
mod preview;
mod window;

use adw::prelude::*;

const APP_ID: &str = "dev.filescope.Filescope";

fn main() -> gtk::glib::ExitCode {
    // Optional: a folder to open at launch, e.g. `filescope ~/Downloads`.
    let initial = std::env::args().nth(1);

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| window::build(app, initial.clone()));
    app.run_with_args::<&str>(&[])
}
