use std::fs::{self, OpenOptions, Permissions};
use std::io;
use std::os::unix::{
    fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use gtk::gdk_pixbuf::Pixbuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Source {
    modified: SystemTime,
    size: u64,
}

impl Source {
    pub(super) fn read(path: &Path) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(path)?;
        Ok(Self {
            modified: metadata.modified()?,
            size: metadata.len(),
        })
    }
}

#[derive(Clone)]
pub(super) struct Work {
    pub uri: String,
    pub input: PathBuf,
    pub output: PathBuf,
    pub exec: String,
}

pub(super) struct Outcome {
    pub source: Option<Source>,
    pub published: bool,
}

pub(super) fn generate(work: Work, failed: Option<Source>, timeout: Duration) -> Outcome {
    let source = Source::read(&work.input).ok();
    let published = source.as_ref().is_some_and(|source| {
        failed.as_ref() != Some(source) && publish(&work, source, timeout).is_ok()
    });
    Outcome { source, published }
}

struct Temporary(PathBuf);
impl Temporary {
    fn new(directory: &Path) -> io::Result<Self> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
        fs::set_permissions(directory, Permissions::from_mode(0o700))?;
        loop {
            let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
            let path = directory.join(format!(".filescope-{}-{serial}.png", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(_) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn terminate(child: &mut Child) {
    // Helpers such as image decoders can have children of their own.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn wait_child(child: &mut Child, expired: impl Fn() -> bool) -> io::Result<()> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other("thumbnailer failed"))
                };
            }
            Ok(None) if !expired() => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                terminate(child);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "thumbnailer timed out",
                ));
            }
            Err(error) => {
                terminate(child);
                return Err(error);
            }
        }
    }
}

fn publish(work: &Work, source: &Source, timeout: Duration) -> io::Result<()> {
    if super::skips_large(
        &super::Thumbnailer {
            exec: work.exec.clone(),
        },
        source.size,
    ) {
        return Err(io::Error::other("image exceeds thumbnail size limit"));
    }
    let parent = work
        .output
        .parent()
        .ok_or_else(|| io::Error::other("missing cache directory"))?;
    let temporary = Temporary::new(parent)?;
    let argv = super::thumbnail_command(
        &work.exec,
        &work.uri,
        &work.input,
        &temporary.0,
        super::THUMB_SIZE,
    );
    let program = argv
        .first()
        .ok_or_else(|| io::Error::other("missing thumbnailer"))?;
    let mut child = Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()?;
    let started = Instant::now();
    wait_child(&mut child, || started.elapsed() >= timeout)?;
    validate_output(&temporary.0, work, source)?;
    fs::set_permissions(&temporary.0, Permissions::from_mode(0o600))?;
    if Source::read(&work.input)? != *source {
        return Err(io::Error::other("source changed"));
    }
    fs::rename(&temporary.0, &work.output)
}

