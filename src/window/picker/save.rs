//! Save feedback never changes an exact name without an explicit user action.
use super::{FilterSpec, valid_name};
use glib::translate::{ToGlibPtr, from_glib_full};
use gtk::{gio, glib};
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub(super) struct Feedback {
    pub message: String,
    pub suggestion: Option<OsString>,
    pub invalid: bool,
}

fn suffixes(filter: &FilterSpec) -> Option<Vec<&str>> {
    let values = filter
        .1
        .iter()
        .map(|(kind, pattern)| {
            if *kind != 0 {
                return None;
            }
            let suffix = pattern.strip_prefix('*')?;
            if !suffix.starts_with('.')
                || suffix.len() == 1
                || suffix.contains(['*', '?', '[', ']', '/', '\\'])
            {
                return None;
            }
            Some(suffix)
        })
        .collect::<Option<Vec<_>>>()?;
    (!values.is_empty()).then_some(values)
}

pub(super) fn name_feedback(name: &OsStr, filter: Option<&FilterSpec>) -> Feedback {
    let mut feedback = Feedback {
        message: String::new(),
        suggestion: None,
        invalid: !valid_name(name),
    };
    if feedback.invalid {
        feedback.message = "Enter a filename without /, NUL, or a parent-directory path".into();
        return feedback;
    }
    let Some(filter) = filter else {
        return feedback;
    };
    let Some(suffixes) = suffixes(filter) else {
        return feedback;
    };
    let text = name.to_string_lossy();
    if suffixes
        .iter()
        .any(|suffix| text.to_lowercase().ends_with(&suffix.to_lowercase()))
    {
        return feedback;
    }
    if Path::new(name).extension().is_none()
        && !text.starts_with('.')
        && suffixes
            .iter()
            .all(|suffix| suffix.eq_ignore_ascii_case(suffixes[0]))
    {
        let mut suggested = name.to_os_string();
        suggested.push(suffixes[0]);
        feedback.message = format!(
            "Suggested name for {}: {}",
            filter.0,
            suggested.to_string_lossy()
        );
        feedback.suggestion = Some(suggested);
    } else {
        feedback.message = format!(
            "Filename does not match {}. It will be saved as entered.",
            filter.0
        );
    }
    feedback
}

pub(super) fn known_name_info(name: &OsStr) -> gio::FileInfo {
    let info = gio::FileInfo::new();
    info.set_name(name);
    info.set_display_name(&name.to_string_lossy());
    let mut uncertain = 0;
    // A null data pointer asks GIO about the name, without guessing an empty file.
    let kind: glib::GString = unsafe {
        from_glib_full(gio::ffi::g_content_type_guess(
            Path::new(name).to_glib_none().0,
            std::ptr::null(),
            0,
            &mut uncertain,
        ))
    };
    if !gio::content_type_is_unknown(&kind) {
        info.set_content_type(&kind);
    }
    info
}

#[derive(PartialEq, Eq)]
struct Stamp(u64, u64, u32, u64, i64, i64, i64, i64);

fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    Stamp(
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

#[derive(PartialEq, Eq)]
struct Target {
    path: PathBuf,
    link: Option<Stamp>,
    followed: Option<Stamp>,
}

fn metadata(path: &Path, follow: bool) -> Result<Option<std::fs::Metadata>, String> {
    let result = if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    match result {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not inspect {}: {error}", path.display())),
    }
}

#[derive(PartialEq, Eq)]
pub(super) struct Snapshot(Vec<Target>);

impl Snapshot {
    pub fn read(paths: &[PathBuf]) -> Result<Self, String> {
        paths
            .iter()
            .map(|path| {
                let link = metadata(path, false)?;
                let followed = if link.as_ref().is_some_and(|metadata| metadata.is_symlink()) {
                    metadata(path, true)?
                } else {
                    None
                };
                Ok(Target {
                    path: path.clone(),
                    link: link.as_ref().map(stamp),
                    followed: followed.as_ref().map(stamp),
                })
            })
            .collect::<Result<Vec<_>, String>>()
            .map(Self)
    }

    pub fn has_existing(&self) -> bool {
        self.0.iter().any(|target| target.link.is_some())
    }

    pub fn existing_names(&self) -> Vec<String> {
        self.0
            .iter()
            .filter(|target| target.link.is_some())
            .filter_map(|target| target.path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect()
    }

    pub fn unchanged(&self) -> bool {
        let paths = self
            .0
            .iter()
            .map(|target| target.path.clone())
            .collect::<Vec<_>>();
        Self::read(&paths).is_ok_and(|current| current == *self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    #[test]
    fn save_suggestions_require_one_explicit_suffix() {
        let text = ("Text".into(), vec![(0, "*.txt".into())]);
        assert_eq!(
            name_feedback(OsStr::new("report"), Some(&text)).suggestion,
            Some("report.txt".into())
        );
        for name in ["report.txt", "report.other", ".env", "name.txt.txt"] {
            assert!(
                name_feedback(OsStr::new(name), Some(&text))
                    .suggestion
                    .is_none()
            );
        }
        for filter in [
            (
                "Ambiguous".into(),
                vec![(0, "*.txt".into()), (0, "*.md".into())],
            ),
            ("MIME".into(), vec![(1, "text/plain".into())]),
            ("Any".into(), vec![(0, "*".into())]),
        ] {
            assert!(
                name_feedback(OsStr::new("report"), Some(&filter))
                    .suggestion
                    .is_none()
            );
        }
        let compound = ("Archive".into(), vec![(0, "*.tar.gz".into())]);
        assert_eq!(
            name_feedback(OsStr::new("archive"), Some(&compound)).suggestion,
            Some("archive.tar.gz".into())
        );
        assert!(
            name_feedback(OsStr::new("archive.tar.gz"), Some(&compound))
                .suggestion
                .is_none()
        );
        let raw = OsString::from_vec(b"raw-\xff".to_vec());
        assert_eq!(
            name_feedback(&raw, Some(&text))
                .suggestion
                .unwrap()
                .as_bytes(),
            b"raw-\xff.txt"
        );
    }

    #[test]
    fn save_snapshot_rechecks_symlink_target() {
        let root = std::env::temp_dir().join(format!(
            "filescope-save-snapshot-{}",
            gtk::glib::uuid_string_random()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("target"), "before").unwrap();
        std::os::unix::fs::symlink("target", root.join("link")).unwrap();
        let snapshot = Snapshot::read(&[root.join("link")]).unwrap();
        assert!(snapshot.unchanged());
        std::fs::write(root.join("target"), "changed target").unwrap();
        assert!(!snapshot.unchanged());
        std::fs::remove_dir_all(root).unwrap();
    }
}
