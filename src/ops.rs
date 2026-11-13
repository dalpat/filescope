//! Background file operations and their exact outcomes. No GTK in the worker.
use crate::fileops;
use async_channel::Sender;
use std::cell::Cell;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Copy,
    Move,
    Delete,
}

pub enum Event {
    Total {
        bytes: u64,
        items: u64,
    },
    Advance {
        bytes_done: u64,
        items_done: u64,
        current: String,
    },
    Conflict {
        name: String,
    },
    Finished {
        ok: u64,
        failed: u64,
        pairs: Vec<(PathBuf, PathBuf)>,
        failures: Vec<Failure>,
        cancelled: bool,
    },
}

pub enum Decision {
    Replace,
    Skip,
    Rename,
    ReplaceAll,
    SkipAll,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
    size: u64,
    modified: i64,
    nanos: i64,
    kind: u32,
}
impl Identity {
    fn from(meta: &fs::Metadata) -> Self {
        Self {
            device: meta.dev(),
            inode: meta.ino(),
            size: meta.len(),
            modified: meta.mtime(),
            nanos: meta.mtime_nsec(),
            kind: meta.mode() & libc::S_IFMT,
        }
    }
    fn read(path: &Path) -> io::Result<Self> {
        Ok(Self::from(&fs::symlink_metadata(path)?))
    }
    fn same_entry(&self, other: &Self) -> bool {
        (self.device, self.inode, self.kind) == (other.device, other.inode, other.kind)
    }
}

#[derive(Clone, Debug)]
pub struct Failure {
    pub source: PathBuf,
    pub message: String,
    /// Cancellation is classified by the error kind, never its message.
    stopped: bool,
    identity: Option<Identity>,
}
impl Failure {
    pub fn can_retry(&self) -> bool {
        self.identity.is_some() && !self.stopped
    }
    pub(crate) fn stopped(&self) -> bool {
        self.stopped
    }
}

fn record_failure(source: PathBuf, error: io::Error, identity: Option<Identity>) -> Failure {
    Failure {
        source,
        message: error.to_string(),
        stopped: error.kind() == io::ErrorKind::Interrupted,
        identity,
    }
}


struct Request {
    source: PathBuf,
    expected: Option<Identity>,
    retry: bool,
}

#[cfg(test)]
type CopyPause = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
#[cfg(test)]
static NEXT_COPY_PAUSE: std::sync::Mutex<Option<CopyPause>> = std::sync::Mutex::new(None);

/// Deterministically hold a supported regular copy after its first written chunk.
#[cfg(test)]
pub(crate) fn pause_next_copy() -> (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>) {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    assert!(
        NEXT_COPY_PAUSE
            .lock()
            .unwrap()
            .replace((ready_tx, release_rx))
            .is_none()
    );
    (release_tx, ready_rx)
}

struct Prog<'a> {
    tx: &'a Sender<Event>,
    cancel: &'a AtomicBool,
    bytes_done: u64,
    items_done: u64,
    current: String,
    capture: bool,
    copied: Vec<(PathBuf, Identity)>,
    destinations: Vec<(PathBuf, Identity)>,
    last_tick: Cell<Option<std::time::Instant>>,
    #[cfg(test)]
    pause: Option<CopyPause>,
}
impl Prog<'_> {
    fn tick(&self) -> bool {
        if self.tx.is_closed() || self.cancelled() {
            return false;
        }
        let now = std::time::Instant::now();
        if self
            .last_tick
            .get()
            .is_some_and(|last| now.duration_since(last) < std::time::Duration::from_millis(50))
        {
            return true;
        }
        self.last_tick.set(Some(now));
        self.tx
            .send_blocking(Event::Advance {
                bytes_done: self.bytes_done,
                items_done: self.items_done,
                current: self.current.clone(),
            })
            .is_ok()
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
    fn check(&self) -> io::Result<()> {
        if self.cancelled() {
            Err(io::ErrorKind::Interrupted.into())
        } else {
            Ok(())
        }
    }
}

pub fn run(
    kind: Kind,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    decisions: Receiver<Decision>,
) {
    let requests = sources
        .into_iter()
        .map(|source| Request {
            source,
            expected: None,
            retry: false,
        })
        .collect();
    run_requests(kind, requests, dest, cancel, tx, decisions);
}

