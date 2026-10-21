// Ask the desktop thumbnailer for a missing preview. The original file is never
// decoded here.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use gtk::{gio, glib, prelude::*};

const THUMB_SIZE: i32 = 256;
const IMAGE_MAX: u64 = 10 * 1024 * 1024;
const RUN_SLOTS: usize = 2;

mod generation;
use generation::{Outcome, Source, Work};

pub(crate) struct Thumbnailer {
    pub exec: String,
}

pub(crate) struct Queue {
    pub running: Vec<String>,
    pub waiting: Vec<String>,
}

impl Queue {
    pub(crate) fn new() -> Self {
        Self { running: Vec::new(), waiting: Vec::new() }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.running.len() + self.waiting.len()
    }

    /// `Start` runs now. `Wait` runs when a slot frees. A URI already in either
    /// list is left where it is.
    pub(crate) fn request(&mut self, uri: &str) -> Admit {
        if self.running.iter().any(|u| u == uri) || self.waiting.iter().any(|u| u == uri) {
            return Admit::Duplicate;
        }
        if self.running.len() < RUN_SLOTS {
            self.running.push(uri.to_string());
            Admit::Start
        } else {
            self.waiting.push(uri.to_string());
            Admit::Wait
        }
    }

    /// Drop a finished URI and start the next waiter, if a slot is free.
    pub(crate) fn finish(&mut self, uri: &str) -> Option<String> {
        self.running.retain(|u| u != uri);
        if self.running.len() >= RUN_SLOTS {
            return None;
        }
        let next = self.waiting.first().cloned()?;
        self.waiting.remove(0);
        self.running.push(next.clone());
        Some(next)
    }
}

pub(crate) enum Admit {
    Start,
    Wait,
    Duplicate,
}

pub(crate) fn thumbnail_command(exec: &str, uri: &str, input: &Path, output: &Path, size: i32) -> Vec<String> {
    let mut argv: Vec<String> = exec
        .split_whitespace()
        .map(|word| {
            word.replace("%u", uri)
                .replace("%i", &input.to_string_lossy())
                .replace("%o", &output.to_string_lossy())
                .replace("%s", &size.to_string())
        })
        .collect();
    if let Some(program) = argv.first_mut()
        && let Some(name) = Path::new(&*program).file_name()
    {
        *program = name.to_string_lossy().into_owned();
    }
    argv
}

pub(crate) fn cache_path(root: &Path, uri: &str) -> PathBuf {
    let hash = glib::compute_checksum_for_string(glib::ChecksumType::Md5, uri).unwrap_or_default();
    root.join("large").join(format!("{hash}.png"))
}

#[cfg(test)]
pub(crate) fn pick_thumbnailer(dir: &Path, mime: &str) -> Option<Thumbnailer> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "thumbnailer") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Some(parsed) = parse_thumbnailer(&text) else { continue };
        if !parsed.mimes.iter().any(|kind| kind == mime) {
            continue;
        }
        if !try_exec_exists(&parsed.try_exec) {
            continue;
        }
        return Some(Thumbnailer { exec: parsed.exec });
    }
    None
}

pub(crate) fn skips_large(thumbnailer: &Thumbnailer, bytes: u64) -> bool {
    thumbnailer.exec.contains("glycin-thumbnailer") && bytes > IMAGE_MAX
}

struct Parsed {
    exec: String,
    try_exec: String,
    mimes: Vec<String>,
}

/// A path must exist. A bare name must be on `PATH` (`evince-thumbnailer`).
fn try_exec_exists(value: &str) -> bool {
    if value.contains('/') {
        return Path::new(value).is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(value).is_file())
    })
}

fn system_thumbnailer(mime: &str) -> Option<Thumbnailer> {
    static CACHE: OnceLock<Vec<Parsed>> = OnceLock::new();
    let list = CACHE.get_or_init(|| load_available(Path::new("/usr/share/thumbnailers")));
    list.iter()
        .find(|parsed| parsed.mimes.iter().any(|kind| kind == mime))
        .map(|parsed| Thumbnailer { exec: parsed.exec.clone() })
}

fn load_available(dir: &Path) -> Vec<Parsed> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return found };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "thumbnailer") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Some(parsed) = parse_thumbnailer(&text) else { continue };
        if try_exec_exists(&parsed.try_exec) {
            found.push(parsed);
        }
    }
    found
}

