use super::*;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;


#[test]
fn changed_destination_during_commit_retains_source() {
    for directory in [false, true] {
        let fixture = Fixture::new("destination-replaced");
        let src = fixture.0.join("source");
        let dst = fixture.0.join("destination");
        if directory {
            fs::create_dir_all(src.join("nested")).unwrap();
            fs::write(src.join("nested/item"), b"original").unwrap();
        } else {
            fs::write(&src, b"original").unwrap();
        }
        let approved = Identity::read(&src).unwrap();
        let (tx, _rx) = async_channel::unbounded();
        let cancel = AtomicBool::new(false);
        let mut prog = move_progress(&tx, &cancel);
        copy_tree(&src, &dst, &mut prog, Some(&approved)).unwrap();
        let commit = |path: &Path, _: &AtomicBool| -> io::Result<()> {
            if path == dst {
                if directory {
                    fs::write(dst.join("nested/item"), b"unrelated replacement")?;
                } else {
                    fs::rename(&dst, fixture.0.join("copied-original"))?;
                    fs::write(&dst, b"unrelated replacement")?;
                }
            }
            Ok(())
        };
        commit(&dst, &cancel).unwrap();
        assert!(remove_copied(&mut prog).is_err());
        let source = if directory {
            src.join("nested/item")
        } else {
            src
        };
        assert_eq!(fs::read(source).unwrap(), b"original");
    }
}

#[test]
fn replacing_a_source_ancestor_is_rejected() {
    for kind in [Kind::Copy, Kind::Move] {
        for directory in [false, true] {
            let fixture = Fixture::new("ancestor-replace");
            let dest = fixture.0.join("destination");
            let source = dest.join("item/sub/item");
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::write(dest.join("item/unrelated"), b"existing data").unwrap();
            if directory {
                fs::create_dir(&source).unwrap();
                fs::write(source.join("contents"), b"original").unwrap();
            } else {
                fs::write(&source, b"original").unwrap();
            }
            let events = execute(kind, vec![source.clone()], dest.clone(), Decision::Replace);
            let data = if directory {
                source.join("contents")
            } else {
                source.clone()
            };
            assert_eq!(
                fs::read(data).unwrap(),
                b"original",
                "{kind:?}: replaced a source ancestor"
            );
            assert_eq!(
                fs::read(dest.join("item/unrelated")).unwrap(),
                b"existing data"
            );
            assert!(events.iter().any(|event| matches!(
                event,
                Event::Finished {
                    ok: 0,
                    failed: 1,
                    ..
                }
            )));
        }
    }
}

#[test]
fn conflict_confirmation_cannot_take_a_replacement_source() {
    for kind in [Kind::Copy, Kind::Move] {
        let fixture = Fixture::new("source-replaced");
        let source = fixture.0.join("item");
        let original = fixture.0.join("original");
        let dest = fixture.0.join("destination");
        fs::create_dir(&dest).unwrap();
        fs::write(&source, b"approved source").unwrap();
        fs::write(dest.join("item"), b"existing destination").unwrap();
        let (tx, rx) = async_channel::unbounded();
        let (dec_tx, dec_rx) = std::sync::mpsc::channel();
        let (worker_source, worker_dest) = (source.clone(), dest.clone());
        let worker = std::thread::spawn(move || {
            run(
                kind,
                vec![worker_source],
                worker_dest,
                Arc::new(AtomicBool::new(false)),
                tx,
                dec_rx,
            )
        });
        let mut finished = false;
        while let Ok(event) = rx.recv_blocking() {
            match event {
                Event::Conflict { .. } => {
                    fs::rename(&source, &original).unwrap();
                    fs::write(&source, b"new replacement").unwrap();
                    dec_tx.send(Decision::Replace).unwrap();
                }
                Event::Finished { ok, failed, .. } => {
                    assert_eq!((ok, failed), (0, 1));
                    finished = true;
                    break;
                }
                _ => {}
            }
        }
        worker.join().unwrap();
        assert!(finished);
        assert_eq!(fs::read(original).unwrap(), b"approved source");
        assert_eq!(fs::read(source).unwrap(), b"new replacement");
        assert_eq!(
            fs::read(dest.join("item")).unwrap(),
            b"existing destination"
        );
    }
}