pub fn retry(
    kind: Kind,
    failures: Vec<Failure>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    decisions: Receiver<Decision>,
) {
    let requests = failures
        .into_iter()
        .map(|failure| Request {
            source: failure.source,
            expected: failure.identity,
            retry: true,
        })
        .collect();
    run_requests(kind, requests, dest, cancel, tx, decisions);
}

fn run_requests(
    kind: Kind,
    requests: Vec<Request>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    decisions: Receiver<Decision>,
) {
    let mut failures = Vec::new();
    let mut measured = Vec::new();
    let destination_error = if kind == Kind::Delete {
        None
    } else {
        fs::metadata(&dest).and_then(|metadata| {
            if metadata.is_dir() { Ok(()) }
            else { Err(io::Error::other("The destination is not a folder")) }
        })
            .err()
            .map(|error| (error.kind(), error.to_string()))
    };
    for request in requests {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let identity = Identity::read(&request.source).ok();
        if request.retry && (request.expected.is_none() || request.expected != identity) {
            failures.push(Failure {
                source: request.source,
                message: "The source changed or could not be verified; start a new operation"
                    .into(),
                stopped: false,
                identity: None,
            });
            continue;
        }
        if let Some(error) = destination_error.clone().map(|(kind, message)| io::Error::new(kind, message)) {
            failures.push(record_failure(request.source, error, identity));
            continue;
        }
        let identity = Identity::read(&request.source).ok();
        let (bytes, items) = measure(&request.source, &cancel);
        measured.push((request.source, bytes, items, identity));
    }
    let bytes = measured
        .iter()
        .fold(0u64, |sum, (_, n, _, _)| sum.saturating_add(*n));
    let items = measured
        .iter()
        .fold(0u64, |sum, (_, _, n, _)| sum.saturating_add(*n));
    let _ = tx.send_blocking(Event::Total {
        bytes,
        items,
    });
    let mut prog = Prog {
        tx: &tx,
        cancel: &cancel,
        bytes_done: 0,
        items_done: 0,
        current: String::new(),
        capture: kind == Kind::Move,
        copied: Vec::new(),
        destinations: Vec::new(),
        last_tick: Cell::new(None),
        #[cfg(test)]
        pause: NEXT_COPY_PAUSE.lock().unwrap().take(),
    };
    let mut pairs = Vec::new();
    let mut ok = 0;
    let mut sticky = None;

    for (src, sbytes, sitems, identity) in measured {
        if prog.cancelled() {
            break;
        }
        prog.current = src
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        prog.copied.clear();
        prog.destinations.clear();
        let mut replaced = false;
        let mut target = None;
        let mut performed = false;
        let result = (|| -> io::Result<()> {
            let approved = identity
                .as_ref()
                .ok_or_else(|| io::Error::other("The source is no longer available"))?;
            verify_source(&src, approved)?;
            if kind == Kind::Delete {
                remove_tree(&src, &mut prog, Some(approved))?;
                performed = true;
                return Ok(());
            }
            let name = src.file_name().ok_or_else(|| io::Error::other("The source has no filename"))?;
            let mut dst = dest.join(name);
            validate_target(&src, &dst, true)?;
            if occupied(&dst)? {
                let conflict_identity = Identity::read(&dst)?;
                let decision = match sticky {
                    Some(true) => Decision::Replace,
                    Some(false) => Decision::Skip,
                    None => {
                        tx.send_blocking(Event::Conflict {
                            name: name.to_string_lossy().into_owned(),
                        })
                        .map_err(|_| io::Error::from(io::ErrorKind::Interrupted))?;
                        decisions.recv().unwrap_or(Decision::Cancel)
                    }
                };
                match decision {
                    Decision::Cancel => {
                        cancel.store(true, Ordering::Relaxed);
                        return Err(io::ErrorKind::Interrupted.into());
                    }
                    Decision::Skip | Decision::SkipAll => {
                        if matches!(decision, Decision::SkipAll) {
                            sticky = Some(false);
                        }
                        prog.bytes_done = prog.bytes_done.saturating_add(sbytes);
                        prog.items_done = prog.items_done.saturating_add(sitems);
                        prog.tick();
                        return Ok(());
                    }
                    Decision::Rename => {
                        dst = unique_target(&dest, name, &cancel)?
                    }
                    Decision::Replace | Decision::ReplaceAll => {
                        if matches!(decision, Decision::ReplaceAll) {
                            sticky = Some(true);
                        }
                        verify_source(&src, approved)?;
                        if Identity::read(&dst).ok().as_ref() != Some(&conflict_identity) {
                            return Err(io::Error::other(
                                "The destination changed while confirmation was open; review the conflict again",
                            ));
                        }
                        validate_target(&src, &dst, false)?;
                        fileops::remove(&dst)?;
                        replaced = true;
                    }
                }
            }
            validate_target(&src, &dst, false)?;
            verify_source(&src, approved)?;
            if kind == Kind::Copy {
                copy_tree(&src, &dst, &mut prog, Some(approved))?;
                verify_destinations(&prog)?;
            } else {
                move_entry(&src, &dst, sbytes, sitems, &mut prog, approved)?;
            }
            performed = true;
            if !replaced {
                target = Some(dst);
            }
            Ok(())
        })();
        match result {
            Ok(()) if target.is_some() => {
                ok += 1;
                pairs.push((src, target.unwrap()));
            }
            Ok(()) => {
                // Skipped entries are processed, but are not successful transfers.
                if performed {
                    ok += 1;
                }
            }
            Err(error) => {
                failures.push(record_failure(
                    src,
                    error,
                    if kind == Kind::Delete { None } else { identity },
                ));
            }
        }
    }
    prog.last_tick.set(None);
    prog.tick();
    let _ = tx.send_blocking(Event::Finished {
        ok,
        failed: failures.len() as u64,
        pairs,
        failures,
        cancelled: cancel.load(Ordering::Relaxed),
    });
}

