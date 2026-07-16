// The Trash, per the freedesktop.org spec: a `Trash` directory holding `files/`
// (the trashed items) and `info/<name>.trashinfo` (where each came from, and when).
//
// Every operation takes the trash root explicitly so it can be tested against a
// temporary directory; the app passes `home_trash()`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One item sitting in the Trash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The item's name inside `files/` (unique within the trash).
    pub name: String,
    /// Where it came from, to restore it to.
    pub original: PathBuf,
    /// Deletion timestamp as recorded (`YYYY-MM-DDTHH:MM:SS`), if present.
    pub deleted: Option<String>,
}

/// The user's home trash: `$XDG_DATA_HOME/Trash` (i.e. `~/.local/share/Trash`).
pub fn home_trash() -> PathBuf {
    let mut dir = gtk::glib::user_data_dir();
    dir.push("Trash");
    dir
}

/// Everything currently in the trash at `root`, newest-recorded first.
///
/// An item is only listed when both its `info/*.trashinfo` and the matching entry
/// in `files/` exist, so a half-removed item never shows up as a ghost.
pub fn entries_in(root: &Path) -> Vec<Entry> {
    let Ok(infos) = fs::read_dir(root.join("info")) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = infos
        .flatten()
        .filter_map(|info| {
            let info_path = info.path();
            let name = info_path.file_name()?.to_str()?.strip_suffix(".trashinfo")?.to_string();
            // Skip an info file whose item is no longer there.
            if root.join("files").join(&name).symlink_metadata().is_err() {
                return None;
            }
            let text = fs::read_to_string(&info_path).ok()?;
            let original = value_of(&text, "Path").map(|p| PathBuf::from(percent_decode(&p)))?;
            Some(Entry { name, original, deleted: value_of(&text, "DeletionDate") })
        })
        .collect();
    // Newest first; ISO-8601 timestamps sort lexicographically.
    entries.sort_by(|a, b| b.deleted.cmp(&a.deleted).then_with(|| a.name.cmp(&b.name)));
    entries
}

/// Restore `entry` from the trash at `root` back to where it came from, returning
/// the path it landed at. If something now occupies the original path, it is
/// restored beside it under a free "(copy)"-style name rather than clobbering.
pub fn restore_in(root: &Path, entry: &Entry) -> io::Result<PathBuf> {
    let parent = entry
        .original
        .parent()
        .ok_or_else(|| io::Error::other("trashed item has no original folder"))?;
    fs::create_dir_all(parent)?;

    let name = entry
        .original
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("trashed item has no name"))?;
    let target = crate::fileops::unique_destination(parent, name);

    crate::undo::move_exact(&root.join("files").join(&entry.name), &target)?;
    let _ = fs::remove_file(root.join("info").join(format!("{}.trashinfo", entry.name)));
    Ok(target)
}

/// Permanently remove everything in the trash at `root`, leaving the trash
/// directories themselves in place.
pub fn empty_in(root: &Path) -> io::Result<()> {
    for dir in ["files", "info"] {
        let Ok(items) = fs::read_dir(root.join(dir)) else {
            continue;
        };
        for item in items.flatten() {
            crate::fileops::remove(&item.path())?;
        }
    }
    Ok(())
}

/// The value of `key=` in a `.trashinfo` body.
fn value_of(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.split_once('=').filter(|(k, _)| k.trim() == key))
        .map(|(_, v)| v.trim().to_string())
}

