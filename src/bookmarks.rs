// Bookmarked folders, persisted one path per line under the user's config dir
// (~/.config/filescope/bookmarks) — filescope's own file, so it never disturbs
// Nautilus's bookmarks.

use std::path::{Path, PathBuf};

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

/// Move bookmark `from` to sit just before or just after `to`.
/// False when either path is missing or the order would not change.
pub fn reorder(list: &mut Vec<PathBuf>, from: &Path, to: &Path, after: bool) -> bool {
    let Some(from_i) = list.iter().position(|p| p == from) else { return false };
    let Some(to_i) = list.iter().position(|p| p == to) else { return false };
    if from_i == to_i {
        return false;
    }
    // Index of `to` once `from` has been taken out.
    let to_i = if from_i < to_i { to_i - 1 } else { to_i };
    let dest = if after { to_i + 1 } else { to_i };
    if dest == from_i {
        return false;
    }
    let item = list.remove(from_i);
    list.insert(dest, item);
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn reorder_moves_a_bookmark_before_or_after_another() {
        let mut list = paths(&["/a", "/b", "/c"]);
        assert!(reorder(&mut list, Path::new("/c"), Path::new("/a"), false));
        assert_eq!(list, paths(&["/c", "/a", "/b"]));

        assert!(reorder(&mut list, Path::new("/c"), Path::new("/b"), true));
        assert_eq!(list, paths(&["/a", "/b", "/c"]));
    }

    #[test]
    fn reorder_ignores_a_drop_that_does_not_change_order() {
        let mut list = paths(&["/a", "/b"]);
        assert!(!reorder(&mut list, Path::new("/a"), Path::new("/a"), false));
        assert!(!reorder(&mut list, Path::new("/b"), Path::new("/a"), true));
        assert!(!reorder(&mut list, Path::new("/missing"), Path::new("/a"), false));
        assert_eq!(list, paths(&["/a", "/b"]));
    }
}
