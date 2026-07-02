// Bookmarked folders, persisted one path per line under the user's config dir
// (~/.config/filescope/bookmarks) — filescope's own file, so it never disturbs
// Nautilus's bookmarks.

use std::path::PathBuf;

fn bookmarks_file() -> PathBuf {
    let mut dir = gtk::glib::user_config_dir();
    dir.push("filescope");
    dir.push("bookmarks");
    dir
}

/// Load the saved bookmarks, dropping any that no longer point at a directory.
pub fn load() -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(bookmarks_file()) else {
        return Vec::new();
    };
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

/// Persist `list` (best-effort — creating the config dir if needed).
pub fn save(list: &[PathBuf]) {
    let path = bookmarks_file();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body: String =
        list.iter().map(|p| format!("{}\n", p.to_string_lossy())).collect();
    let _ = std::fs::write(path, body);
}
