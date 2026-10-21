// Query GNOME's existing index; no second crawler or database to maintain.
use super::*;
use std::ffi::OsStr;
use std::time::{Duration, Instant};

fn query_args(home: &Path, terms: &[String]) -> Vec<String> {
    let filters = (0..terms.len())
        .map(|i| format!("CONTAINS(?lower, ~term{i})"))
        .collect::<Vec<_>>()
        .join(" && ");
    let query = format!(
        "SELECT ?url WHERE {{ GRAPH tracker:FileSystem {{
           ?file a nfo:FileDataObject ; nie:url ?url ; nfo:fileName ?name .
           BIND(LCASE(?name) AS ?lower)
           BIND(EXISTS {{ ?file nie:interpretedAs/a nfo:Folder }} AS ?folder)
           FILTER({filters} && STRSTARTS(?url, ~home))
           FILTER(!REGEX(SUBSTR(?url, STRLEN(~home) + 1), '(^|/)[.]'))
         }} }}
         ORDER BY DESC(?lower = ~phrase)
                  ASC(REGEX(?url, '/(node_modules|__pycache__)/'))
                  DESC(?folder) DESC(STRSTARTS(?lower, ~phrase))
                  ASC(STRLEN(?lower)) ASC(?url)
         LIMIT {}",
        MAX_HITS * 2
    );
    // Bind all user input, including home: quotes and SPARQL syntax stay literal.
    let mut args = vec!["--query".into(), query];
    for (i, term) in terms.iter().enumerate() {
        args.extend(["--arg".into(), format!("term{i}:s:{term}")]);
    }
    args.extend([
        "--arg".into(),
        format!("phrase:s:{}", terms.join(" ")),
        "--arg".into(),
        format!(
            "home:s:{}/",
            gio::File::for_path(home).uri().trim_end_matches('/')
        ),
    ]);
    args
}

async fn run_query(args: &[String], cancel: Arc<AtomicBool>, timeout: Duration) -> Option<String> {
    let argv: Vec<_> = args.iter().map(OsStr::new).collect();
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE,
    )
    .ok()?;
    let child = process.clone();
    let started = Instant::now();
    // Killing and awaiting the child also drains its pipes and reaps it.
    let watch = glib::timeout_add_local(Duration::from_millis(20), move || {
        if cancel.load(Ordering::Relaxed) || started.elapsed() >= timeout {
            child.force_exit();
        }
        glib::ControlFlow::Continue
    });
    let output = process.communicate_utf8_future(None).await;
    watch.remove();
    if !process.is_successful() {
        return None;
    }
    output.ok()?.0.map(|s| s.to_string())
}

pub(super) async fn search(
    home: &Path,
    terms: &[String],
    cancel: Arc<AtomicBool>,
) -> Option<Vec<String>> {
    page(home, terms, cancel, 0).await
}

