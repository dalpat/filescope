// Archive support: recognising archives, naming what they extract into, and
// building the commands that do the work.
//
// Extraction/compression shells out to the standard tools (tar, unzip, zip, 7z,
// unrar) rather than linking an archive library, so filescope handles whatever
// the system already handles. Everything here is pure — the actual spawning
// lives in the UI layer.

use std::ffi::OsString;
use std::path::Path;

/// An archive format we can extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Zip,
    /// `.tar` and every compressed tar — `tar -xf` detects the compression.
    Tar,
    SevenZip,
    Rar,
}

/// A format we can create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compress {
    Zip,
    TarGz,
    TarXz,
}

/// Extensions that mark an archive, longest first so `.tar.gz` wins over `.gz`.
const EXTENSIONS: &[(&str, Format)] = &[
    (".tar.gz", Format::Tar),
    (".tar.bz2", Format::Tar),
    (".tar.xz", Format::Tar),
    (".tar.zst", Format::Tar),
    (".tgz", Format::Tar),
    (".tbz2", Format::Tar),
    (".tbz", Format::Tar),
    (".txz", Format::Tar),
    (".tar", Format::Tar),
    (".zip", Format::Zip),
    (".7z", Format::SevenZip),
    (".rar", Format::Rar),
];

/// The archive extension `name` ends with, and its format.
fn matching_extension(name: &str) -> Option<(&'static str, Format)> {
    let lower = name.to_lowercase();
    EXTENSIONS
        .iter()
        .find(|(ext, _)| lower.len() > ext.len() && lower.ends_with(ext))
        .map(|(ext, format)| (*ext, *format))
}

impl Format {
    /// The archive format `name` denotes, if any. Case-insensitive.
    pub fn from_name(name: &str) -> Option<Format> {
        matching_extension(name).map(|(_, format)| format)
    }

    /// The binary needed to extract this format.
    pub fn tool(self) -> &'static str {
        match self {
            Format::Zip => "unzip",
            Format::Tar => "tar",
            Format::SevenZip => "7z",
            Format::Rar => "unrar",
        }
    }

    /// The command that extracts `archive` into the existing directory `dest`.
    pub fn extract_argv(self, archive: &Path, dest: &Path) -> Vec<OsString> {
        let tool: OsString = self.tool().into();
        match self {
            // `tar -xf` detects gz/bz2/xz/zst itself.
            Format::Tar => vec![tool, "-xf".into(), archive.into(), "-C".into(), dest.into()],
            Format::Zip => vec![tool, "-q".into(), archive.into(), "-d".into(), dest.into()],
            // 7z wants the destination glued to -o, with no space.
            Format::SevenZip => {
                let mut out = OsString::from("-o");
                out.push(dest);
                vec![tool, "x".into(), "-y".into(), out, archive.into()]
            }
            // unrar needs a trailing separator on the destination.
            Format::Rar => {
                let mut out = OsString::from(dest);
                out.push("/");
                vec![tool, "x".into(), "-y".into(), archive.into(), out]
            }
        }
    }
}

impl Compress {
    pub fn extension(self) -> &'static str {
        match self {
            Compress::Zip => ".zip",
            Compress::TarGz => ".tar.gz",
            Compress::TarXz => ".tar.xz",
        }
    }

    pub fn tool(self) -> &'static str {
        match self {
            Compress::Zip => "zip",
            Compress::TarGz | Compress::TarXz => "tar",
        }
    }

    /// The command that packs `items` (names relative to the working directory)
    /// into `output`.
    pub fn argv(self, output: &Path, items: &[String]) -> Vec<OsString> {
        let tool: OsString = self.tool().into();
        let mut argv = match self {
            Compress::Zip => vec![tool, "-r".into(), "-q".into(), output.into()],
            Compress::TarGz => vec![tool, "-czf".into(), output.into()],
            Compress::TarXz => vec![tool, "-cJf".into(), output.into()],
        };
        argv.extend(items.iter().map(OsString::from));
        argv
    }
}

/// `name` with its archive extension removed — what the archive extracts into.
/// A name that isn't an archive comes back unchanged.
pub fn base_name(name: &str) -> String {
    match matching_extension(name) {
        Some((ext, _)) => name[..name.len() - ext.len()].to_string(),
        None => name.to_string(),
    }
}

