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
    let source = root.join("files").join(&entry.name);
    let (stem, ext) = crate::fileops::split_name(name);
    let mut target = parent.join(name);
    for n in 1.. {
        match move_without_replace(&source, &target) {
            Ok(()) => break,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let suffix = if n == 1 { " (copy)".to_string() } else { format!(" (copy {n})") };
                target = parent.join(format!("{stem}{suffix}{ext}"));
            }
            Err(error) => return Err(error),
        }
    }
    let _ = fs::remove_file(root.join("info").join(format!("{}.trashinfo", entry.name)));
    Ok(target)
}

/// Reserve the destination atomically, including dangling symlinks and races.
fn move_without_replace(source: &Path, target: &Path) -> io::Result<()> {
    let error = match crate::fileops::rename_no_replace(source, target) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    if error.raw_os_error() != Some(libc::EXDEV) {
        return Err(error);
    }
    copy_without_replace(source, target)?;
    crate::fileops::remove(source)
}

/// Cross-filesystem restores keep the entire source until copying succeeds.
fn copy_without_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

    let meta = fs::symlink_metadata(source)?;
    if meta.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)
    } else if meta.is_dir() {
        // Keep private directory contents inaccessible until the copy is complete.
        fs::DirBuilder::new().mode(0o700).create(target)?;
        let copy = || -> io::Result<()> {
            for child in fs::read_dir(source)? {
                let child = child?;
                copy_without_replace(&child.path(), &target.join(child.file_name()))?;
            }
            fs::set_permissions(target, meta.permissions())
        };
        copy().map_err(|error| io::Error::other(format!("{error}; partial copy may remain at {}", target.display())))
    } else if meta.is_file() {
        let mut input = fs::File::open(source)?;
        let mut output = fs::OpenOptions::new().write(true).create_new(true)
            .mode(meta.permissions().mode()).open(target)?;
        io::copy(&mut input, &mut output)?;
        fs::set_permissions(target, meta.permissions())
    } else {
        Err(io::Error::new(io::ErrorKind::Unsupported, "Cannot restore this file type across filesystems"))
    }
}

/// Snapshot only the selected Trash identities; selecting never touches disk.
#[cfg(test)]
pub fn selected_entries(entries: &[Entry], positions: impl IntoIterator<Item = usize>) -> Vec<Entry> {
    positions.into_iter().filter_map(|position| entries.get(position).cloned()).collect()
}