pub(super) async fn page(
    home: &Path,
    terms: &[String],
    cancel: Arc<AtomicBool>,
    offset: usize,
) -> Option<Vec<String>> {
    let terms = normalized_terms(terms);
    if terms.is_empty() || cancel.load(Ordering::Relaxed) {
        return Some(Vec::new());
    }
    let (program, command, service) = if let Some(path) = glib::find_program_in_path("tinysparql") {
        (path, "query", "org.freedesktop.LocalSearch3")
    } else {
        (
            glib::find_program_in_path("tracker3")?,
            "sparql",
            "org.freedesktop.Tracker3.Miner.Files",
        )
    };
    let mut args = vec![
        program.to_string_lossy().into_owned(),
        command.into(),
        "--dbus-service".into(),
        service.into(),
    ];
    let mut query = query_args(home, &terms);
    query[1].push_str(&format!(" OFFSET {offset}"));
    args.extend(query);
    let output = run_query(&args, cancel, Duration::from_secs(2)).await?;
    // The query selects only URIs. Ignore the CLI's heading and blank lines;
    // URI escaping preserves spaces, commas, and newlines in filenames.
    Some(
        output
            .lines()
            .map(str::trim)
            .filter(|line| local_path(line).is_some())
            .take(MAX_HITS * 2)
            .map(str::to_owned)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_and_home_are_bound_not_interpolated() {
        let term = "quote'\"\\\n } UNION { ?s ?p ?o }".to_string();
        let args = query_args(Path::new("/home/some one"), std::slice::from_ref(&term));
        assert!(!args[1].contains(&term));
        assert!(args.contains(&format!("term0:s:{term}")));
        assert!(args.contains(&"home:s:file:///home/some%20one/".into()));
    }

    #[test]
    fn query_process_is_killed_on_timeout_and_cancellation() {
        let context = glib::MainContext::default();
        context
            .with_thread_default(|| {
                context.block_on(async {
                    for (cancelled, timeout) in [
                        (false, Duration::from_millis(40)),
                        (true, Duration::from_secs(2)),
                    ] {
                        let started = Instant::now();
                        assert!(
                            run_query(
                                &["/bin/sleep".into(), "10".into()],
                                Arc::new(AtomicBool::new(cancelled)),
                                timeout
                            )
                            .await
                            .is_none()
                        );
                        assert!(started.elapsed() < Duration::from_secs(2));
                    }
                })
            })
            .unwrap();
    }

    #[test]
    #[ignore = "run with FILESCOPE_TEST_INDEX=1 dbus-run-session -- cargo test indexed_query_integration -- --ignored"]
    fn indexed_query_integration() {
        use std::{
            fs,
            process::{Command, Stdio},
        };
        assert_eq!(std::env::var("FILESCOPE_TEST_INDEX").as_deref(), Ok("1"));
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
        let owned = || {
            bus.call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "NameHasOwner",
                Some(&("org.freedesktop.LocalSearch3",).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                1000,
                gio::Cancellable::NONE,
            )
            .unwrap()
            .get::<(bool,)>()
            .unwrap()
            .0
        };
        assert!(
            !owned(),
            "Use a private session bus, never the desktop's index"
        );
        let root =
            std::env::temp_dir().join(format!("filescope-index-test-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let child = Command::new("tinysparql")
            .args([
                "endpoint",
                "--ontology",
                "nepomuk",
                "--dbus-service",
                "org.freedesktop.LocalSearch3",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        struct Cleanup(std::process::Child, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
                let _ = fs::remove_dir_all(&self.1);
            }
        }
        let _cleanup = Cleanup(child, root.clone());
        let started = Instant::now();
        while !owned() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "Test endpoint did not start"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut insert = String::from("INSERT DATA { GRAPH tracker:FileSystem {");
        let mut add = |relative: &str, folder: bool| {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if folder {
                fs::create_dir_all(&path).unwrap();
            } else {
                fs::write(&path, "test").unwrap();
            }
            let uri = gio::File::for_path(&path).uri().to_string();
            let name = path.file_name().unwrap().to_string_lossy();
            insert.push_str(&format!(
                "<{uri}> a nfo:FileDataObject ; nie:url {uri:?} ; nfo:fileName {name:?} . "
            ));
            if folder {
                insert.push_str(&format!(
                    "<{uri}> nie:interpretedAs <{uri}#folder> . <{uri}#folder> a nfo:Folder . "
                ));
            }
            uri
        };
        let folder = add("development", true);
        let file = add("development notes.txt", false);
        add("node_modules/development", true);
        add(".hidden/development", true);
        for i in 0..210 {
            add(&format!("Needle item {i:03}.txt"), false);
        }
        let deep = add(
            "one/two/three/four/five/Needle zebra long enough for refinement.txt",
            false,
        );
        let quoted = add("quoted'\"name.txt", false);
        insert.push_str("} }");
        let query_file = root.join("fixture.rq");
        fs::write(&query_file, insert).unwrap();
        let output = Command::new("tinysparql")
            .args([
                "query",
                "--dbus-service",
                "org.freedesktop.LocalSearch3",
                "--update",
                "--file",
            ])
            .arg(&query_file)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let context = glib::MainContext::default();
        context
            .with_thread_default(|| {
                context.block_on(async {
                    let cancel = Arc::new(AtomicBool::new(false));
                    let hits = search(&root, &["develop".into()], cancel.clone())
                        .await
                        .unwrap();
                    assert_eq!(&hits[..2], &[folder, file.clone()]);
                    assert!(hits.iter().all(|uri| !uri.contains("/.hidden/")));
                    assert_eq!(
                        search(&root, &["development notes.txt".into()], cancel.clone())
                            .await
                            .unwrap(),
                        [file]
                    );
                    let broad = search(&root, &["needle".into()], cancel.clone())
                        .await
                        .unwrap();
                    let alias = root.join("alias");
                    std::os::unix::fs::symlink(&root, &alias).unwrap();
                    let alias_page = super::super::search_page(
                        alias.clone(),
                        vec!["needle".into()],
                        cancel.clone(),
                        0,
                    )
                    .await;
                    assert_eq!(
                        alias_page.ids.len(),
                        broad.len(),
                        "symlink scope lost indexed results"
                    );
                    assert_eq!(alias_page.next_offset, Some(broad.len()));
                    let alias_next = super::super::search_page(
                        alias,
                        vec!["needle".into()],
                        cancel.clone(),
                        broad.len(),
                    )
                    .await;
                    assert!(
                        alias_next.ids.contains(&deep),
                        "symlink scope lost the next page"
                    );
                    assert!(
                        !broad.contains(&deep),
                        "Fixture must exercise the initial cap"
                    );
                    let next = page(&root, &["needle".into()], cancel.clone(), broad.len())
                        .await
                        .unwrap();
                    assert!(next.contains(&deep));
                    assert!(next.iter().all(|id| !broad.contains(id)));
                    let refined = search(&root, &["needle".into(), "zebra".into()], cancel.clone())
                        .await
                        .unwrap();
                    assert_eq!(
                        super::super::subsearch(
                            &root,
                            &refined,
                            &["needle".into(), "zebra".into()],
                            &cancel
                        ),
                        [deep]
                    );
                    assert_eq!(
                        search(&root, &["quoted'\"name".into()], cancel.clone())
                            .await
                            .unwrap(),
                        [quoted]
                    );
                    assert!(
                        search(&root, &["' } UNION { ?s ?p ?o }".into()], cancel)
                            .await
                            .unwrap()
                            .is_empty()
                    );
                })
            })
            .unwrap();
    }
}
