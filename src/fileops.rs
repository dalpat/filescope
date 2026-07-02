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

/// Total apparent size (bytes) and entry count of a directory tree, for the
/// Properties dialog. Symlinks are counted as entries but not followed (so the
/// walk stays cycle-free and nothing outside the tree is double-counted), and
/// unreadable sub-entries are skipped rather than aborting the walk. Can be slow
/// on a large tree — run it off the UI thread.
pub fn dir_size(path: &Path) -> (u64, u64) {
    let (mut bytes, mut count) = (0u64, 0u64);
    let Ok(entries) = fs::read_dir(path) else {
        return (bytes, count);
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        count += 1;
        if meta.is_dir() {
            let (b, c) = dir_size(&entry.path());
            bytes += b;
            count += c;
        } else {
            bytes += meta.len();
        }
    }
    (bytes, count)
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
    fn remove_deletes_a_file_and_a_whole_tree() {
        let tmp = std::env::temp_dir().join(format!("filescope-rm-{}", std::process::id()));
        fs::create_dir_all(tmp.join("tree/sub")).unwrap();
        fs::write(tmp.join("tree/a.txt"), b"hello").unwrap();
        fs::write(tmp.join("tree/sub/b.txt"), b"world").unwrap();
        let file = tmp.join("lone.bin");
        fs::write(&file, b"x").unwrap();

        // A whole directory tree is removed recursively…
        remove(&tmp.join("tree")).unwrap();
        assert!(!tmp.join("tree").exists());
        // …and a single file too.
        remove(&file).unwrap();
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&tmp);
    }
}