fn parse_thumbnailer(text: &str) -> Option<Parsed> {
    let mut exec = None;
    let mut try_exec = None;
    let mut mimes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("Exec=") {
            exec = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("TryExec=") {
            try_exec = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("MimeType=") {
            mimes = value.split(';').map(str::trim).filter(|mime| !mime.is_empty()).map(str::to_string).collect();
        }
    }
    Some(Parsed { exec: exec?, try_exec: try_exec?, mimes })
}

struct Job {
    uri: String,
    work: Work,
    image: gtk::Image,
    token: u64,
    pixel_size: i32,
}

struct State {
    queue: Queue,
    jobs: Vec<Job>,
    failed: HashMap<String, Source>,
}

impl State {
    fn complete(&mut self, uri: &str, outcome: Outcome) -> Option<String> {
        if outcome.published {
            self.failed.remove(uri);
        } else if let Some(source) = outcome.source {
            self.failed.insert(uri.to_string(), source);
        }
        self.jobs.retain(|job| job.uri != uri);
        self.queue.finish(uri)
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State {
        queue: Queue::new(),
        jobs: Vec::new(),
        failed: HashMap::new(),
    });
}

static THUMB_GEN: AtomicU64 = AtomicU64::new(0);

/// Tag `image` so a late thumbnail cannot paint over a reused row.
pub(crate) fn arm(image: &gtk::Image) -> u64 {
    let token = THUMB_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    unsafe { image.set_data("thumb-token", token) };
    token
}

/// Queue a thumbnailer for `info` when GIO has no valid thumbnail yet.
pub(crate) fn schedule(image: &gtk::Image, info: &gio::FileInfo, pixel_size: i32, token: u64) {
    let Some(file) = info.attribute_object("standard::file").and_downcast::<gio::File>() else { return };
    let uri = file.uri().to_string();
    let Some(mime) = info.content_type() else { return };
    let bytes = info.size().max(0) as u64;
    let Some(thumbnailer) = system_thumbnailer(mime.as_str()) else { return };
    if skips_large(&thumbnailer, bytes) {
        return;
    }
    let Some(input) = file.path() else { return; };
    let xdg = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
    let output = cache_path(&cache_root(xdg.as_deref(), &glib::home_dir()), &uri);
    let work = Work { uri: uri.clone(), input, output, exec: thumbnailer.exec };
    let job = Job {
        uri: uri.clone(),
        work,
        image: image.clone(),
        token,
        pixel_size,
    };
    let admit = STATE.with(|state| {
        let mut state = state.borrow_mut();
        let admit = state.queue.request(&uri);
        match admit {
            Admit::Duplicate => {
                if let Some(existing) = state.jobs.iter_mut().find(|job| job.uri == uri) {
                    existing.work = job.work;
                    existing.image = job.image;
                    existing.token = job.token;
                    existing.pixel_size = job.pixel_size;
                }
            }
            Admit::Start | Admit::Wait => state.jobs.push(job),
        }
        admit
    });
    if matches!(admit, Admit::Start) {
        launch(uri);
    }
}

fn launch(uri: String) {
    let Some((work, failed)) = STATE.with(|state| {
        let state = state.borrow();
        state.jobs.iter().find(|job| job.uri == uri)
            .map(|job| (job.work.clone(), state.failed.get(&uri).cloned()))
    }) else { return; };
    glib::spawn_future_local(async move {
        let outcome = gio::spawn_blocking(move || generation::generate(work, failed, Duration::from_secs(30)))
            .await.unwrap_or(Outcome { source: None, published: false });
        let paint = STATE.with(|state| {
            state.borrow().jobs.iter().find(|job| job.uri == uri).map(|job| {
                (job.image.clone(), job.token, job.pixel_size, job.work.output.clone())
            })
        });
        if outcome.published
            && let Some((image, token, pixel_size, path)) = paint
        {
            crate::icon::invalidate_photo(&path);
            if let Some(pixels) = crate::icon::decode_limited(path.clone(), pixel_size, None).await {
                let texture = pixels.texture();
                // Refresh the cached texture even when the requesting row was recycled.
                crate::icon::store_photo(&path, pixel_size, &texture);
                let current = unsafe { image.data::<u64>("thumb-token") };
                if current.map(|p| unsafe { *p.as_ref() }) == Some(token) {
                    image.set_paintable(Some(&texture));
                }
            }
        }
        let next = STATE.with(|state| state.borrow_mut().complete(&uri, outcome));
        if let Some(next) = next { launch(next); }
    });
}

fn cache_root(xdg_cache_home: Option<&Path>, home: &Path) -> PathBuf {
    xdg_cache_home.filter(|root| !root.as_os_str().is_empty()).map(Path::to_path_buf)
        .unwrap_or_else(|| home.join(".cache")).join("thumbnails")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const URI: &str = "file:///home/a/b.mp4";
    const GST: &str = "/usr/bin/gst-video-thumbnailer --input-uri %u --output %o --size %s";

    #[test]
    fn cache_path_honors_xdg_cache_home() {
        let home = Path::new("/home/test");
        assert_eq!(cache_root(Some(Path::new("/tmp/custom")), home), Path::new("/tmp/custom/thumbnails"));
        for root in [None, Some(Path::new(""))] {
            assert_eq!(cache_root(root, home), Path::new("/home/test/.cache/thumbnails"));
        }
    }

    #[test]
    fn thumbnail_command_fills_uri_output_and_size() {
        let argv = thumbnail_command(GST, URI, Path::new("/home/a/b.mp4"), Path::new("/tmp/out.png"), 256);
        assert_eq!(
            argv,
            [
                "gst-video-thumbnailer",
                "--input-uri",
                "file:///home/a/b.mp4",
                "--output",
                "/tmp/out.png",
                "--size",
                "256",
            ]
        );
    }

    #[test]
    fn thumbnail_command_fills_input_path() {
        let argv = thumbnail_command(
            "heif-thumbnailer -s %s %i %o",
            URI,
            Path::new("/home/a/b.heic"),
            Path::new("/tmp/out.png"),
            256,
        );
        assert_eq!(argv, ["heif-thumbnailer", "-s", "256", "/home/a/b.heic", "/tmp/out.png"]);
    }

    #[test]
    fn bare_try_exec_uses_path() {
        let dir = std::env::temp_dir().join(format!("filescope-thumb-try-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("pdf.thumbnailer"),
            "\
[Thumbnailer Entry]
TryExec=true
Exec=evince-thumbnailer -s %s %u %o
MimeType=application/pdf;
",
        )
        .unwrap();
        assert!(pick_thumbnailer(&dir, "application/pdf").is_some());
        fs::write(
            dir.join("pdf.thumbnailer"),
            "\
[Thumbnailer Entry]
TryExec=filescope-not-a-real-binary
Exec=evince-thumbnailer -s %s %u %o
MimeType=application/pdf;
",
        )
        .unwrap();
        assert!(pick_thumbnailer(&dir, "application/pdf").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_path_is_the_md5_of_the_uri() {
        let root = Path::new("/home/dsp/.cache/thumbnails");
        let path = cache_path(root, URI);
        assert_eq!(
            path,
            root.join("large").join("f6c9ca96d616f5e927f8f1a6652bba91.png")
        );
    }

    #[test]
    fn picks_the_thumbnailer_for_the_mime() {
        let dir = fixture_dir("pick");
        let video = pick_thumbnailer(&dir, "video/mp4").expect("video");
        let image = pick_thumbnailer(&dir, "image/jpeg").expect("image");
        assert!(video.exec.contains("gst-video-thumbnailer"));
        assert!(image.exec.contains("glycin-thumbnailer"));
        assert!(pick_thumbnailer(&dir, "text/plain").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn skips_a_large_image_and_allows_a_large_video() {
        let dir = fixture_dir("skip");
        let bytes = 11 * 1024 * 1024;
        let image = pick_thumbnailer(&dir, "image/jpeg").expect("image");
        let video = pick_thumbnailer(&dir, "video/mp4").expect("video");
        assert!(skips_large(&image, bytes));
        assert!(!skips_large(&video, bytes));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_request_for_the_same_uri_is_not_queued() {
        let mut queue = Queue::new();
        queue.request("a");
        queue.request("a");
        assert_eq!(queue.len(), 1);
        queue.request("b");
        queue.request("c");
        assert_eq!(queue.waiting.len(), 1);
        assert_eq!(queue.len(), 3);
        queue.request("a");
        queue.request("c");
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.waiting.len(), 1);
        assert_eq!(queue.finish("a").as_deref(), Some("c"));
        assert!(queue.waiting.is_empty());
        assert_eq!(queue.running, ["b".to_string(), "c".to_string()]);
    }

    fn fixture_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("filescope-thumb-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("gst.thumbnailer"),
            "\
[Thumbnailer Entry]
TryExec=/bin/true
Exec=/usr/bin/gst-video-thumbnailer --input-uri %u --output %o --size %s
MimeType=video/mp4;
",
        )
        .unwrap();
        fs::write(
            dir.join("glycin.thumbnailer"),
            "\
[Thumbnailer Entry]
TryExec=/bin/true
Exec=/usr/bin/glycin-thumbnailer --input %u --output %o --size %s
MimeType=image/jpeg;
",
        )
        .unwrap();
        dir
    }
}
