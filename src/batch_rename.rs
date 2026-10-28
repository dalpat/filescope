//! A preview is a snapshot of exact files, never a recipe rerun against a folder.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

pub const NAME: &str = "[Original name]";
pub const NUMBER_1: &str = "[1, 2, 3]";
pub const NUMBER_2: &str = "[01, 02, 03]";
pub const NUMBER_3: &str = "[001, 002, 003]";

#[derive(Clone, Debug, PartialEq)]
struct Stamp {
    dev: u64,
    ino: u64,
    mode: u32,
    len: u64,
    modified: (i64, i64),
    created: Option<std::time::SystemTime>,
}

impl Stamp {
    fn read(path: &std::path::Path) -> Result<Self, String> {
        let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        Ok(Self {
            dev: m.dev(),
            ino: m.ino(),
            mode: m.mode(),
            len: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            created: m.created().ok(),
        })
    }

    fn same_file(&self, other: &Self) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && self.created == other.created
            && self.mode & libc::S_IFMT == other.mode & libc::S_IFMT
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    stamp: Stamp,
}

impl Item {
    pub fn read(path: PathBuf) -> Result<Self, String> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("Filename is not valid UTF-8")?
            .to_owned();
        let parent = path
            .parent()
            .ok_or("No parent folder")?
            .canonicalize()
            .map_err(|e| e.to_string())?;
        let path = parent.join(&name);
        let stamp = Stamp::read(&path)?;
        let is_dir = path.is_dir();
        Ok(Self {
            path,
            name,
            is_dir,
            stamp,
        })
    }

    pub fn modified(&self) -> (i64, i64) {
        self.stamp.modified
    }
}

#[derive(Clone)]
pub enum Rule {
    Template(String),
    Replace { find: String, replacement: String },
}

#[derive(Clone)]
pub struct Row {
    pub item: Item,
    pub name: String,
    pub error: Option<String>,
}

#[derive(Clone)]
pub struct Plan {
    pub rows: Vec<Row>,
}

/// A guarded move ready to execute. History checks identity, allowing edits to
/// the same file after a rename; the initial operation also checks its snapshot.
#[derive(Clone, Debug)]
pub struct Rename {
    pub from: PathBuf,
    pub to: PathBuf,
    expected: Stamp,
    snapshot: bool,
}

pub struct Outcome {
    pub undo: Vec<Rename>,
    pub failed: Vec<(Rename, String)>,
}

fn template(pattern: &str, name: &str, number: usize) -> String {
    let mut rest = pattern;
    let mut output = String::new();
    while !rest.is_empty() {
        let token = [NAME, NUMBER_1, NUMBER_2, NUMBER_3]
            .into_iter()
            .find(|t| rest.starts_with(t));
        if let Some(token) = token {
            let value = match token {
                NAME => name.to_owned(),
                NUMBER_2 => format!("{number:02}"),
                NUMBER_3 => format!("{number:03}"),
                _ => number.to_string(),
            };
            output.push_str(&value);
            rest = &rest[token.len()..];
        } else {
            let ch = rest.chars().next().unwrap();
            output.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    output
}

// Keep the archive type with its compression suffix, e.g. .tar.gz.
fn split_extension(name: &str) -> (&str, &str) {
    let (stem, extension) = crate::fileops::split_name(name);
    if !extension.is_empty()
        && stem.len() > 4
        && stem
            .get(stem.len() - 4..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".tar"))
    {
        name.split_at(stem.len() - 4)
    } else {
        (stem, extension)
    }
}

pub fn prepare(items: &[Item], rule: &Rule, keep_extensions: bool) -> Plan {
    let mut rows: Vec<_> = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let (stem, ext) = if keep_extensions && !item.is_dir {
                split_extension(&item.name)
            } else {
                (item.name.as_str(), "")
            };
            let (base, mut error) = match rule {
                Rule::Template(pattern) => (template(pattern, stem, i + 1), None),
                Rule::Replace { find, replacement } => (
                    stem.replace(find, replacement),
                    find.is_empty().then(|| "Enter text to find".into()),
                ),
            };
            let name = format!("{base}{ext}");
            if base.trim().is_empty()
                || name == "."
                || name == ".."
                || name.contains(['/', '\0'])
                || name.len() > 255
            {
                error = Some("Invalid filename".into());
            }
            let request = Rename {
                from: item.path.clone(),
                to: item.path.with_file_name(&name),
                expected: item.stamp.clone(),
                snapshot: true,
            };
            if error.is_none() {
                error = request.check().err();
            }
            Row {
                item: item.clone(),
                name,
                error,
            }
        })
        .collect();
    let mut targets = HashMap::new();
    for row in &rows {
        *targets
            .entry(row.item.path.with_file_name(&row.name))
            .or_insert(0) += 1;
    }
    let folders: HashSet<_> = items
        .iter()
        .filter(|item| item.is_dir)
        .map(|item| item.path.as_path())
        .collect();
    for row in &mut rows {
        if targets[&row.item.path.with_file_name(&row.name)] > 1 {
            row.error = Some("Several items have this destination".into());
        }
        if row
            .item
            .path
            .ancestors()
            .skip(1)
            .any(|parent| folders.contains(parent))
        {
            row.error = Some("Select either a folder or its contents".into());
        }
    }
    Plan { rows }
}