/// Try every selected entry, retaining both successful destinations and failures.
pub fn restore_selected_in(root: &Path, entries: &[Entry]) -> Vec<(Entry, io::Result<PathBuf>)> {
    entries.iter().map(|entry| (entry.clone(), restore_in(root, entry))).collect()
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

    #[test]
    fn trash_selection_does_not_restore() {
        let root = tmp("selection");
        let original = root.join("original/notes.txt");
        fake_trash(&root, "notes.txt", original.to_str().unwrap(), "", b"notes");
        let entries = entries_in(&root);
        let info = fs::read(root.join("info/notes.txt.trashinfo")).unwrap();
        assert_eq!(selected_entries(&entries, [0]), entries);
        assert!(selected_entries(&entries, []).is_empty());
        assert!(!original.exists());
        assert_eq!(fs::read(root.join("files/notes.txt")).unwrap(), b"notes");
        assert_eq!(fs::read(root.join("info/notes.txt.trashinfo")).unwrap(), info);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_uses_only_selected_entries() {
        let root = tmp("selected");
        // Same original basename in different folders, distinct Trash identities.
        for name in ["a", "b", "c"] {
            let original = root.join(name).join("notes.txt");
            fake_trash(&root, name, original.to_str().unwrap(), "", name.as_bytes());
        }
        let entries = entries_in(&root);
        let results = restore_selected_in(&root, &selected_entries(&entries, [0, 2]));
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|(_, result)| result.is_ok()));
        assert_eq!(fs::read(root.join("a/notes.txt")).unwrap(), b"a");
        assert_eq!(fs::read(root.join("c/notes.txt")).unwrap(), b"c");
        assert_eq!(entries_in(&root), vec![entries[1].clone()]);
        assert!(root.join("info/b.trashinfo").exists());
        assert!(!root.join("b/notes.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_never_overwrites_existing_destination() {
        let root = tmp("no-overwrite");
        let original = root.join("original/notes.txt");
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        fs::write(&original, b"new").unwrap();
        fake_trash(&root, "notes.txt", original.to_str().unwrap(), "", b"old");
        let landed = restore_in(&root, &entries_in(&root)[0]).unwrap();
        assert_eq!(landed, root.join("original/notes (copy).txt"));
        assert_eq!(fs::read(&original).unwrap(), b"new");
        assert_eq!(fs::read(landed).unwrap(), b"old");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_preserves_a_dangling_destination_symlink() {
        let root = tmp("symlink-collision");
        let original = root.join("original/notes.txt");
        fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("missing", &original).unwrap();
        fake_trash(&root, "notes.txt", original.to_str().unwrap(), "", b"old");
        let landed = restore_in(&root, &entries_in(&root)[0]).unwrap();
        assert_ne!(landed, original);
        assert_eq!(fs::read_link(original).unwrap(), PathBuf::from("missing"));
        assert_eq!(fs::read(landed).unwrap(), b"old");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_batch_keeps_failed_entries() {
        let root = tmp("partial");
        let blocked = root.join("blocked");
        fake_trash(&root, "a-fails", blocked.join("notes.txt").to_str().unwrap(), "", b"retry");
        fake_trash(&root, "b-works", root.join("ok/notes.txt").to_str().unwrap(), "", b"ok");
        fs::write(&blocked, b"not a directory").unwrap();
        let entries = entries_in(&root);
        let results = restore_selected_in(&root, &entries);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, entries[0]);
        assert!(results[0].1.is_err());
        assert_eq!(results[1].0, entries[1]);
        assert_eq!(results[1].1.as_ref().unwrap(), &root.join("ok/notes.txt"));
        assert_eq!(fs::read(root.join("ok/notes.txt")).unwrap(), b"ok");
        assert_eq!(fs::read(root.join("files/a-fails")).unwrap(), b"retry");
        assert_eq!(entries_in(&root), vec![entries[0].clone()]);
        assert!(root.join("info/a-fails.trashinfo").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cross_filesystem_restore_preserves_files_directories_and_symlinks() {
        use std::os::unix::fs::MetadataExt;
        let root = tmp("cross-device");
        let destination = PathBuf::from("/dev/shm").join(format!("filescope-restore-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        if !fs::metadata("/dev/shm").is_ok_and(|meta| meta.dev() != fs::metadata(&root).unwrap().dev()) {
            fs::remove_dir_all(root).unwrap();
            return; // This environment has no second filesystem for the integration check.
        }
        let _ = fs::remove_dir_all(&destination);
        fs::create_dir(&destination).unwrap();
        assert_ne!(fs::metadata(&root).unwrap().dev(), fs::metadata(&destination).unwrap().dev());
        for name in ["file.txt", "folder", "link"] {
            fake_trash(&root, name, destination.join(name).to_str().unwrap(), "", b"contents");
        }
        fs::remove_file(root.join("files/folder")).unwrap();
        fs::create_dir(root.join("files/folder")).unwrap();
        fs::write(root.join("files/folder/child"), b"child").unwrap();
        fs::remove_file(root.join("files/link")).unwrap();
        std::os::unix::fs::symlink("file.txt", root.join("files/link")).unwrap();
        fs::write(destination.join("file.txt"), b"existing").unwrap();
        let results = restore_selected_in(&root, &entries_in(&root));
        assert!(results.iter().all(|(_, result)| result.is_ok()), "{results:?}");
        assert_eq!(fs::read(destination.join("file.txt")).unwrap(), b"existing");
        assert_eq!(fs::read(destination.join("file (copy).txt")).unwrap(), b"contents");
        assert_eq!(fs::read(destination.join("folder/child")).unwrap(), b"child");
        assert_eq!(fs::read_link(destination.join("link")).unwrap(), PathBuf::from("file.txt"));
        assert!(entries_in(&root).is_empty());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(destination).unwrap();
    }

    #[test]
    fn failed_cross_filesystem_restore_keeps_the_whole_source() {
        use std::os::unix::fs::MetadataExt;
        let root = tmp("cross-device-failure");
        let destination = PathBuf::from("/dev/shm").join(format!("filescope-restore-failure-{}", std::process::id()));
        fake_trash(&root, "folder", destination.to_str().unwrap(), "", b"");
        if !fs::metadata("/dev/shm").is_ok_and(|meta| meta.dev() != fs::metadata(&root).unwrap().dev()) {
            fs::remove_dir_all(root).unwrap();
            return;
        }
        let _ = fs::remove_dir_all(&destination);
        fs::remove_file(root.join("files/folder")).unwrap();
        fs::create_dir(root.join("files/folder")).unwrap();
        fs::write(root.join("files/folder/child"), b"keep").unwrap();
        let socket = std::os::unix::net::UnixListener::bind(root.join("files/folder/socket")).unwrap();
        let entries = entries_in(&root);
        let result = restore_in(&root, &entries[0]);
        assert!(result.is_err());
        assert_eq!(fs::read(root.join("files/folder/child")).unwrap(), b"keep");
        assert_eq!(entries_in(&root), entries);
        drop(socket);
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(destination).unwrap();
    }

    // Limits and potentially blocking file types must stay out of the test runner.
    fn isolated_copy_test(name: &str, root: &Path) -> bool {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("trash::tests::{name}"), "--nocapture"])
            .env("FILESCOPE_COPY_TEST_ROOT", root)
            .spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(status) = child.try_wait().unwrap() { return status.success(); }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                eprintln!("{name}: copy blocked instead of returning an error");
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn failed_copy_keeps_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        if let Some(root) = std::env::var_os("FILESCOPE_COPY_TEST_ROOT") {
            let root = PathBuf::from(root);
            let limit = libc::rlimit { rlim_cur: 1024, rlim_max: 1024 };
            unsafe {
                libc::umask(0o022);
                libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
                assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            }
            let result = copy_without_replace(&root.join("secret"), &root.join("partial"));
            assert!(result.is_err());
            let partial = fs::metadata(root.join("partial")).unwrap();
            assert_eq!(partial.len(), 1024);
            assert_eq!(partial.permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(root.join("secret")).unwrap().len(), 8192);
            return;
        }
        let root = tmp("private-copy-failure");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("secret"), vec![b'x'; 8192]).unwrap();
        fs::set_permissions(root.join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
        let passed = isolated_copy_test("failed_copy_keeps_private_permissions", &root);
        fs::remove_dir_all(root).unwrap();
        assert!(passed, "failed copy exposed private contents");
    }

    #[test]
    fn failed_directory_copy_keeps_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        if let Some(root) = std::env::var_os("FILESCOPE_COPY_TEST_ROOT") {
            let root = PathBuf::from(root);
            unsafe { libc::umask(0o022); }
            assert!(copy_without_replace(&root.join("private"), &root.join("partial")).is_err());
            assert_eq!(fs::metadata(root.join("partial")).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(fs::read(root.join("private/secret")).unwrap(), b"private directory contents");
            return;
        }
        let root = tmp("private-directory-copy-failure");
        fs::create_dir_all(root.join("private")).unwrap();
        fs::set_permissions(root.join("private"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("private/secret"), b"private directory contents").unwrap();
        let socket = std::os::unix::net::UnixListener::bind(root.join("private/socket")).unwrap();
        let passed = isolated_copy_test("failed_directory_copy_keeps_private_permissions", &root);
        drop(socket);
        fs::remove_dir_all(root).unwrap();
        assert!(passed, "failed copy exposed a private directory");
    }

    #[test]
    fn copy_rejects_named_pipes_without_opening_them() {
        use std::os::unix::fs::FileTypeExt;
        if let Some(root) = std::env::var_os("FILESCOPE_COPY_TEST_ROOT") {
            let root = PathBuf::from(root);
            let result = copy_without_replace(&root.join("files/events"), &root.join("destination"));
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Unsupported);
            assert!(fs::symlink_metadata(root.join("files/events")).unwrap().file_type().is_fifo());
            assert!(root.join("info/events.trashinfo").exists());
            assert!(!root.join("destination").exists());
            return;
        }
        let root = tmp("pipe-copy");
        fake_trash(&root, "events", "/unused/events", "", b"");
        fs::remove_file(root.join("files/events")).unwrap();
        let fifo = std::ffi::CString::new(root.join("files/events").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let passed = isolated_copy_test("copy_rejects_named_pipes_without_opening_them", &root);
        fs::remove_dir_all(root).unwrap();
        assert!(passed, "named pipe was opened or converted to a regular file");
    }
}