fn occupied(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}


fn validate_target(src: &Path, dst: &Path, allow_same: bool) -> io::Result<()> {
    let source = Identity::read(src)?;
    if let Ok(target) = Identity::read(dst) {
        if source.same_entry(&target) {
            return if allow_same {
                Ok(())
            } else {
                Err(io::Error::other(
                    "Source and destination refer to the same item",
                ))
            };
        }
    }
    if !allow_same && fs::symlink_metadata(dst).is_ok_and(|meta| meta.is_dir()) {
        let parent = src
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let source_entry = fs::canonicalize(parent)?.join(
            src.file_name()
                .ok_or_else(|| io::Error::other("No source name"))?,
        );
        if source_entry.starts_with(fs::canonicalize(dst)?) {
            return Err(io::Error::other(
                "A folder containing the source cannot be replaced",
            ));
        }
    }
    if source.kind == libc::S_IFDIR {
        let source = fs::canonicalize(src)?;
        let target = match fs::canonicalize(dst) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::canonicalize(
                dst.parent()
                    .ok_or_else(|| io::Error::other("No destination folder"))?,
            )?
            .join(
                dst.file_name()
                    .ok_or_else(|| io::Error::other("No destination name"))?,
            ),
            Err(error) => return Err(error),
        };
        if target.starts_with(source) {
            return Err(io::Error::other(
                "A folder cannot be copied or moved inside itself",
            ));
        }
    }
    Ok(())
}

fn unique_target(
    dir: &Path,
    name: &OsStr,
    cancel: &AtomicBool,
) -> io::Result<PathBuf> {
    let original = Path::new(name);
    for number in 1u64.. {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let mut candidate = original.file_stem().unwrap_or(name).to_os_string();
        candidate.push(if number == 1 {
            " (copy)".into()
        } else {
            format!(" (copy {number})")
        });
        if let Some(extension) = original.extension() {
            candidate.push(".");
            candidate.push(extension);
        }
        let path = dir.join(candidate);
        if !occupied(&path)? {
            return Ok(path);
        }
    }
    unreachable!()
}


fn verify_source(path: &Path, approved: &Identity) -> io::Result<()> {
    if Identity::read(path).ok().as_ref() == Some(approved) {
        Ok(())
    } else {
        Err(io::Error::other(
            "The source changed; start a new operation",
        ))
    }
}

fn move_entry(
    src: &Path,
    dst: &Path,
    sbytes: u64,
    sitems: u64,
    prog: &mut Prog,
    approved: &Identity,
) -> io::Result<()> {
    prog.check()?;
    verify_source(src, approved)?;
    if fileops::rename_no_replace(src, dst).is_ok() {
        prog.bytes_done = prog.bytes_done.saturating_add(sbytes);
        prog.items_done = prog.items_done.saturating_add(sitems);
        prog.tick();
        return Ok(());
    }
    copy_tree(src, dst, prog, Some(approved))?;
    prog.check()?;
    remove_copied(prog)
}