fn validate_output(path: &Path, work: &Work, source: &Source) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
        return Err(io::Error::other("invalid thumbnail output"));
    }
    let (format, width, height) =
        Pixbuf::file_info(path).ok_or_else(|| io::Error::other("invalid image"))?;
    if format.name().as_deref() != Some("png")
        || !(1..=super::THUMB_SIZE).contains(&width)
        || !(1..=super::THUMB_SIZE).contains(&height)
    {
        return Err(io::Error::other("invalid thumbnail format or size"));
    }
    let image = Pixbuf::from_file(path).map_err(io::Error::other)?;
    let mtime = source
        .modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs();
    if image
        .option("tEXt::Thumb::URI")
        .is_some_and(|uri| uri != work.uri)
        || image
            .option("tEXt::Thumb::MTime")
            .is_some_and(|value| value.parse::<u64>().ok() != Some(mtime))
        || image
            .option("tEXt::Thumb::Size")
            .is_some_and(|value| value.parse::<u64>().ok() != Some(source.size))
    {
        return Err(io::Error::other("conflicting thumbnail metadata"));
    }
    let mtime = mtime.to_string();
    let size = source.size.to_string();
    image
        .savev(
            path,
            "png",
            &[
                ("tEXt::Thumb::URI", &work.uri),
                ("tEXt::Thumb::MTime", &mtime),
                ("tEXt::Thumb::Size", &size),
            ],
        )
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
    use gtk::prelude::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("filescope-thumbnail-{}-{name}", std::process::id()));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("source"), "source data").unwrap();
            Self(dir)
        }
        fn png(&self, options: &[(&str, &str)]) {
            let image = Pixbuf::new(Colorspace::Rgb, true, 8, 16, 16).unwrap();
            image.fill(0x4488ccff);
            image
                .savev(self.0.join("fixture.png"), "png", options)
                .unwrap();
        }
        fn work(&self, script: &str) -> Work {
            fs::write(self.0.join("helper.sh"), script).unwrap();
            let input = self.0.join("source");
            let uri = gtk::gio::File::for_path(&input).uri().to_string();
            Work {
                input,
                output: self.0.join("cache/thumbnails/large/result.png"),
                uri,
                exec: format!("sh {}/helper.sh %i %o", self.0.display()),
            }
        }
        fn generate(&self, work: Work) -> Outcome {
            generate(work, None, Duration::from_secs(2))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn thumbnail_output_requires_matching_metadata() {
        let f = Fixture::new("validation");
        f.png(&[]);
        let work = f.work("cp \"$(dirname \"$0\")/fixture.png\" \"$2\"\n");
        assert!(f.generate(work.clone()).published);
        let png = Pixbuf::from_file(&work.output).unwrap();
        assert_eq!(
            png.option("tEXt::Thumb::URI").as_deref(),
            Some(work.uri.as_str())
        );
        let source = Source::read(&work.input).unwrap();
        assert_eq!(
            png.option("tEXt::Thumb::MTime")
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            source
                .modified
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        );
        assert_eq!(
            fs::metadata(&work.output).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(work.output.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let valid = fs::read(&work.output).unwrap();
        for (key, value) in [
            ("tEXt::Thumb::URI", "file:///wrong"),
            ("tEXt::Thumb::MTime", "0"),
            ("tEXt::Thumb::Size", "99999"),
        ] {
            f.png(&[(key, value)]);
            assert!(
                !f.generate(work.clone()).published,
                "accepted conflicting {key}"
            );
            assert_eq!(
                fs::read(&work.output).unwrap(),
                valid,
                "replaced valid cache on failure"
            );
        }
        fs::write(f.0.join("fixture.png"), "not a PNG").unwrap();
        assert!(!f.generate(work.clone()).published);
        fs::write(f.0.join("fixture.png"), &valid[..valid.len() / 2]).unwrap();
        assert!(!f.generate(work.clone()).published, "accepted a truncated PNG");
        fs::write(f.0.join("fixture.png"), &valid).unwrap();
        assert!(f.generate(work.clone()).published, "rejected matching metadata");
        assert_eq!(
            fs::read_dir(work.output.parent().unwrap()).unwrap().count(),
            1,
            "temporary output leaked"
        );
    }

    #[test]
    fn source_change_during_generation_is_rejected() {
        let f = Fixture::new("changed");
        f.png(&[]);
        let work =
            f.work("cp \"$(dirname \"$0\")/fixture.png\" \"$2\"\nprintf changed >> \"$1\"\n");
        assert!(!f.generate(work.clone()).published);
        assert!(!work.output.exists());
        assert_eq!(
            fs::read_dir(work.output.parent().unwrap()).unwrap().count(),
            0
        );
    }

    #[test]
    fn failures_are_suppressed_until_source_changes() {
        let f = Fixture::new("retry");
        let work = f.work("printf attempt >> \"$(dirname \"$0\")/attempts\"\nexit 1\n");
        let failed = f.generate(work.clone());
        assert!(!failed.published);
        assert!(!generate(work.clone(), failed.source.clone(), Duration::from_secs(2)).published);
        assert_eq!(fs::read_to_string(f.0.join("attempts")).unwrap(), "attempt");
        fs::write(&work.input, "different source").unwrap();
        assert!(!generate(work, failed.source, Duration::from_secs(2)).published);
        assert_eq!(
            fs::read_to_string(f.0.join("attempts")).unwrap(),
            "attemptattempt"
        );
    }

    #[test]
    fn timeout_releases_the_slot() {
        let mut child = Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        assert_eq!(
            wait_child(&mut child, || true).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert!(child.try_wait().unwrap().is_some(), "child was not reaped");
        let f = Fixture::new("timeout");
        let work = f.work("sleep 30\n");
        let mut state = super::super::State {
            queue: super::super::Queue::new(),
            jobs: Vec::new(),
            failed: std::collections::HashMap::new(),
        };
        state.queue.request(&work.uri);
        state.queue.request("busy");
        state.queue.request("next");
        let outcome = generate(work.clone(), None, Duration::ZERO);
        assert!(!outcome.published);
        assert_eq!(state.complete(&work.uri, outcome).as_deref(), Some("next"));
        assert_eq!(state.queue.running, ["busy", "next"]);
        assert_eq!(
            state.failed.get(&work.uri),
            Some(&Source::read(&work.input).unwrap())
        );
        assert_eq!(
            fs::read_dir(work.output.parent().unwrap()).unwrap().count(),
            0
        );
    }

    #[test]
    #[ignore = "requires glycin, gst-video-thumbnailer, ffmpeg and an isolated XDG_CACHE_HOME"]
    fn installed_thumbnailers_publish_valid_cache() {
        let cache = gtk::glib::user_cache_dir();
        assert!(
            cache.starts_with(std::env::temp_dir())
                && cache
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("filescope-thumbnail-real-")
        );
        let f = Fixture::new("real");
        f.png(&[]);
        let video = f.0.join("fixture.mp4");
        assert!(
            Command::new("ffmpeg")
                .args([
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=c=blue:s=64x64:r=1",
                    "-t",
                    "1",
                    "-an",
                    "-c:v",
                    "mpeg4",
                    "-y"
                ])
                .arg(&video)
                .status()
                .unwrap()
                .success()
        );
        for (input, mime) in [(f.0.join("fixture.png"), "image/png"), (video, "video/mp4")] {
            let file = gtk::gio::File::for_path(&input);
            let uri = file.uri().to_string();
            let output = super::super::cache_path(&cache.join("thumbnails"), &uri);
            let exec = super::super::system_thumbnailer(mime)
                .expect("missing system thumbnailer")
                .exec;
            assert!(
                generate(
                    Work {
                        input,
                        uri: uri.clone(),
                        output: output.clone(),
                        exec
                    },
                    None,
                    Duration::from_secs(30)
                )
                .published,
                "{mime} generation failed"
            );
            let png = Pixbuf::from_file(&output).unwrap();
            assert_eq!(
                png.option("tEXt::Thumb::URI").as_deref(),
                Some(uri.as_str())
            );
            let info = file
                .query_info(
                    "thumbnail::path,thumbnail::is-valid",
                    gtk::gio::FileQueryInfoFlags::NONE,
                    gtk::gio::Cancellable::NONE,
                )
                .unwrap();
            assert!(
                info.boolean("thumbnail::is-valid"),
                "GIO did not accept {mime} thumbnail"
            );
            assert_eq!(
                info.attribute_byte_string("thumbnail::path").as_deref(),
                output.to_str()
            );
            fs::remove_file(output).unwrap();
        }
    }
}
