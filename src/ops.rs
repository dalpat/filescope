// Background file-operation engine: copy, move, and delete run on a worker
// thread and report progress to the UI, stay cancellable, and ask the UI how to
// resolve name conflicts. No GTK here — the worker only talks over channels.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

use async_channel::Sender;

use crate::fileops;

/// Which operation a job performs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Copy,
    Move,
    Delete,
}

/// A message from the worker to the UI.
pub enum Event {
    /// The measured size of the whole job, sent once up front.
    Total { bytes: u64, items: u64 },
    /// Progress so far, sent as work proceeds.
    Advance { bytes_done: u64, items_done: u64, current: String },
    /// `name` already exists at the destination — the worker now blocks until
    /// the UI sends back a [`Decision`].
    Conflict { name: String },
    /// The job has ended (possibly cancelled or with some failures).
    Finished { ok: u64, failed: u64 },
}

/// The UI's answer to a [`Event::Conflict`].
pub enum Decision {
    Replace,
    Skip,
    /// Keep both by copying to a non-colliding "(copy)" name.
    Rename,
    ReplaceAll,
    SkipAll,
    Cancel,
}

/// Live counters shared through the copy/delete recursion.
struct Prog<'a> {
    tx: &'a Sender<Event>,
    cancel: &'a AtomicBool,
    bytes_done: u64,
    items_done: u64,
    current: String,
}

impl Prog<'_> {
    /// Push the current counters to the UI. Returns `false` if the UI has gone
    /// away (channel closed) so the worker can stop early.
    fn tick(&self) -> bool {
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
}

/// Run a job to completion on the calling (worker) thread. `dest` is the target
/// directory for Copy/Move and is ignored for Delete.
pub fn run(
    kind: Kind,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    decisions: Receiver<Decision>,
) {
    // Measure everything first so the progress bar has a denominator.
    let measured: Vec<(PathBuf, u64, u64)> =
        sources.iter().map(|s| { let (b, i) = measure(s); (s.clone(), b, i) }).collect();
    let total_bytes: u64 = measured.iter().map(|(_, b, _)| b).sum();
    let total_items: u64 = measured.iter().map(|(_, _, i)| i).sum();
    let _ = tx.send_blocking(Event::Total { bytes: total_bytes, items: total_items });

    let mut prog = Prog { tx: &tx, cancel: &cancel, bytes_done: 0, items_done: 0, current: String::new() };
    let (mut ok, mut failed) = (0u64, 0u64);
    // Sticky "apply to all" choice.
    let mut sticky: Option<Decision> = None;

    for (src, sbytes, sitems) in measured {
        if prog.cancelled() {
            break;
        }
        prog.current = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

        if kind == Kind::Delete {
            match remove_tree(&src, &mut prog) {
                Ok(()) => ok += 1,
                Err(_) => failed += 1,
            }
            continue;
        }

        // Copy / Move: resolve any destination-name conflict.
        let name = prog.current.clone();
        let mut target = dest.join(&name);
        if target.exists() {
            let decision = match &sticky {
                Some(Decision::ReplaceAll) => Decision::Replace,
                Some(Decision::SkipAll) => Decision::Skip,
                _ => {
                    if tx.send_blocking(Event::Conflict { name: name.clone() }).is_err() {
                        break;
                    }
                    decisions.recv().unwrap_or(Decision::Cancel)
                }
            };
            match decision {
                Decision::Cancel => {
                    cancel.store(true, Ordering::Relaxed);
                    break;
                }
                Decision::Skip => {
                    prog.bytes_done += sbytes;
                    prog.items_done += sitems;
                    prog.tick();
                    continue;
                }
                Decision::SkipAll => {
                    sticky = Some(Decision::SkipAll);
                    prog.bytes_done += sbytes;
                    prog.items_done += sitems;
                    prog.tick();
                    continue;
                }
                Decision::Rename => target = fileops::unique_destination(&dest, &name),
                Decision::Replace => {
                    let _ = fileops::remove(&target);
                }
                Decision::ReplaceAll => {
                    sticky = Some(Decision::ReplaceAll);
                    let _ = fileops::remove(&target);
                }
            }
        }

        let result = match kind {
            Kind::Copy => copy_tree(&src, &target, &mut prog),
            Kind::Move => move_entry(&src, &target, sbytes, sitems, &mut prog),
            Kind::Delete => unreachable!(),
        };
        match result {
            Ok(()) => ok += 1,
            Err(_) => failed += 1,
        }
    }

    let _ = tx.send_blocking(Event::Finished { ok, failed });
}