fn remove_copied(prog: &mut Prog) -> io::Result<()> {
    verify_destinations(prog)?;
    for (path, identity) in &prog.copied {
        if Identity::read(path).ok().as_ref() != Some(identity) {
            return Err(io::Error::other(
                "The source changed during transfer; it was not removed",
            ));
        }
    }
    // Only remove copied identities. A newly added directory child makes
    // remove_dir fail; it is never recursively deleted as an unseen entry.
    for (path, identity) in prog.copied.iter().rev() {
        prog.check()?;
        let current = Identity::read(path)?;
        let unchanged = if identity.kind == libc::S_IFDIR {
            identity.same_entry(&current)
        } else {
            *identity == current
        };
        if !unchanged {
            return Err(io::Error::other("The source changed before removal"));
        }
        if identity.kind == libc::S_IFDIR {
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn copy_tree(
    src: &Path,
    dst: &Path,
    prog: &mut Prog,
    approved: Option<&Identity>,
) -> io::Result<()> {
    prog.check()?;
    let meta = fs::symlink_metadata(src).map_err(|error| error_at(src, error))?;
    if approved.is_some_and(|approved| *approved != Identity::from(&meta)) {
        return Err(io::Error::other("The source changed before copying"));
    }
    if prog.capture {
        prog.copied.push((src.to_path_buf(), Identity::from(&meta)));
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(
            fs::read_link(src).map_err(|error| error_at(src, error))?,
            dst,
        )
        .map_err(|error| error_at(dst, error))?;
        prog.bytes_done = prog.bytes_done.saturating_add(meta.len());
        prog.items_done += 1;
        if !prog.tick() {
            return Err(io::ErrorKind::Interrupted.into());
        }
    } else if meta.is_dir() {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dst)
            .map_err(|error| error_at(dst, error))?;
        prog.items_done += 1;
        if !prog.tick() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let entries = fs::read_dir(src).map_err(|error| error_at(src, error))?;
        verify_source(src, &Identity::from(&meta))?;
        for entry in entries {
            let entry = entry.map_err(|error| error_at(src, error))?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()), prog, None)?;
        }
        verify_source(src, &Identity::from(&meta))?;
        fs::set_permissions(dst, meta.permissions())?;
    } else if meta.is_file() {
        copy_file(src, dst, &meta, prog).map_err(|error| error_at(src, error))?;
    } else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{src:?}: this file type cannot be copied safely"),
        ));
    }
    verify_source(src, &Identity::from(&meta))?;
    prog.destinations
        .push((dst.to_path_buf(), Identity::read(dst)?));
    Ok(())
}

fn verify_destinations(prog: &Prog) -> io::Result<()> {
    for (path, approved) in &prog.destinations {
        prog.check()?;
        if Identity::read(path).ok().as_ref() != Some(approved) {
            return Err(io::Error::other(format!(
                "{path:?}: destination changed before completion; the source was kept"
            )));
        }
    }
    Ok(())
}

fn error_at(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{path:?}: {error}"))
}

fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata, prog: &mut Prog) -> io::Result<()> {
    let mut reader = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(src)?;
    if Identity::from(&reader.metadata()?) != Identity::from(meta) {
        return Err(io::Error::other("The source changed before copying"));
    }
    let mut writer = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dst)?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        prog.check()?;
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        writer.write_all(&buffer[..count])?;
        prog.bytes_done = prog.bytes_done.saturating_add(count as u64);
        if !prog.tick() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        #[cfg(test)]
        if let Some((ready, release)) = prog.pause.take() {
            let _ = ready.send(());
            release
                .recv_timeout(std::time::Duration::from_secs(30))
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::Interrupted,
                        "test copy pause released or expired",
                    )
                })?;
        }
    }
    writer.flush()?;
    drop(writer);
    if Identity::read(src)? != Identity::from(meta) {
        return Err(io::Error::other("The source changed during copying"));
    }
    fs::set_permissions(dst, meta.permissions())?;
    prog.items_done += 1;
    Ok(())
}

