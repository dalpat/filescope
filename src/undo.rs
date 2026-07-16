// The undo/redo model: what happened, and how to reverse it.
//
// Actions record the *exact* paths involved (post conflict-resolution), so undo
// restores things where they came from rather than guessing. Permanent deletes
// are never recorded — they can't be reversed.

use std::fs;
use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::prelude::*;

/// A reversible thing the user did.
#[derive(Clone)]
pub enum Action {
    /// Entries moved, as (from, to).
    Move { pairs: Vec<(PathBuf, PathBuf)> },
    /// Entries copied, as (source, created copy).
    Copy { pairs: Vec<(PathBuf, PathBuf)> },
    Rename { from: PathBuf, to: PathBuf },
    NewFolder { path: PathBuf },
    /// Entries sent to Trash, by their original paths.
    Trash { originals: Vec<PathBuf> },
}

impl Action {
    /// Short past-tense description, e.g. for "Undo — renamed 'x'".
    pub fn describe(&self) -> String {
        match self {
            Action::Move { pairs } => format!("move of {}", count(pairs.len())),
            Action::Copy { pairs } => format!("copy of {}", count(pairs.len())),
            Action::Rename { .. } => "rename".to_string(),
            Action::NewFolder { .. } => "new folder".to_string(),
            Action::Trash { originals } => format!("trashing of {}", count(originals.len())),
        }
    }
}

fn count(n: usize) -> String {
    format!("{n} item{}", if n == 1 { "" } else { "s" })
}

/// Move `from` back to `to` exactly. Same-filesystem renames are instant; a
/// cross-filesystem entry falls back to a copy followed by removing the source.
pub fn move_exact(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        // EXDEV and friends: fall back to copy + delete.
        Err(_) => {
            copy_tree(from, to)?;
            crate::fileops::remove(from)
        }
    }
}

/// Recursive copy to an exact path (used only by the cross-filesystem fallback).
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
        let _ = fs::set_permissions(dst, meta.permissions());
    } else {
        fs::copy(src, dst)?;
    }
    Ok(())
}

/// Restore a trashed item back to `original`. Returns whether it was restored.
///
/// The home trash goes through the (tested) [`crate::trash`] module; anything
/// trashed to another volume's `.Trash-<uid>` falls back to a `trash:///` scan,
/// which GIO aggregates across every mount.
pub fn restore_from_trash(original: &Path) -> bool {
    let root = crate::trash::home_trash();
    if let Some(entry) = crate::trash::entries_in(&root).into_iter().find(|e| e.original == original)
    {
        return crate::trash::restore_in(&root, &entry).is_ok();
    }
    restore_via_gio(original)
}

/// Fallback for items trashed onto another volume.
fn restore_via_gio(original: &Path) -> bool {
    let trash = gio::File::for_uri("trash:///");
    let Ok(entries) = trash.enumerate_children(
        "standard::name,trash::orig-path",
        gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
        gio::Cancellable::NONE,
    ) else {
        return false;
    };
    for info in entries.flatten() {
        let orig = info.attribute_byte_string("trash::orig-path").map(PathBuf::from);
        if orig.as_deref() != Some(original) {
            continue;
        }
        let trashed = trash.child(info.name());
        let target = gio::File::for_path(original);
        if trashed
            .move_(&target, gio::FileCopyFlags::NONE, gio::Cancellable::NONE, None)
            .is_ok()
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_exact_restores_a_file_to_its_original_path() {
        let tmp = std::env::temp_dir().join(format!("filescope-undo-{}", std::process::id()));
        fs::create_dir_all(tmp.join("a")).unwrap();
        fs::create_dir_all(tmp.join("b")).unwrap();
        let from = tmp.join("b/note.txt");
        let to = tmp.join("a/note.txt");
        fs::write(&from, b"hi").unwrap();

        move_exact(&from, &to).unwrap();
        assert!(!from.exists(), "source is gone");
        assert_eq!(fs::read(&to).unwrap(), b"hi", "restored at the exact path");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn describe_reads_naturally() {
        let a = Action::Rename { from: "x".into(), to: "y".into() };
        assert_eq!(a.describe(), "rename");
        let m = Action::Move { pairs: vec![("a".into(), "b".into())] };
        assert_eq!(m.describe(), "move of 1 item");
        let t = Action::Trash { originals: vec!["a".into(), "b".into()] };
        assert_eq!(t.describe(), "trashing of 2 items");
    }
}