#[test]
fn source_commit_wait_cannot_take_a_replacement() {
    let fixture = Fixture::new("source-replaced");
    let src = fixture.0.join("source");
    let dst = fixture.0.join("destination");
    fs::write(&src, b"approved").unwrap();
    let approved = Identity::read(&src).unwrap();
    fs::rename(&src, fixture.0.join("original")).unwrap();
    fs::write(&src, b"replacement").unwrap();
    let (tx, _rx) = async_channel::unbounded();
    let cancel = AtomicBool::new(false);
    let mut prog = move_progress(&tx, &cancel);
    assert!(move_entry(&src, &dst, 8, 1, &mut prog, &approved).is_err());
    assert_eq!(fs::read(src).unwrap(), b"replacement");
    assert!(!dst.exists());
}
#[test]
fn symlink_copy_reports_complete_progress() {
    let fixture = Fixture::new("link-progress");
    let source = fixture.0.join("links");
    let dest = fixture.0.join("destination");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&dest).unwrap();
    for index in 0..100 {
        std::os::unix::fs::symlink("target-name", source.join(format!("link-{index}"))).unwrap();
    }
    let events = execute(Kind::Copy, vec![source], dest, Decision::Rename);
    let total = events
        .iter()
        .find_map(|event| match event {
            Event::Total { bytes, items, .. } => Some((*bytes, *items)),
            _ => None,
        })
        .unwrap();
    let last = events
        .iter()
        .filter_map(|event| match event {
            Event::Advance {
                bytes_done,
                items_done,
                ..
            } => Some((*bytes_done, *items_done)),
            _ => None,
        })
        .last()
        .unwrap();
    assert_eq!(last, total);
}

fn move_progress<'a>(tx: &'a Sender<Event>, cancel: &'a AtomicBool) -> Prog<'a> {
    Prog {
        tx,
        cancel,
        bytes_done: 0,
        items_done: 0,
        current: "source".into(),
        capture: true,
        copied: Vec::new(),
        destinations: Vec::new(),
        last_tick: Cell::new(None),
        pause: None,
    }
}


#[test]
fn source_change_after_commit_is_not_removed() {
    let fixture = Fixture::new("changed-source");
    let src = fixture.0.join("source");
    let dst = fixture.0.join("destination");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("original"), b"copied").unwrap();
    let (tx, _rx) = async_channel::unbounded();
    let cancel = AtomicBool::new(false);
    let mut prog = move_progress(&tx, &cancel);
    let approved = Identity::read(&src).unwrap();
    copy_tree(&src, &dst, &mut prog, Some(&approved)).unwrap();
    fs::write(src.join("new-after-copy"), b"must survive").unwrap();
    assert!(remove_copied(&mut prog).is_err());
    assert_eq!(fs::read(src.join("original")).unwrap(), b"copied");
    assert_eq!(
        fs::read(src.join("new-after-copy")).unwrap(),
        b"must survive"
    );
    assert!(!dst.join("new-after-copy").exists());
}

#[test]
fn copy_into_own_descendant_is_rejected() {
    let fixture = Fixture::new("descendant");
    let src = fixture.0.join("source");
    let dest = src.join("nested");
    fs::create_dir_all(&dest).unwrap();
    fs::write(src.join("original"), b"keep").unwrap();
    let events = execute(
        Kind::Copy,
        vec![src.clone()],
        dest.clone(),
        Decision::Rename,
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Finished { failed: 1, .. }))
    );
    assert!(!dest.join("source").exists());
    assert_eq!(fs::read(src.join("original")).unwrap(), b"keep");
}