fn remove_tree(path: &Path, prog: &mut Prog, approved: Option<&Identity>) -> io::Result<()> {
    prog.check()?;
    let meta = fs::symlink_metadata(path)?;
    if approved.is_some_and(|approved| *approved != Identity::from(&meta)) {
        return Err(io::Error::other("The source changed before deletion"));
    }
    if meta.is_dir() && !meta.file_type().is_symlink() {
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path(), prog, None)?;
        }
        fs::remove_dir(path)?;
    } else {
        prog.bytes_done = prog.bytes_done.saturating_add(meta.len());
        fs::remove_file(path)?;
    }
    prog.items_done += 1;
    prog.tick();
    Ok(())
}

fn measure(path: &Path, cancel: &AtomicBool) -> (u64, u64) {
    if cancel.load(Ordering::Relaxed) {
        return (0, 0);
    }
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        let (mut bytes, mut items) = (0u64, 1u64);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let (b, i) = measure(&entry.path(), cancel);
                bytes = bytes.saturating_add(b);
                items = items.saturating_add(i);
            }
        }
        (bytes, items)
    } else {
        (meta.len(), 1)
    }
}

#[cfg(test)]
#[path = "ops/regression_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn copy_job_copies_a_tree_and_reports_totals() {
        let tmp = std::env::temp_dir().join(format!("filescope-ops-{}", std::process::id()));
        let src = tmp.join("src");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap(); // 5 bytes
        fs::write(src.join("sub/b.txt"), b"world!!").unwrap(); // 7 bytes
        let dest = tmp.join("dest");
        fs::create_dir_all(&dest).unwrap();

        let (tx, rx) = async_channel::unbounded();
        let (_dec_tx, dec_rx) = mpsc::channel();
        run(
            Kind::Copy,
            vec![src.clone()],
            dest.clone(),
            Arc::new(AtomicBool::new(false)),
            tx,
            dec_rx,
        );

        let (mut total, mut finished) = (None, None);
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Total { bytes, items, .. } => total = Some((bytes, items)),
                Event::Finished {
                    ok, failed, pairs, ..
                } => finished = Some((ok, failed, pairs)),
                _ => {}
            }
        }
        // src dir + a.txt + sub dir + b.txt = 4 items, 12 bytes total.
        assert_eq!(total, Some((12, 4)));
        let (ok, failed, pairs) = finished.expect("the job reports Finished");
        assert_eq!((ok, failed), (1, 0));
        // It reports what it actually created, which is what undo replays.
        assert_eq!(pairs, vec![(src, dest.join("src"))]);

        let copied = dest.join("src");
        assert!(copied.join("a.txt").exists());
        assert_eq!(fs::read(copied.join("sub/b.txt")).unwrap(), b"world!!");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn conflict_keep_both_renames_without_clobbering() {
        let tmp = std::env::temp_dir().join(format!("filescope-opsconf-{}", std::process::id()));
        let src = tmp.join("item.txt");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(&src, b"new").unwrap();
        let dest = tmp.join("dest");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("item.txt"), b"old").unwrap();

        let (tx, rx) = async_channel::unbounded();
        let (dec_tx, dec_rx) = mpsc::channel();
        let dest2 = dest.clone();
        let worker = std::thread::spawn(move || {
            run(
                Kind::Copy,
                vec![src],
                dest2,
                Arc::new(AtomicBool::new(false)),
                tx,
                dec_rx,
            );
        });
        // Answer the conflict with "Keep Both".
        loop {
            match rx.recv_blocking() {
                Ok(Event::Conflict { .. }) => dec_tx.send(Decision::Rename).unwrap(),
                Ok(Event::Finished { .. }) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        worker.join().unwrap();

        // The existing file is untouched and a "(copy)" holds the new content.
        assert_eq!(fs::read(dest.join("item.txt")).unwrap(), b"old");
        assert_eq!(fs::read(dest.join("item (copy).txt")).unwrap(), b"new");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn delete_job_removes_sources() {
        let tmp = std::env::temp_dir().join(format!("filescope-opsdel-{}", std::process::id()));
        fs::create_dir_all(tmp.join("d/inner")).unwrap();
        fs::write(tmp.join("d/inner/f"), b"data").unwrap();
        let (tx, rx) = async_channel::unbounded();
        let (_dec_tx, dec_rx) = mpsc::channel();
        run(
            Kind::Delete,
            vec![tmp.join("d")],
            PathBuf::new(),
            Arc::new(AtomicBool::new(false)),
            tx,
            dec_rx,
        );
        while rx.try_recv().is_ok() {}
        assert!(!tmp.join("d").exists());
        let _ = fs::remove_dir_all(&tmp);
    }
}