/// The default name to suggest when compressing `items` (no extension): the lone
/// item's stem, or "Archive" for a multi-item selection.
pub fn default_archive_name(items: &[String]) -> String {
    match items {
        [only] => crate::fileops::split_name(only).0.to_string(),
        _ => "Archive".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn argv(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn recognises_archive_formats() {
        assert_eq!(Format::from_name("photos.zip"), Some(Format::Zip));
        assert_eq!(Format::from_name("src.tar"), Some(Format::Tar));
        assert_eq!(Format::from_name("src.tar.gz"), Some(Format::Tar));
        assert_eq!(Format::from_name("src.tar.bz2"), Some(Format::Tar));
        assert_eq!(Format::from_name("src.tar.xz"), Some(Format::Tar));
        assert_eq!(Format::from_name("src.tgz"), Some(Format::Tar));
        assert_eq!(Format::from_name("src.txz"), Some(Format::Tar));
        assert_eq!(Format::from_name("blob.7z"), Some(Format::SevenZip));
        assert_eq!(Format::from_name("old.rar"), Some(Format::Rar));
    }

    #[test]
    fn is_case_insensitive_and_rejects_non_archives() {
        assert_eq!(Format::from_name("PHOTOS.ZIP"), Some(Format::Zip));
        assert_eq!(Format::from_name("Src.Tar.Gz"), Some(Format::Tar));
        assert_eq!(Format::from_name("notes.txt"), None);
        assert_eq!(Format::from_name("README"), None);
        // A bare .gz isn't a tar; we don't claim to extract it.
        assert_eq!(Format::from_name("data.gz"), None);
        // The extension must be an extension, not just a substring.
        assert_eq!(Format::from_name("ziphead"), None);
    }

    #[test]
    fn base_name_strips_the_whole_archive_extension() {
        // The compound extension must go entirely — not leave "photos.tar".
        assert_eq!(base_name("photos.tar.gz"), "photos");
        assert_eq!(base_name("photos.tgz"), "photos");
        assert_eq!(base_name("photos.zip"), "photos");
        assert_eq!(base_name("blob.7z"), "blob");
        // Dots inside the name survive.
        assert_eq!(base_name("my.backup.2026.tar.xz"), "my.backup.2026");
        // Not an archive → unchanged.
        assert_eq!(base_name("notes.txt"), "notes.txt");
    }

    #[test]
    fn suggests_an_archive_name_from_the_selection() {
        assert_eq!(default_archive_name(&["report.pdf".into()]), "report");
        assert_eq!(default_archive_name(&["Projects".into()]), "Projects");
        assert_eq!(default_archive_name(&["a.txt".into(), "b.txt".into()]), "Archive");
        assert_eq!(default_archive_name(&[]), "Archive");
    }

    #[test]
    fn builds_extract_commands() {
        let a = PathBuf::from("/tmp/x.tar.gz");
        let d = PathBuf::from("/tmp/out");
        assert_eq!(argv(Format::Tar.extract_argv(&a, &d)), ["tar", "-xf", "/tmp/x.tar.gz", "-C", "/tmp/out"]);
        assert_eq!(
            argv(Format::Zip.extract_argv(Path::new("/tmp/x.zip"), &d)),
            ["unzip", "-q", "/tmp/x.zip", "-d", "/tmp/out"]
        );
        // 7z takes its destination glued to -o.
        assert_eq!(
            argv(Format::SevenZip.extract_argv(Path::new("/tmp/x.7z"), &d)),
            ["7z", "x", "-y", "-o/tmp/out", "/tmp/x.7z"]
        );
        assert_eq!(
            argv(Format::Rar.extract_argv(Path::new("/tmp/x.rar"), &d)),
            ["unrar", "x", "-y", "/tmp/x.rar", "/tmp/out/"]
        );
    }

    #[test]
    fn extract_tools_match_their_formats() {
        assert_eq!(Format::Tar.tool(), "tar");
        assert_eq!(Format::Zip.tool(), "unzip");
        assert_eq!(Format::SevenZip.tool(), "7z");
        assert_eq!(Format::Rar.tool(), "unrar");
    }

    #[test]
    fn builds_compress_commands() {
        let items = vec!["a.txt".to_string(), "Docs".to_string()];
        assert_eq!(
            argv(Compress::Zip.argv(Path::new("out.zip"), &items)),
            ["zip", "-r", "-q", "out.zip", "a.txt", "Docs"]
        );
        assert_eq!(
            argv(Compress::TarGz.argv(Path::new("out.tar.gz"), &items)),
            ["tar", "-czf", "out.tar.gz", "a.txt", "Docs"]
        );
        assert_eq!(
            argv(Compress::TarXz.argv(Path::new("out.tar.xz"), &items)),
            ["tar", "-cJf", "out.tar.xz", "a.txt", "Docs"]
        );
    }

    #[test]
    fn compress_extensions_and_tools() {
        assert_eq!(Compress::Zip.extension(), ".zip");
        assert_eq!(Compress::TarGz.extension(), ".tar.gz");
        assert_eq!(Compress::TarXz.extension(), ".tar.xz");
        assert_eq!(Compress::Zip.tool(), "zip");
        assert_eq!(Compress::TarGz.tool(), "tar");
    }
}
