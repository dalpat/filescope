// File operations: copy, move, delete, rename, new folder. Local-filesystem
// operations use std::fs (fast, reliable); trashing lives in the UI layer via
// GIO since std has no trash concept.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A destination path inside `dir` for an entry named `name` that does not
/// collide with anything already there — appending " (copy)", " (copy 2)", …
/// before the extension as needed.
pub fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_name(name);
    for n in 1.. {
        let suffix = if n == 1 { " (copy)".to_string() } else { format!(" (copy {n})") };
        let candidate = dir.join(format!("{stem}{suffix}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// Split a filename into (stem, extension-including-dot). A leading dot (dotfile)
/// is treated as part of the stem, not an extension.
fn split_name(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Copy `src` into directory `dir` (recursively for directories), choosing a
/// non-colliding destination name. Returns the path created.
pub fn copy_into(src: &Path, dir: &Path) -> io::Result<PathBuf> {
    let name = file_name(src)?;
    let dest = unique_destination(dir, &name);
    copy_recursive(src, &dest)?;
    Ok(dest)
}

/// Move `src` into directory `dir`, choosing a non-colliding destination name.
/// Uses a rename when possible, falling back to copy-then-delete across
/// filesystems. Returns the path created.
pub fn move_into(src: &Path, dir: &Path) -> io::Result<PathBuf> {
    let name = file_name(src)?;
    let dest = unique_destination(dir, &name);
    match fs::rename(src, &dest) {
        Ok(()) => Ok(dest),
        // EXDEV (cross-device): copy then remove the original.
        Err(_) => {
            copy_recursive(src, &dest)?;
            remove(src)?;
            Ok(dest)
        }
    }
}

/// Recursively copy `src` to the exact path `dest`.
pub fn copy_recursive(src: &Path, dest: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dest)?;
    } else if meta.is_dir() {
        fs::create_dir(dest)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dest.join(entry.file_name()))?;
        }
    } else {
        fs::copy(src, dest)?;
    }
    Ok(())
}

/// Permanently remove `path` — a single file/symlink, or a whole directory tree.
pub fn remove(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Create a new directory named `name` inside `dir`. Errors if it already exists.
pub fn make_dir(dir: &Path, name: &str) -> io::Result<PathBuf> {
    let path = dir.join(name);
    fs::create_dir(&path)?;
    Ok(path)
}

/// Rename the entry at `path` to `new_name` (kept in the same directory).
pub fn rename(path: &Path, new_name: &str) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| io::Error::other("no parent directory"))?;
    let dest = parent.join(new_name);
    fs::rename(path, &dest)?;
    Ok(dest)
}

/// The final path component as a string, or an error for a path that has none.
fn file_name(path: &Path) -> io::Result<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| io::Error::other("path has no file name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_name_handles_extensions_and_dotfiles() {
        assert_eq!(split_name("photo.jpg"), ("photo", ".jpg"));
        assert_eq!(split_name("archive.tar.gz"), ("archive.tar", ".gz"));
        assert_eq!(split_name("README"), ("README", ""));
        assert_eq!(split_name(".bashrc"), (".bashrc", ""));
    }

    #[test]
    fn unique_destination_avoids_collisions() {
        let tmp = std::env::temp_dir().join(format!("filescope-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        let name = "file.txt";
        // Nothing there yet → the plain name.
        assert_eq!(unique_destination(&tmp, name), tmp.join("file.txt"));
        // With the original present → " (copy)".
        fs::write(tmp.join(name), b"x").unwrap();
        assert_eq!(unique_destination(&tmp, name), tmp.join("file (copy).txt"));
        // With the copy present too → " (copy 2)".
        fs::write(tmp.join("file (copy).txt"), b"x").unwrap();
        assert_eq!(unique_destination(&tmp, name), tmp.join("file (copy 2).txt"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn copy_and_remove_round_trip_a_tree() {
        let tmp = std::env::temp_dir().join(format!("filescope-tree-{}", std::process::id()));
        let src = tmp.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("sub/b.txt"), b"world").unwrap();

        let dest_dir = tmp.join("dest");
        fs::create_dir_all(&dest_dir).unwrap();
        let created = copy_into(&src, &dest_dir).unwrap();
        assert!(created.join("a.txt").exists());
        assert!(created.join("sub/b.txt").exists());
        assert_eq!(fs::read(created.join("a.txt")).unwrap(), b"hello");

        remove(&created).unwrap();
        assert!(!created.exists());
        let _ = fs::remove_dir_all(&tmp);
    }
}