impl Plan {
    pub fn ready(&self) -> bool {
        self.rows.iter().all(|row| row.error.is_none())
            && self.rows.iter().any(|row| row.name != row.item.name)
    }

    pub fn requests(&self) -> Vec<Rename> {
        self.rows
            .iter()
            .filter(|row| row.name != row.item.name)
            .map(|row| Rename {
                from: row.item.path.clone(),
                to: row.item.path.with_file_name(&row.name),
                expected: row.item.stamp.clone(),
                snapshot: true,
            })
            .collect()
    }

    pub fn apply(&self) -> Result<Outcome, String> {
        if !self.ready() {
            return Err("Resolve the preview errors before renaming".into());
        }
        // Recheck even unchanged rows: the selected snapshot must still be valid.
        for row in &self.rows {
            Rename {
                from: row.item.path.clone(),
                to: row.item.path.with_file_name(&row.name),
                expected: row.item.stamp.clone(),
                snapshot: true,
            }
            .check()
            .map_err(|e| format!("{}: {e}", row.item.path.display()))?;
        }
        Ok(run(&self.requests()))
    }
}

impl Rename {
    /// The directory entry captured by the plan, including a symlink's own inode.
    pub(crate) fn entry_identity(&self) -> (u64, u64) { (self.expected.dev, self.expected.ino) }

