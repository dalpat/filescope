// GNOME Overview search. No windows or filesystem work in search dispatch.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gtk::prelude::*;
use gtk::{gio, glib};

mod application;
mod index;
pub(crate) use application::new as new_application;

const MAX_HITS: usize = 100;
const MAX_SCAN_HITS: usize = 20;
const MAX_ENTRIES: usize = 10_000;
const MAX_DEPTH: usize = 4;

fn search(
    home: &Path,
    terms: &[String],
    cancel: &AtomicBool,
    limit: usize,
    expired: impl Fn() -> bool,
) -> Vec<String> {
    let terms = normalized_terms(terms);
    if terms.is_empty() || cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let stopped = || cancel.load(Ordering::Relaxed) || expired();
    if stopped() {
        return Vec::new();
    }
    let Ok(home) = home.canonicalize() else {
        return Vec::new();
    };
    let mut pending = VecDeque::from([(home.clone(), 0)]);
    let mut remaining = limit;
    let mut hits = Vec::new();
    while let Some((directory, depth)) = pending.pop_front() {
        if remaining == 0 || stopped() {
            break;
        }
        // Recheck queued directories: never descend into a directory symlink.
        let Ok(meta) = std::fs::symlink_metadata(&directory) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        if stopped() {
            break;
        }
        let Ok(resolved) = directory.canonicalize() else {
            continue;
        };
        if !resolved.starts_with(&home) {
            continue;
        }
        if stopped() {
            break;
        }
        let Ok(mut entries) = std::fs::read_dir(directory) else {
            continue;
        };
        loop {
            if remaining == 0 || stopped() {
                return hits;
            }
            let Some(entry) = entries.next() else { break };
            remaining -= 1;
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if stopped() {
                return hits;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if name_matches(&name, &terms) && eligible(&home, &path, &stopped) {
                hits.push(gio::File::for_path(&path).uri().to_string());
                if hits.len() == MAX_SCAN_HITS {
                    return hits;
                }
            }
            if kind.is_dir() && depth + 1 < MAX_DEPTH {
                pending.push_back((path, depth + 1));
            }
        }
    }
    hits
}

fn subsearch(
    home: &Path,
    previous: &[String],
    terms: &[String],
    cancel: &AtomicBool,
) -> Vec<String> {
    validated_results(home, previous, terms, cancel, MAX_HITS)
}

fn validated_results(
    home: &Path,
    previous: &[String],
    terms: &[String],
    cancel: &AtomicBool,
    limit: usize,
) -> Vec<String> {
    let terms = normalized_terms(terms);
    if terms.is_empty() || cancel.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let Ok(home) = home.canonicalize() else {
        return Vec::new();
    };
    previous
        .iter()
        .take(MAX_HITS * 2)
        .take_while(|_| !cancel.load(Ordering::Relaxed))
        .filter(|uri| {
            local_path(uri).is_some_and(|path| {
                path.file_name()
                    .is_some_and(|name| name_matches(&name.to_string_lossy(), &terms))
                    && eligible(&home, &path, &|| cancel.load(Ordering::Relaxed))
            })
        })
        .take(limit)
        .cloned()
        .collect()
}

#[derive(Default)]
pub(crate) struct SearchPage {
    pub ids: Vec<String>,
    pub next_offset: Option<usize>,
    pub limited: bool,
}

/// In-window search uses the same ordering as Overview and can request more pages.
pub(crate) async fn search_page(
    root: PathBuf,
    terms: Vec<String>,
    cancel: Arc<AtomicBool>,
    offset: usize,
) -> SearchPage {
    if cancel.load(Ordering::Relaxed) || normalized_terms(&terms).is_empty() {
        return SearchPage::default();
    }
    // The index stores target paths, while a tab may have been opened via a symlink.
    let root = gio::spawn_blocking(move || root.canonicalize())
        .await
        .ok()
        .and_then(|result| result.ok());
    let Some(root) = root else {
        return SearchPage::default();
    };
    let indexed = index::page(&root, &terms, cancel.clone(), offset).await;
    let next_offset = indexed
        .as_ref()
        .filter(|ids| ids.len() == MAX_HITS * 2)
        .map(|ids| offset + ids.len());
    let limited = indexed.is_none();
    let ids = gio::spawn_blocking(move || {
        if let Some(ids) = indexed {
            validated_results(&root, &ids, &terms, &cancel, MAX_HITS * 2)
        } else if offset == 0 {
            let started = std::time::Instant::now();
            search(&root, &terms, &cancel, MAX_ENTRIES, || {
                started.elapsed() >= std::time::Duration::from_millis(250)
            })
        } else {
            Vec::new()
        }
    })
    .await
    .unwrap_or_default();
    SearchPage {
        ids,
        next_offset,
        limited,
    }
}

fn activate_target(uri: &str) -> Option<crate::file_manager::Target> {
    let path = local_path(uri)?;
    Some(crate::file_manager::Target {
        dir: path.parent()?.to_path_buf(),
        names: vec![path.file_name()?.to_string_lossy().into_owned()],
        properties: false,
    })
}

fn result_metas(ids: &[String]) -> Vec<HashMap<String, glib::Variant>> {
    ids.iter()
        .take(MAX_HITS)
        .map(|uri| {
            let path = local_path(uri);
            let info = path.as_ref().and_then(|path| {
                gio::File::for_path(path)
                    .query_info(
                        "standard::icon,standard::display-name",
                        gio::FileQueryInfoFlags::NONE,
                        gio::Cancellable::NONE,
                    )
                    .ok()
            });
            let name = info
                .as_ref()
                .map(|info| info.display_name().to_string())
                .or_else(|| {
                    path.as_ref()
                        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                })
                .unwrap_or_else(|| uri.clone());
            let parent = path
                .as_ref()
                .and_then(|p| p.parent())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let icon = info.and_then(|info| info.icon()).unwrap_or_else(|| {
                let (content_type, _) = gio::content_type_guess(path.as_deref(), &[]);
                gio::content_type_get_icon(&content_type)
            });
            let mut meta = HashMap::from([
                ("id".into(), uri.to_variant()),
                ("name".into(), name.to_variant()),
                ("description".into(), parent.to_variant()),
            ]);
            if let Some(icon) = icon.serialize() {
                meta.insert("icon".into(), icon);
            }
            meta
        })
        .collect()
}

fn normalized_terms(terms: &[String]) -> Vec<String> {
    terms
        .iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn name_matches(name: &str, terms: &[String]) -> bool {
    let name = name.to_lowercase();
    terms.iter().all(|term| name.contains(term))
}

fn local_path(uri: &str) -> Option<PathBuf> {
    let (path, host) = glib::filename_from_uri(uri).ok()?;
    if host.is_some_and(|host| host.as_str() != "localhost") {
        return None;
    }
    Some(path)
}

fn eligible(home: &Path, path: &Path, stopped: &impl Fn() -> bool) -> bool {
    let Ok(relative) = path.strip_prefix(home) else {
        return false;
    };
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() || parts.iter().any(|part| {
        !matches!(part, Component::Normal(name) if !name.to_string_lossy().starts_with('.'))
    }) { return false; }
    // A supplied subsearch id must obey the same no-symlink-directory rule.
    for parent in path.ancestors().skip(1).take_while(|p| *p != home) {
        if stopped() {
            return false;
        }
        if !std::fs::symlink_metadata(parent).is_ok_and(|m| m.is_dir()) {
            return false;
        }
    }
    if stopped() {
        return false;
    }
    let Ok(resolved) = path.canonicalize() else {
        return false;
    };
    resolved.starts_with(home)
        && !stopped()
        && std::fs::metadata(resolved).is_ok_and(|m| m.is_file() || m.is_dir())
}

enum SearchRequest {
    Initial(Vec<String>),
    Subsearch(Vec<String>, Vec<String>),
}

struct Job {
    id: u64,
    request: SearchRequest,
    cancel: Arc<AtomicBool>,
}

struct Active<R> {
    id: u64,
    cancel: Arc<AtomicBool>,
    reply: Option<R>,
}

struct SearchQueue<R> {
    serial: u64,
    active: Option<Active<R>>,
    pending: Option<(SearchRequest, R)>,
}

impl<R> SearchQueue<R> {
    fn new() -> Self {
        Self {
            serial: 0,
            active: None,
            pending: None,
        }
    }

    fn submit(&mut self, request: SearchRequest, reply: R) -> Vec<R> {
        let mut superseded = Vec::new();
        if let Some(active) = &mut self.active {
            active.cancel.store(true, Ordering::Relaxed);
            if let Some(reply) = active.reply.take() {
                superseded.push(reply);
            }
        }
        if let Some((_, reply)) = self.pending.take() {
            superseded.push(reply);
        }
        self.pending = Some((request, reply));
        superseded
    }

    fn start(&mut self) -> Option<Job> {
        if self.active.is_some() {
            return None;
        }
        let (request, reply) = self.pending.take()?;
        self.serial = self.serial.wrapping_add(1);
        let cancel = Arc::new(AtomicBool::new(false));
        self.active = Some(Active {
            id: self.serial,
            cancel: cancel.clone(),
            reply: Some(reply),
        });
        Some(Job {
            id: self.serial,
            request,
            cancel,
        })
    }

    fn finish(&mut self, id: u64) -> Option<R> {
        if !self.active.as_ref().is_some_and(|active| active.id == id) {
            return None;
        }
        self.active.take()?.reply
    }
}

const OBJECT_PATH: &str = "/dev/filescope/SearchProvider";
const INTERFACE: &str = "org.gnome.Shell.SearchProvider2";
const XML: &str = r#"<node><interface name="org.gnome.Shell.SearchProvider2">
  <method name="GetInitialResultSet"><arg type="as" direction="in"/><arg type="as" direction="out"/></method>
  <method name="GetSubsearchResultSet"><arg type="as" direction="in"/><arg type="as" direction="in"/><arg type="as" direction="out"/></method>
  <method name="GetResultMetas"><arg type="as" direction="in"/><arg type="aa{sv}" direction="out"/></method>
  <method name="ActivateResult"><arg type="s" direction="in"/><arg type="as" direction="in"/><arg type="u" direction="in"/></method>
  <method name="LaunchSearch"><arg type="as" direction="in"/><arg type="u" direction="in"/></method>
</interface></node>"#;

struct Reply {
    invocation: gio::DBusMethodInvocation,
    _hold: gio::ApplicationHoldGuard,
}

impl Reply {
    fn results(self, ids: Vec<String>) {
        self.invocation.return_value(Some(&(ids,).to_variant()));
    }
}

struct Provider {
    home: PathBuf,
    queue: RefCell<SearchQueue<Reply>>,
}

impl Provider {
    fn submit(self: &Rc<Self>, request: SearchRequest, reply: Reply) {
        let superseded = self.queue.borrow_mut().submit(request, reply);
        for reply in superseded {
            reply.results(Vec::new());
        }
        self.start_pending();
    }

    fn start_pending(self: &Rc<Self>) {
        let Some(job) = self.queue.borrow_mut().start() else {
            return;
        };
        let provider = self.clone();
        let home = self.home.clone();
        glib::spawn_future_local(async move {
            let id = job.id;
            let terms = match &job.request {
                SearchRequest::Initial(terms) | SearchRequest::Subsearch(_, terms) => terms,
            };
            // Refinements query the index again so an early result cap cannot
            // hide a better match as the user keeps typing.
            let indexed = index::search(&home, terms, job.cancel.clone()).await;
            let result = gio::spawn_blocking(move || {
                if let Some(ids) = indexed {
                    let terms = match &job.request {
                        SearchRequest::Initial(terms) | SearchRequest::Subsearch(_, terms) => terms,
                    };
                    return subsearch(&home, &ids, terms, &job.cancel);
                }
                match job.request {
                    SearchRequest::Initial(terms) => {
                        let started = std::time::Instant::now();
                        search(&home, &terms, &job.cancel, MAX_ENTRIES, || {
                            started.elapsed() >= std::time::Duration::from_millis(250)
                        })
                    }
                    SearchRequest::Subsearch(previous, terms) => {
                        subsearch(&home, &previous, &terms, &job.cancel)
                    }
                }
            })
            .await
            .unwrap_or_default();
            let reply = provider.queue.borrow_mut().finish(id);
            if let Some(reply) = reply {
                reply.results(result);
            }
            provider.start_pending();
        });
    }
}

fn register(
    app: &adw::Application,
    bus: &gio::DBusConnection,
) -> Result<gio::RegistrationId, glib::Error> {
    let node = gio::DBusNodeInfo::for_xml(XML)?;
    let interface = node
        .lookup_interface(INTERFACE)
        .expect("search interface is in XML");
    let app = app.downgrade();
    let provider = Rc::new(Provider {
        home: glib::home_dir(),
        queue: RefCell::new(SearchQueue::new()),
    });
    bus.register_object(OBJECT_PATH, &interface)
        .method_call(move |_, _, _, _, method, params, invocation| {
            let Some(app) = app.upgrade() else {
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.Failed",
                    "Application is shutting down",
                );
                return;
            };
            match method {
                "GetInitialResultSet" => {
                    if let Some((terms,)) = params.get::<(Vec<String>,)>() {
                        provider.submit(
                            SearchRequest::Initial(terms),
                            Reply {
                                invocation,
                                _hold: app.hold(),
                            },
                        );
                        return;
                    }
                }
                "GetSubsearchResultSet" => {
                    if let Some((previous, terms)) = params.get::<(Vec<String>, Vec<String>)>() {
                        provider.submit(
                            SearchRequest::Subsearch(previous, terms),
                            Reply {
                                invocation,
                                _hold: app.hold(),
                            },
                        );
                        return;
                    }
                }
                "GetResultMetas" => {
                    if let Some((ids,)) = params.get::<(Vec<String>,)>() {
                        let hold = app.hold();
                        glib::spawn_future_local(async move {
                            let metas = gio::spawn_blocking(move || result_metas(&ids))
                                .await
                                .unwrap_or_default();
                            invocation.return_value(Some(&(metas,).to_variant()));
                            drop(hold);
                        });
                        return;
                    }
                }
                "ActivateResult" => {
                    if let Some((uri, _, _)) = params.get::<(String, Vec<String>, u32)>() {
                        let hold = app.hold();
                        let home = provider.home.clone();
                        glib::spawn_future_local(async move {
                            let target = gio::spawn_blocking(move || {
                                let home = home.canonicalize().ok()?;
                                let path = local_path(&uri)?;
                                if eligible(&home, &path, &|| false) {
                                    activate_target(&uri)
                                } else {
                                    None
                                }
                            })
                            .await
                            .ok()
                            .flatten();
                            if let Some(target) = target {
                                crate::window::open_targets(&app, &[target]);
                            }
                            invocation.return_value(Some(&().to_variant()));
                            drop(hold);
                        });
                        return;
                    }
                }
                "LaunchSearch" => {
                    if let Some((terms, _)) = params.get::<(Vec<String>, u32)>() {
                        let hold = app.hold();
                        let home = provider.home.clone();
                        glib::idle_add_local_once(move || {
                            crate::window::open_search(
                                &app,
                                &gio::File::for_path(home),
                                &terms.join(" "),
                            );
                            drop(hold);
                        });
                        invocation.return_value(Some(&().to_variant()));
                        return;
                    }
                }
                _ => {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        "Unknown search method",
                    );
                    return;
                }
            }
            invocation.return_dbus_error(
                "org.freedesktop.DBus.Error.InvalidArgs",
                "Invalid search arguments",
            );
        })
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("filescope-search-{}-{name}", std::process::id()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        fn file(&self, name: &str) -> String {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, "fixture").unwrap();
            gio::File::for_path(path).uri().to_string()
        }
        fn search(&self, terms: &[&str]) -> Vec<String> {
            search(
                &self.0,
                &terms.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                &AtomicBool::new(false),
                MAX_ENTRIES,
                || false,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn search_hits_require_every_term() {
        let f = Fixture::new("terms");
        let cat = f.file("Cat.png");
        let catalog = f.file("catalog.txt");
        f.file("dog.png");
        let found = f.search(&["cat"]);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&cat) && found.contains(&catalog));
        assert_eq!(f.search(&["cat", "PNG"]), vec![cat]);
    }

    #[test]
    fn search_skips_hidden_and_stops_at_twenty() {
        let f = Fixture::new("hidden");
        let hidden = f.file(".secret-hit");
        let nested = f.file(".hidden/hit");
        for i in 0..=MAX_SCAN_HITS {
            f.file(&format!("hit-{i}"));
        }
        let hits = f.search(&["hit"]);
        assert_eq!(hits.len(), MAX_SCAN_HITS);
        assert!(!hits.contains(&hidden) && !hits.contains(&nested));
    }

    #[test]
    fn search_does_not_leave_home() {
        let home = Fixture::new("home");
        let outside = Fixture::new("outside");
        outside.file("hit");
        symlink(&outside.0, home.0.join("external-hit")).unwrap();
        symlink(outside.0.join("hit"), home.0.join("file-hit")).unwrap();
        assert!(home.search(&["hit"]).is_empty());
        let real = home.file("nested/hit");
        symlink(home.0.join("nested"), home.0.join("alias")).unwrap();
        assert_eq!(home.search(&["hit"]), vec![real]);
    }

    #[test]
    fn activate_target_is_the_parent_and_the_file_name() {
        let target = activate_target("file:///home/a/b/c%20d.png").unwrap();
        assert_eq!(target.dir, PathBuf::from("/home/a/b"));
        assert_eq!(target.names, ["c d.png"]);
        assert!(!target.properties);
        assert!(activate_target("https://example.com/a").is_none());
        assert!(activate_target("file://other-machine/home/a").is_none());
    }

    #[test]
    fn search_stops_without_matches() {
        let f = Fixture::new("budget");
        for i in 0..50 {
            f.file(&format!("file-{i}"));
        }
        let checks = Cell::new(0);
        let terms = vec!["absent".into()];
        assert!(
            search(&f.0, &terms, &AtomicBool::new(false), 1, || {
                checks.set(checks.get() + 1);
                false
            })
            .is_empty()
        );
        assert!(checks.get() <= 10, "entry budget did not stop the walk");
        let checks = Cell::new(0);
        assert!(
            search(&f.0, &terms, &AtomicBool::new(false), MAX_ENTRIES, || {
                checks.set(checks.get() + 1);
                checks.get() >= 3
            })
            .is_empty()
        );
        assert_eq!(checks.get(), 3);
        assert!(
            search(
                &f.0,
                &terms,
                &AtomicBool::new(true),
                MAX_ENTRIES,
                || panic!("cancelled walk continued")
            )
            .is_empty()
        );
    }

    #[test]
    fn search_obeys_depth_and_empty_terms() {
        let f = Fixture::new("depth");
        let allowed = f.file("one/two/three/hit");
        f.file("one/two/three/four/hit");
        assert_eq!(f.search(&["hit"]), vec![allowed]);
        assert!(
            search(
                &f.0,
                &["  ".into()],
                &AtomicBool::new(false),
                MAX_ENTRIES,
                || panic!("empty query walked")
            )
            .is_empty()
        );
    }

    #[test]
    fn subsearch_filters_previous_ids_only() {
        let f = Fixture::new("subsearch");
        let cat = f.file("Cat.png");
        let catalog = f.file("catalog.txt");
        let removed = f.file("cat-removed.png");
        fs::remove_file(f.0.join("cat-removed.png")).unwrap();
        f.file("cat-new.png");
        let hidden = f.file(".cat-hidden.png");
        assert_eq!(
            subsearch(
                &f.0,
                &[cat.clone(), catalog, removed, hidden],
                &["cat".into(), "png".into()],
                &AtomicBool::new(false)
            ),
            vec![cat]
        );
    }

    #[test]
    fn new_search_supersedes_old_work() {
        let mut queue = SearchQueue::new();
        assert!(
            queue
                .submit(SearchRequest::Initial(vec!["a".into()]), 1)
                .is_empty()
        );
        let first = queue.start().unwrap();
        assert_eq!(
            queue.submit(SearchRequest::Initial(vec!["ab".into()]), 2),
            vec![1]
        );
        assert!(first.cancel.load(Ordering::Relaxed));
        assert!(queue.start().is_none());
        assert_eq!(
            queue.submit(SearchRequest::Initial(vec!["abc".into()]), 3),
            vec![2]
        );
        assert!(queue.finish(first.id).is_none());
        let latest = queue.start().unwrap();
        assert!(matches!(latest.request, SearchRequest::Initial(ref terms) if terms == &["abc"]));
        assert!(queue.finish(first.id).is_none());
        assert_eq!(queue.finish(latest.id), Some(3));
        assert!(queue.start().is_none());
    }

    #[test]
    fn result_metas_include_id_and_name() {
        let ids = vec![
            "file:///home/a/my%20file.txt".into(),
            "file:///home/a/missing.pdf".into(),
        ];
        let metas = result_metas(&ids);
        assert_eq!(metas.len(), 2);
        for (meta, id) in metas.iter().zip(&ids) {
            assert_eq!(meta["id"].get::<String>().as_ref(), Some(id));
            assert_eq!(meta["description"].str(), Some("/home/a"));
        }
        assert_eq!(metas[0]["name"].str(), Some("my file.txt"));
        assert_eq!(metas[1]["name"].str(), Some("missing.pdf"));
        assert_eq!((metas,).to_variant().type_().as_str(), "(aa{sv})");
    }

    #[test]
    fn metadata_uses_folder_icon_and_readable_parent() {
        let f = Fixture::new("metadata");
        let folder = f.0.join("my folder");
        fs::create_dir(&folder).unwrap();
        let ids = vec![
            gio::File::for_path(&folder).uri().to_string(),
            f.file("photo.png"),
        ];
        let metas = result_metas(&ids);
        assert_eq!(metas[0]["description"].str(), f.0.to_str());
        let icon = gio::Icon::deserialize(&metas[0]["icon"]).unwrap();
        assert!(icon.to_string().unwrap().contains("folder"));
        let icon = gio::Icon::deserialize(&metas[1]["icon"]).unwrap();
        assert!(icon.to_string().unwrap().contains("image"));
    }

    #[test]
    fn indexed_results_can_be_deep_but_not_hidden_or_external() {
        let f = Fixture::new("indexed-depth");
        let deep = f.file("one/two/three/four/five/hit");
        assert!(eligible(&f.0, &local_path(&deep).unwrap(), &|| false));
        let hidden = f.file("one/.hidden/hit");
        assert!(!eligible(&f.0, &local_path(&hidden).unwrap(), &|| false));
        assert!(!eligible(&f.0, &f.0.join("../external"), &|| false));
    }
}