/// Move one entry: try a rename (instant, same filesystem), else copy + delete.
fn move_entry(src: &Path, dst: &Path, sbytes: u64, sitems: u64, prog: &mut Prog) -> io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => {
            prog.bytes_done += sbytes;
            prog.items_done += sitems;
            prog.tick();
            Ok(())
        }
        Err(_) => {
            copy_tree(src, dst, prog)?;
            fileops::remove(src)?;
            Ok(())
        }
    }
}

/// Recursively copy `src` to `dst`, updating `prog` and honouring cancellation.
fn copy_tree(src: &Path, dst: &Path, prog: &mut Prog) -> io::Result<()> {
    if prog.cancelled() {
        return Err(io::Error::from(io::ErrorKind::Interrupted));
    }
    let meta = fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        prog.items_done += 1;
    } else if meta.is_dir() {
        fs::create_dir(dst)?;
        prog.items_done += 1;
        if !prog.tick() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()), prog)?;
        }
        let _ = fs::set_permissions(dst, meta.permissions());
    } else {
        copy_file(src, dst, &meta, prog)?;
    }
    Ok(())
}

/// Copy a single file in chunks so a large file still advances the bar and stays
/// cancellable mid-copy.
fn copy_file(src: &Path, dst: &Path, meta: &fs::Metadata, prog: &mut Prog) -> io::Result<()> {
    let mut reader = fs::File::open(src)?;
    let mut writer = fs::File::create(dst)?;
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB
    loop {
        if prog.cancelled() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        prog.bytes_done += n as u64;
        if !prog.tick() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
    }
    let _ = fs::set_permissions(dst, meta.permissions());
    prog.items_done += 1;
    Ok(())
}

/// Recursively delete `path`, updating `prog`.
fn remove_tree(path: &Path, prog: &mut Prog) -> io::Result<()> {
    if prog.cancelled() {
        return Err(io::Error::from(io::ErrorKind::Interrupted));
    }
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path(), prog)?;
        }
        fs::remove_dir(path)?;
    } else {
        prog.bytes_done += meta.len();
        fs::remove_file(path)?;
    }
    prog.items_done += 1;
    prog.tick();
    Ok(())
}

/// Total (bytes, items) of `path`, counting the entry itself as one item and
/// recursing into directories (symlinks counted, never followed).
fn measure(path: &Path) -> (u64, u64) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return (0, 0);
    };
    if meta.is_dir() && !meta.file_type().is_symlink() {
        let (mut bytes, mut items) = (0u64, 1u64);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                let (b, i) = measure(&entry.path());
                bytes += b;
                items += i;
            }
        }
        (bytes, items)
    } else {
        (meta.len(), 1)
    }
}

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
        run(Kind::Copy, vec![src], dest.clone(), Arc::new(AtomicBool::new(false)), tx, dec_rx);

        let (mut total, mut finished) = (None, None);
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Total { bytes, items } => total = Some((bytes, items)),
                Event::Finished { ok, failed } => finished = Some((ok, failed)),
                _ => {}
            }
        }
        // src dir + a.txt + sub dir + b.txt = 4 items, 12 bytes total.
        assert_eq!(total, Some((12, 4)));
        assert_eq!(finished, Some((1, 0)));

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
            run(Kind::Copy, vec![src], dest2, Arc::new(AtomicBool::new(false)), tx, dec_rx);
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
        run(Kind::Delete, vec![tmp.join("d")], PathBuf::new(), Arc::new(AtomicBool::new(false)), tx, dec_rx);
        while rx.try_recv().is_ok() {}
        assert!(!tmp.join("d").exists());
        let _ = fs::remove_dir_all(&tmp);
    }
}
