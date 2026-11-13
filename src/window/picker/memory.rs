//! Per-caller recent folders; only successful requests record a location.
use gtk::prelude::*;
use gtk::{gio, glib};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const RECENT_LIMIT: usize = 10;
type Store = BTreeMap<String, Vec<String>>;
struct Cache {
    store: Option<Store>,
    revision: u64,
}
static CACHE: Mutex<Cache> = Mutex::new(Cache {
    store: None,
    revision: 0,
});
static WRITER: Mutex<()> = Mutex::new(());

pub(super) struct PendingWrite {
    revision: u64,
    store: Store,
    path: PathBuf,
}

fn file() -> PathBuf {
    glib::user_config_dir().join("filescope/picker-locations.json")
}

fn read(path: &Path) -> Result<Store, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid picker folder memory: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Store::new()),
        Err(error) => Err(error.to_string()),
    }
}

fn identified(app_id: &str) -> bool {
    !app_id.trim().is_empty()
}

pub(super) fn recent(app_id: &str) -> Vec<PathBuf> {
    if !identified(app_id) {
        return Vec::new();
    }
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    if cache.store.is_none() {
        match read(&file()) {
            Ok(store) => cache.store = Some(store),
            Err(error) => {
                eprintln!("filescope: {error}");
                return Vec::new();
            }
        }
    }
    cache
        .store
        .as_ref()
        .unwrap()
        .get(app_id)
        .into_iter()
        .flatten()
        .filter_map(|uri| {
            crate::path_completion::resolve(uri, &gio::File::for_path(glib::home_dir()))
                .and_then(|file| file.path())
        })
        .filter(|path| path.is_absolute())
        .take(RECENT_LIMIT)
        .collect()
}

fn record(store: &mut Store, app_id: &str, folder: &Path) {
    let uri = gio::File::for_path(folder).uri().to_string();
    let recent = store.entry(app_id.to_owned()).or_default();
    recent.retain(|old| old != &uri);
    recent.insert(0, uri);
    recent.truncate(RECENT_LIMIT);
}

pub(super) fn remember(app_id: &str, folder: &Path) -> Result<Option<PendingWrite>, String> {
    if !identified(app_id) {
        return Ok(None);
    }
    if !folder.is_absolute() {
        return Err("The accepted folder must be absolute".into());
    }
    let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
    let store = cache
        .store
        .as_mut()
        .ok_or("Picker folder memory was not loaded; its file was kept unchanged")?;
    record(store, app_id, folder);
    let store = store.clone();
    cache.revision = cache.revision.wrapping_add(1);
    Ok(Some(PendingWrite {
        revision: cache.revision,
        store,
        path: file(),
    }))
}

pub(super) fn persist(write: PendingWrite) -> Result<(), String> {
    let _writer = WRITER.lock().unwrap_or_else(|error| error.into_inner());
    if CACHE
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .revision
        != write.revision
    {
        return Ok(());
    }
    let PendingWrite { store, path, .. } = write;
    let parent = path.parent().unwrap();
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".picker-locations-{}", glib::uuid_string_random()));
    let result = (|| -> Result<(), String> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        output
            .write_all(&serde_json::to_vec(&store).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        output.sync_all().map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, &path).map_err(|error| error.to_string())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn picker_folder_memory_keeps_exact_paths_and_caller_scopes() {
        let root = std::env::temp_dir().join(format!(
            "filescope-picker-memory-{}",
            glib::uuid_string_random()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let raw = root.join(std::ffi::OsString::from_vec(b"folder-\xff".to_vec()));
        let mut store = Store::new();
        record(&mut store, "one", &raw);
        record(&mut store, "two", &root);
        let path = root.join("memory.json");
        std::fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();
        let loaded = read(&path).unwrap();
        assert_eq!(gio::File::for_uri(&loaded["one"][0]).path(), Some(raw));
        assert_eq!(loaded["two"].len(), 1);
        for i in 0..12 {
            record(&mut store, "one", &root.join(format!("recent{i}")));
        }
        assert_eq!(store["one"].len(), RECENT_LIMIT);
        assert!(store["one"][0].ends_with("recent11"));
        assert!(!identified(" "));
        assert!(!identified(""));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn picker_folder_memory_does_not_persist_an_older_acceptance_last() {
        let root = std::env::temp_dir().join(format!(
            "filescope-picker-memory-order-{}",
            glib::uuid_string_random()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("memory.json");
        let mut old = Store::new();
        record(&mut old, "app", &root.join("old"));
        let mut new = old.clone();
        record(&mut new, "app", &root.join("new"));
        let previous = {
            let mut cache = CACHE.lock().unwrap_or_else(|error| error.into_inner());
            std::mem::replace(
                &mut *cache,
                Cache {
                    store: Some(new.clone()),
                    revision: 2,
                },
            )
        };
        persist(PendingWrite {
            revision: 2,
            store: new,
            path: path.clone(),
        })
        .unwrap();
        persist(PendingWrite {
            revision: 1,
            store: old,
            path: path.clone(),
        })
        .unwrap();
        assert!(read(&path).unwrap()["app"][0].ends_with("/new"));
        *CACHE.lock().unwrap_or_else(|error| error.into_inner()) = previous;
        std::fs::remove_dir_all(root).unwrap();
    }
}