/// Decode the percent-escapes the spec uses in `Path`.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a trash root containing `name` trashed from `original`.
    fn fake_trash(dir: &Path, name: &str, original: &str, date: &str, body: &[u8]) {
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::create_dir_all(dir.join("info")).unwrap();
        fs::write(dir.join("files").join(name), body).unwrap();
        fs::write(
            dir.join("info").join(format!("{name}.trashinfo")),
            format!("[Trash Info]\nPath={original}\nDeletionDate={date}\n"),
        )
        .unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("filescope-trash-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn lists_entries_with_their_original_path_and_date() {
        let root = tmp("list");
        fake_trash(&root, "notes.txt", "/home/me/notes.txt", "2026-07-14T10:00:00", b"x");
        let entries = entries_in(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "notes.txt");
        assert_eq!(entries[0].original, PathBuf::from("/home/me/notes.txt"));
        assert_eq!(entries[0].deleted.as_deref(), Some("2026-07-14T10:00:00"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_or_missing_trash_lists_nothing() {
        assert!(entries_in(&tmp("missing")).is_empty());
        let root = tmp("empty");
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        assert!(entries_in(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn info_without_a_matching_file_is_not_listed() {
        // A stale .trashinfo whose item is gone must not appear as a ghost entry.
        let root = tmp("ghost");
        fs::create_dir_all(root.join("files")).unwrap();
        fs::create_dir_all(root.join("info")).unwrap();
        fs::write(
            root.join("info/gone.txt.trashinfo"),
            "[Trash Info]\nPath=/home/me/gone.txt\nDeletionDate=2026-07-14T10:00:00\n",
        )
        .unwrap();
        assert!(entries_in(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn percent_encoded_paths_are_decoded() {
        // The spec percent-encodes the Path value.
        let root = tmp("encoded");
        fake_trash(&root, "my file", "/home/me/my%20file", "2026-07-14T10:00:00", b"x");
        assert_eq!(entries_in(&root)[0].original, PathBuf::from("/home/me/my file"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_puts_the_item_back_and_clears_its_info() {
        let root = tmp("restore");
        let home = tmp("restore-home");
        fs::create_dir_all(&home).unwrap();
        let original = home.join("notes.txt");
        fake_trash(&root, "notes.txt", original.to_str().unwrap(), "2026-07-14T10:00:00", b"hello");

        let entry = entries_in(&root).remove(0);
        let landed = restore_in(&root, &entry).unwrap();

        assert_eq!(landed, original);
        assert_eq!(fs::read(&original).unwrap(), b"hello");
        assert!(!root.join("files/notes.txt").exists(), "removed from the trash");
        assert!(!root.join("info/notes.txt.trashinfo").exists(), "its info is cleared");
        assert!(entries_in(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn restore_does_not_clobber_something_back_at_the_original_path() {
        let root = tmp("clobber");
        let home = tmp("clobber-home");
        fs::create_dir_all(&home).unwrap();
        let original = home.join("notes.txt");
        fs::write(&original, b"newer").unwrap(); // something took the name back
        fake_trash(&root, "notes.txt", original.to_str().unwrap(), "2026-07-14T10:00:00", b"older");

        let entry = entries_in(&root).remove(0);
        let landed = restore_in(&root, &entry).unwrap();

        assert_ne!(landed, original, "restored beside it, not over it");
        assert_eq!(fs::read(&original).unwrap(), b"newer", "the existing file is untouched");
        assert_eq!(fs::read(&landed).unwrap(), b"older");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn empty_removes_every_item_and_its_info() {
        let root = tmp("emptying");
        fake_trash(&root, "a.txt", "/home/me/a.txt", "2026-07-14T10:00:00", b"a");
        fake_trash(&root, "b.txt", "/home/me/b.txt", "2026-07-14T11:00:00", b"b");
        fs::create_dir_all(root.join("files/adir/inner")).unwrap();
        fs::write(root.join("files/adir/inner/f"), b"x").unwrap();

        empty_in(&root).unwrap();

        assert!(entries_in(&root).is_empty());
        assert_eq!(fs::read_dir(root.join("files")).unwrap().count(), 0);
        assert_eq!(fs::read_dir(root.join("info")).unwrap().count(), 0);
        assert!(root.join("files").is_dir(), "the trash itself survives");
        let _ = fs::remove_dir_all(&root);
    }
}