    fn check(&self) -> Result<(), String> {
        let now = Stamp::read(&self.from)?;
        if !self.expected.same_file(&now) || (self.snapshot && now != self.expected) {
            return Err("Source changed since it was selected; select it again".into());
        }
        if self.from != self.to {
            match fs::symlink_metadata(&self.to) {
                Ok(_) => return Err("Destination already exists".into()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(())
    }
}

pub fn run(requests: &[Rename]) -> Outcome {
    let mut outcome = Outcome {
        undo: Vec::new(),
        failed: Vec::new(),
    };
    for request in requests {
        if request.from == request.to {
            continue;
        }
        let result = request.check().and_then(|()| {
            crate::fileops::rename_no_replace(&request.from, &request.to).map_err(|e| e.to_string())
        });
        match result {
            Ok(()) => outcome.undo.push(Rename {
                from: request.to.clone(),
                to: request.from.clone(),
                expected: request.expected.clone(),
                snapshot: false,
            }),
            Err(e) => outcome.failed.push((request.clone(), e)),
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(tag: &str, names: &[&str]) -> (std::path::PathBuf, Vec<Item>) {
        let root =
            std::env::temp_dir().join(format!("filescope-batch-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        for name in names {
            fs::write(root.join(name), name.as_bytes()).unwrap();
        }
        let items = names
            .iter()
            .map(|name| Item::read(root.join(name)).unwrap())
            .collect();
        (root, items)
    }

    #[test]
    fn batch_numbering_is_stable_and_preserves_extensions() {
        let (root, items) = fixture("numbering", &["b.jpg", "a.JPG", "c.png"]);
        let rule = Rule::Template(format!("Holiday {NUMBER_3}"));
        let plan = prepare(&items, &rule, true);
        assert!(plan.ready());
        assert_eq!(
            plan.rows
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            ["Holiday 001.jpg", "Holiday 002.JPG", "Holiday 003.png"]
        );
        let result = plan.apply().unwrap();
        assert_eq!(result.undo.len(), 3);
        assert!(result.failed.is_empty());
        assert_eq!(fs::read(root.join("Holiday 001.jpg")).unwrap(), b"b.jpg");
        assert_eq!(fs::read(root.join("Holiday 002.JPG")).unwrap(), b"a.JPG");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn batch_rename_rejects_invalid_or_colliding_names() {
        let (root, items) = fixture("invalid", &["a.txt", "b.txt"]);
        for pattern in ["", ".", "..", "bad/name", "bad\0name"] {
            assert!(
                !prepare(&items, &Rule::Template(pattern.into()), false).ready(),
                "accepted {pattern:?}"
            );
        }
        let duplicates = prepare(&items, &Rule::Template("same".into()), true);
        assert!(duplicates.rows.iter().all(|row| row.error.is_some()));
        fs::write(root.join("old_a.txt"), b"keep").unwrap();
        let collision = prepare(&items, &Rule::Template(format!("old_{NAME}")), true);
        assert!(!collision.ready());
        assert!(collision.apply().is_err());
        assert_eq!(fs::read(root.join("old_a.txt")).unwrap(), b"keep");
        assert!(root.join("b.txt").exists());
        std::os::unix::fs::symlink("missing", root.join("link_a.txt")).unwrap();
        assert!(!prepare(&items, &Rule::Template(format!("link_{NAME}")), true).ready());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_batch_input_is_not_overwritten() {
        let (root, items) = fixture("stale-target", &["a.txt", "b.txt"]);
        let plan = prepare(&items, &Rule::Template(format!("new_{NAME}")), true);
        assert!(plan.ready());
        fs::write(root.join("new_b.txt"), b"new arrival").unwrap();
        assert!(plan.apply().is_err());
        assert!(
            root.join("a.txt").exists(),
            "preflight must check the whole set before renaming"
        );
        assert!(!root.join("new_a.txt").exists());
        assert_eq!(fs::read(root.join("new_b.txt")).unwrap(), b"new arrival");
        fs::remove_file(root.join("new_b.txt")).unwrap();
        fs::rename(root.join("a.txt"), root.join("saved.txt")).unwrap();
        fs::write(root.join("a.txt"), b"replacement").unwrap();
        assert!(plan.apply().is_err());
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"replacement");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn batch_undo_tracks_successful_renames_only() {
        let (root, items) = fixture("partial", &["a.txt", "b.txt", "c.txt"]);
        let plan = prepare(&items, &Rule::Template(format!("new_{NAME}")), true);
        let requests = plan.requests();
        // Simulate a disappearance after the whole-batch preflight.
        fs::remove_file(root.join("b.txt")).unwrap();
        let result = run(&requests);
        assert_eq!(result.undo.len(), 2);
        assert_eq!(result.failed.len(), 1);
        assert!(!root.join("new_b.txt").exists());
        fs::write(root.join("a.txt"), b"created after rename").unwrap();
        let undone = run(&result.undo);
        assert_eq!(undone.undo.len(), 1);
        assert_eq!(undone.failed.len(), 1);
        assert_eq!(
            fs::read(root.join("a.txt")).unwrap(),
            b"created after rename"
        );
        assert_eq!(fs::read(root.join("new_a.txt")).unwrap(), b"a.txt");
        assert_eq!(fs::read(root.join("c.txt")).unwrap(), b"c.txt");
        fs::remove_file(root.join("a.txt")).unwrap();
        let retry: Vec<_> = undone
            .failed
            .iter()
            .map(|(request, _)| request.clone())
            .collect();
        assert_eq!(run(&retry).undo.len(), 1);
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"a.txt");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn batch_find_replace_and_templates_preserve_literal_unicode_names() {
        let (root, items) = fixture(
            "literal",
            &["IMG_café.jpg", "IMG_日本語.png", "[001, 002, 003].txt"],
        );
        let plan = prepare(
            &items,
            &Rule::Replace {
                find: "IMG_".into(),
                replacement: "Trip_".into(),
            },
            true,
        );
        assert_eq!(plan.rows[0].name, "Trip_café.jpg");
        assert_eq!(plan.rows[1].name, "Trip_日本語.png");
        assert_eq!(plan.rows[2].name, "[001, 002, 003].txt");
        let plan = prepare(&items, &Rule::Template(format!("pre_{NAME}_suffix")), true);
        assert_eq!(
            plan.rows[2].name, "pre_[001, 002, 003]_suffix.txt",
            "inserted names must not be parsed as template tokens"
        );
        let extension = prepare(
            &items,
            &Rule::Replace {
                find: ".jpg".into(),
                replacement: ".png".into(),
            },
            true,
        );
        assert_eq!(extension.rows[0].name, "IMG_café.jpg");
        let extension = prepare(
            &items,
            &Rule::Replace {
                find: ".jpg".into(),
                replacement: ".png".into(),
            },
            false,
        );
        assert_eq!(extension.rows[0].name, "IMG_café.png");
        fs::create_dir(root.join("folder.with.dots")).unwrap();
        let folder = Item::read(root.join("folder.with.dots")).unwrap();
        assert_eq!(
            prepare(&[folder], &Rule::Template(format!("{NAME}_suffix")), true).rows[0].name,
            "folder.with.dots_suffix"
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod edge_tests {
    use super::*;

    #[test]
    fn history_allows_edits_but_rejects_a_replacement_file() {
        let root =
            std::env::temp_dir().join(format!("filescope-batch-history-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), b"original").unwrap();
        let items = vec![Item::read(root.join("a.txt")).unwrap()];
        let renamed = prepare(&items, &Rule::Template(format!("new_{NAME}")), true)
            .apply()
            .unwrap();
        fs::write(root.join("new_a.txt"), b"edited after rename").unwrap();
        let undone = run(&renamed.undo);
        assert_eq!(undone.undo.len(), 1);
        assert_eq!(
            fs::read(root.join("a.txt")).unwrap(),
            b"edited after rename"
        );
        fs::rename(root.join("a.txt"), root.join("saved.txt")).unwrap();
        fs::write(root.join("a.txt"), b"replacement").unwrap();
        assert_eq!(run(&undone.undo).failed.len(), 1);
        assert!(!root.join("new_a.txt").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn large_batch_is_exact_and_groups_redo() {
        let root =
            std::env::temp_dir().join(format!("filescope-batch-large-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let items: Vec<_> = (0..1000)
            .map(|n| {
                let path = root.join(format!("item-{n}.txt"));
                fs::write(&path, n.to_string()).unwrap();
                Item::read(path).unwrap()
            })
            .collect();
        fs::write(root.join("unselected.txt"), b"untouched").unwrap();
        let renamed = prepare(&items, &Rule::Template(format!("New {NUMBER_3}")), true)
            .apply()
            .unwrap();
        assert_eq!(renamed.undo.len(), 1000);
        assert!(renamed.failed.is_empty());
        assert_eq!(
            fs::read_to_string(root.join("New 1000.txt")).unwrap(),
            "999"
        );
        let undone = run(&renamed.undo);
        assert_eq!(undone.undo.len(), 1000);
        let redone = run(&undone.undo);
        assert_eq!(redone.undo.len(), 1000);
        assert_eq!(fs::read(root.join("unselected.txt")).unwrap(), b"untouched");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn selected_folders_cannot_hide_selected_children_or_destinations() {
        let root =
            std::env::temp_dir().join(format!("filescope-batch-nested-{}", std::process::id()));
        fs::create_dir_all(root.join("folder")).unwrap();
        fs::write(root.join("folder/a.txt"), b"child").unwrap();
        let items = [
            Item::read(root.join("folder")).unwrap(),
            Item::read(root.join("folder/a.txt")).unwrap(),
        ];
        assert!(!prepare(&items, &Rule::Template(format!("new_{NAME}")), true).ready());
        let too_long = Rule::Template("x".repeat(256));
        assert!(!prepare(&items[..1], &too_long, false).ready());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod symlink_tests {
    use super::*;

    #[test]
    fn rename_moves_symlinks_without_modifying_their_targets() {
        let root =
            std::env::temp_dir().join(format!("filescope-batch-links-{}", std::process::id()));
        fs::create_dir_all(root.join("target.folder")).unwrap();
        fs::write(root.join("target.folder/keep.txt"), b"keep").unwrap();
        std::os::unix::fs::symlink("target.folder", root.join("shortcut.folder")).unwrap();
        std::os::unix::fs::symlink("missing", root.join("missing.link")).unwrap();
        let items = [
            Item::read(root.join("shortcut.folder")).unwrap(),
            Item::read(root.join("missing.link")).unwrap(),
        ];
        let plan = prepare(&items, &Rule::Template(format!("{NAME}_new")), true);
        assert_eq!(plan.rows[0].name, "shortcut.folder_new");
        assert_eq!(plan.rows[1].name, "missing_new.link");
        let outcome = plan.apply().unwrap();
        assert_eq!(outcome.undo.len(), 2);
        assert!(outcome.failed.is_empty());
        assert_eq!(
            fs::read_link(root.join("shortcut.folder_new")).unwrap(),
            PathBuf::from("target.folder")
        );
        assert_eq!(
            fs::read(root.join("target.folder/keep.txt")).unwrap(),
            b"keep"
        );
        assert_eq!(run(&outcome.undo).undo.len(), 2);
        assert!(
            fs::symlink_metadata(root.join("missing.link"))
                .unwrap()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod archive_extension_tests {
    use super::*;

    #[test]
    fn batch_preserves_compound_archive_extensions_in_both_modes() {
        let root =
            std::env::temp_dir().join(format!("filescope-batch-archives-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let items: Vec<_> = ["one.tar.gz", "two.tar.xz", "three.TAR.BZ2", "four.jpg"]
            .iter()
            .map(|name| {
                let path = root.join(name);
                fs::write(&path, name).unwrap();
                Item::read(path).unwrap()
            })
            .collect();
        let plan = prepare(&items, &Rule::Template(format!("Backup {NUMBER_3}")), true);
        assert!(plan.ready());
        assert_eq!(
            plan.rows
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            [
                "Backup 001.tar.gz",
                "Backup 002.tar.xz",
                "Backup 003.TAR.BZ2",
                "Backup 004.jpg"
            ]
        );
        let suffix = prepare(&items, &Rule::Template(format!("{NAME}_copy")), true);
        assert_eq!(suffix.rows[0].name, "one_copy.tar.gz");
        let rule = Rule::Replace {
            find: ".tar".into(),
            replacement: ".zip".into(),
        };
        assert_eq!(prepare(&items, &rule, true).rows[0].name, "one.tar.gz");
        assert_eq!(prepare(&items, &rule, false).rows[0].name, "one.zip.gz");
        let result = plan.apply().unwrap();
        assert!(result.failed.is_empty());
        assert_eq!(
            fs::read(root.join("Backup 001.tar.gz")).unwrap(),
            b"one.tar.gz"
        );
        assert_eq!(run(&result.undo).undo.len(), 4);
        fs::remove_dir_all(root).unwrap();
    }
}