#[test]
fn keep_both_preserves_raw_names_and_dangling_links() {
    let fixture = Fixture::new("raw-conflict");
    let name = std::ffi::OsString::from_vec(b"report-\xff.txt".to_vec());
    let src = fixture.0.join(&name);
    let dest = fixture.0.join("destination");
    fs::create_dir(&dest).unwrap();
    fs::write(&src, b"new").unwrap();
    std::os::unix::fs::symlink("unavailable", dest.join(&name)).unwrap();
    execute(Kind::Copy, vec![src], dest.clone(), Decision::Rename);
    let expected = std::ffi::OsString::from_vec(b"report-\xff (copy).txt".to_vec());
    assert_eq!(fs::read(dest.join(expected)).unwrap(), b"new");
    assert!(
        fs::symlink_metadata(dest.join(name))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn transfer_retry_keeps_completed_results_and_rechecks_sources() {
    let fixture = Fixture::new("retry");
    let good = fixture.0.join("good");
    let missing = fixture.0.join("missing");
    let dest = fixture.0.join("destination");
    fs::create_dir(&dest).unwrap();
    fs::write(&good, b"completed").unwrap();
    let events = execute(
        Kind::Copy,
        vec![good.clone(), missing],
        dest.clone(),
        Decision::Cancel,
    );
    let failures = events
        .into_iter()
        .find_map(|event| match event {
            Event::Finished { ok, failures, .. } => {
                assert_eq!(ok, 1);
                Some(failures)
            }
            _ => None,
        })
        .unwrap();
    fs::write(dest.join("good"), b"edited afterward").unwrap();
    let (tx, rx) = async_channel::unbounded();
    let (_dec_tx, dec_rx) = std::sync::mpsc::channel();
    retry(
        Kind::Copy,
        failures,
        dest.clone(),
        Arc::new(AtomicBool::new(false)),
        tx,
        dec_rx,
    );
    assert_eq!(fs::read(dest.join("good")).unwrap(), b"edited afterward");
    let mut finished = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(
            event,
            Event::Finished {
                ok: 0,
                failed: 1,
                ..
            }
        ) {
            finished = true;
        }
    }
    assert!(finished);
}

#[test]
fn special_files_are_rejected_without_opening_them() {
    let fixture = Fixture::new("special");
    let src = fixture.0.join("socket");
    let _socket = std::os::unix::net::UnixListener::bind(&src).unwrap();
    let dest = fixture.0.join("destination");
    fs::create_dir(&dest).unwrap();
    let events = execute(Kind::Copy, vec![src], dest.clone(), Decision::Cancel);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Finished { failed: 1, .. }))
    );
    assert!(!dest.join("socket").exists());
}

struct Fixture(PathBuf);
impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "filescope-worker-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn execute(kind: Kind, sources: Vec<PathBuf>, dest: PathBuf, decision: Decision) -> Vec<Event> {
    let (tx, rx) = async_channel::unbounded();
    let (dec_tx, dec_rx) = std::sync::mpsc::channel();
    dec_tx.send(decision).unwrap();
    run(
        kind,
        sources,
        dest,
        Arc::new(AtomicBool::new(false)),
        tx,
        dec_rx,
    );
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[test]
fn transfer_preserves_raw_source_name() {
    let fixture = Fixture::new("raw");
    let name = std::ffi::OsString::from_vec(b"report-\xff.txt".to_vec());
    let src = fixture.0.join(&name);
    let dest = fixture.0.join("destination");
    fs::create_dir(&dest).unwrap();
    fs::write(&src, b"exact identity").unwrap();
    execute(
        Kind::Copy,
        vec![src.clone()],
        dest.clone(),
        Decision::Rename,
    );
    assert_eq!(fs::read(dest.join(name)).unwrap(), b"exact identity");
    assert_eq!(fs::read(src).unwrap(), b"exact identity");
}

#[test]
fn transfer_replace_never_deletes_its_own_source() {
    let fixture = Fixture::new("self-replace");
    let src = fixture.0.join("same.txt");
    fs::write(&src, b"the original must survive").unwrap();
    let events = execute(
        Kind::Copy,
        vec![src.clone()],
        fixture.0.clone(),
        Decision::Replace,
    );
    assert_eq!(fs::read(src).unwrap(), b"the original must survive");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Finished { failed: 1, .. }))
    );
}

#[test]
fn partial_copy_keeps_private_permissions() {
    const MARKER: &str = "FILESCOPE_PARTIAL_WORKER_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ops::regression_tests::partial_copy_keeps_private_permissions",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let fixture = Fixture::new("private-partial");
    let src = fixture.0.join("secret");
    let dest = fixture.0.join("destination");
    fs::create_dir(&dest).unwrap();
    fs::write(&src, vec![42u8; 8192]).unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o600)).unwrap();
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        let limit = libc::rlimit {
            rlim_cur: 1024,
            rlim_max: 1024,
        };
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
    }
    let events = execute(Kind::Copy, vec![src], dest.clone(), Decision::Cancel);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Event::Finished { failed: 1, .. }))
    );
    assert_eq!(
        fs::metadata(dest.join("secret"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
