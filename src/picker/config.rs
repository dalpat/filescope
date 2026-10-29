//! Reversible opt-in for the FileChooser portal only.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const CHOOSER: &str = "org.freedesktop.impl.portal.FileChooser";
const BACKEND: &str = "zz-filescope";

struct Paths {
    config: PathBuf,
    state: PathBuf,
    search: Vec<PathBuf>,
    desktops: Vec<String>,
    service: PathBuf,
    descriptor: PathBuf,
}

impl Paths {
    fn user() -> Result<Self, String> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set")?;
        let user_dir = |name: &str, fallback: PathBuf| std::env::var_os(name).map(PathBuf::from)
            .filter(|path| path.is_absolute()).unwrap_or(fallback);
        let system_dirs = |name: &str, fallback: &str| std::env::split_paths(&std::env::var_os(name)
            .unwrap_or_else(|| fallback.into())).filter(|path| path.is_absolute()).collect::<Vec<_>>();
        let config = user_dir("XDG_CONFIG_HOME", home.join(".config"));
        let data = user_dir("XDG_DATA_HOME", home.join(".local/share"));
        let mut roots = vec![config.clone()];
        roots.extend(system_dirs("XDG_CONFIG_DIRS", "/etc/xdg"));
        roots.push("/etc".into());
        roots.push(data.clone());
        roots.extend(system_dirs("XDG_DATA_DIRS", "/usr/local/share:/usr/share"));
        roots.push("/usr/share".into());
        let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().split(':')
            .filter(|name| !name.is_empty() && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'))
            .map(str::to_ascii_lowercase).collect();
        Ok(Self {
            state: config.join("filescope/file-picker.json"),
            config: config.join("xdg-desktop-portal"),
            search: roots.into_iter().map(|root| root.join("xdg-desktop-portal")).collect(), desktops,
            service: data.join("dbus-1/services/org.freedesktop.impl.portal.desktop.filescope.service"),
            descriptor: std::env::var_os("XDG_DESKTOP_PORTAL_DIR").map(PathBuf::from)
                .unwrap_or_else(|| "/usr/share/xdg-desktop-portal/portals".into()).join("zz-filescope.portal"),
        })
    }

    fn effective(&self) -> Result<Option<(PathBuf, String)>, String> {
        let names = self.desktops.iter().map(|desktop| format!("{desktop}-portals.conf"))
            .chain(std::iter::once("portals.conf".into())).collect::<Vec<_>>();
        for root in &self.search {
            for name in &names {
                let path = root.join(name);
                if let Some(text) = read_regular(&path)? { return Ok(Some((path, text))); }
            }
        }
        Ok(None)
    }
}

fn read_regular(path: &Path) -> Result<Option<String>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => fs::read_to_string(path).map(Some).map_err(|err| format!("{}: {err}", path.display())),
        Ok(_) => Err(format!("Refusing to change non-regular configuration {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
    }
}

fn chooser(text: &str) -> Option<String> {
    preference(text, CHOOSER)
}

fn preference(text: &str, name: &str) -> Option<String> {
    let mut preferred = false;
    let mut value = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') { preferred = line == "[preferred]"; }
        if preferred && let Some((key, choice)) = line.split_once('=') && key.trim() == name {
            value = Some(choice.trim().into());
        }
    }
    value
}

fn patch_chooser(text: &str, choice: Option<&str>) -> String {
    let mut result = String::new();
    let mut preferred = false;
    let mut inserted = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with('[') { preferred = trimmed == "[preferred]"; }
        if preferred && trimmed.split_once('=').is_some_and(|(key, _)| key.trim() == CHOOSER) { continue; }
        result.push_str(line);
        if preferred && trimmed == "[preferred]" && !inserted {
            if let Some(choice) = choice {
                if !result.ends_with('\n') { result.push('\n'); }
                result.push_str(&format!("{CHOOSER}={choice}\n"));
            }
            inserted = true;
        }
    }
    if !inserted && let Some(choice) = choice {
        if !result.is_empty() && !result.ends_with('\n') { result.push('\n'); }
        result.push_str(&format!("[preferred]\n{CHOOSER}={choice}\n"));
    }
    result
}

fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let parent = path.parent().ok_or("Configuration has no parent directory")?;
    fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    let tmp = parent.join(format!(".filescope-picker-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    let _ = fs::remove_file(&tmp);
    result.map_err(|err: std::io::Error| format!("{}: {err}", path.display()))
}

struct Change {
    note: String,
    path: Option<PathBuf>,
    before: Option<String>,
    after: Option<String>,
    state_before: Option<String>,
    state_after: Option<String>,
}

impl Change {
    fn unchanged(note: &str) -> Self {
        Self { note: note.into(), path: None, before: None, after: None, state_before: None, state_after: None }
    }
}

fn picker_lock(paths: &Paths) -> Result<fs::File, String> {
    let state_dir = paths.state.parent().ok_or("Configuration has no parent directory")?;
    fs::create_dir_all(state_dir).map_err(|err| err.to_string())?;
    let lock_path = state_dir.join("file-picker.lock");
    let lock = OpenOptions::new().write(true).create(true).truncate(false).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW).open(lock_path).map_err(|err| err.to_string())?;
    // Concurrent enable/disable commands must not overwrite each other's restore record.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 { return Err(std::io::Error::last_os_error().to_string()); }
    Ok(lock)
}

fn configure(paths: &Paths, on: bool) -> Result<Change, String> {
    if !on && read_regular(&paths.state)?.is_none() { return Ok(Change::unchanged("Filescope file picker is already disabled")); }
    let _lock = picker_lock(paths)?;
    let previous_state = read_regular(&paths.state)?;
    if !on {
        let Some(state) = previous_state.as_deref() else { return Ok(Change::unchanged("Filescope file picker is already disabled")); };
        let state: serde_json::Value = serde_json::from_str(&state).map_err(|err| format!("Invalid picker restore record: {err}"))?;
        let name = state["name"].as_str().filter(|name| name.ends_with("portals.conf") && !name.contains('/'))
            .ok_or("Invalid picker restore filename")?;
        let installed = state["installed"].as_str().ok_or("Invalid picker restore record")?;
        if state["version"].as_u64() != Some(1) || !(state["original"].is_null() || state["original"].is_string())
            || !(state["previous"].is_null() || state["previous"].is_string()) { return Err("Invalid picker restore record".into()); }
        let path = paths.config.join(name);
        let before = read_regular(&path)?;
        let mut after = before.clone();
        if let Some(current) = before.as_deref() {
            if current == installed {
                after = state["original"].as_str().map(str::to_owned);
                if let Some(original) = state["original"].as_str() { atomic_write(&path, original)?; }
                else { fs::remove_file(&path).map_err(|err| err.to_string())?; }
            } else if let Some(choice) = chooser(&current) {
                let choices = choice.split(';').map(str::trim).filter(|choice| !choice.is_empty()).collect::<Vec<_>>();
                if choices.contains(&BACKEND) {
                    // Keep new fallback choices; remove only our own list entries.
                    let remaining = choices.into_iter().filter(|choice| *choice != BACKEND).collect::<Vec<_>>();
                    let restored = if remaining.is_empty() { state["previous"].as_str().map(str::to_owned) }
                        else { Some(format!("{};", remaining.join(";"))) };
                    let text = patch_chooser(&current, restored.as_deref());
                    atomic_write(&path, &text)?;
                    after = Some(text);
                }
            }
        }
        let mut change = Change { note: "Filescope file picker disabled".into(), path: Some(path), before, after,
            state_before: previous_state, state_after: None };
        if let Err(error) = fs::remove_file(&paths.state) {
            change.state_after = change.state_before.clone();
            return Err(match restore_snapshot(paths, &change) {
                Ok(()) => format!("{error}. Previous picker preference restored."),
                Err(restore) => match save_recovery(paths, &change) {
                    Ok(path) => format!("{error}. Could not restore the previous preference: {restore}. Recovery data saved to {}", path.display()),
                    Err(recovery) => format!("{error}. Could not restore the previous preference: {restore}. Could not save recovery data: {recovery}"),
                },
            });
        }
        return Ok(change);
    }
    let descriptor = fs::read_to_string(&paths.descriptor).map_err(|_| "Install Filescope before enabling its file picker (portal registration is missing)".to_string())?;
    let service = fs::read_to_string(&paths.service).map_err(|_| "Install Filescope before enabling its file picker (activation service is missing)".to_string())?;
    if !descriptor.contains("DBusName=org.freedesktop.impl.portal.desktop.filescope") || !descriptor.contains(CHOOSER)
        || !service.contains("--file-picker-portal") { return Err("Filescope portal registration is invalid; reinstall Filescope".into()); }
    let effective = paths.effective()?;
    let source = effective.as_ref().map(|(_, text)| text.as_str()).unwrap_or("");
    let path = effective.as_ref().filter(|(path, _)| path.parent() == Some(paths.config.as_path()))
        .map(|(path, _)| path.clone()).unwrap_or_else(|| paths.config.join(paths.desktops.first()
            .map(|desktop| format!("{desktop}-portals.conf")).unwrap_or_else(|| "portals.conf".into())));
    if let Some(previous) = previous_state.as_deref() {
        let record: serde_json::Value = serde_json::from_str(previous).map_err(|err| format!("Invalid picker restore record: {err}"))?;
        let name = record["name"].as_str().filter(|name| name.ends_with("portals.conf") && !name.contains('/')).ok_or("Invalid picker restore filename")?;
        if record["version"].as_u64() != Some(1) || !record["installed"].is_string()
            || !(record["original"].is_null() || record["original"].is_string())
            || !(record["previous"].is_null() || record["previous"].is_string()) { return Err("Invalid picker restore record".into()); }
        if paths.config.join(name) != path { return Err("Another desktop has a saved file picker setting. Disable it in that desktop before changing this one.".into()); }
        if enabled_in(paths)? { return Ok(Change::unchanged("Filescope file picker is already enabled")); }
    }
    let original = read_regular(&path)?;
    let installed = patch_chooser(source, Some(BACKEND));
    let record = serde_json::json!({"version": 1, "name": path.file_name().and_then(|name| name.to_str()),
        "original": &original, "installed": &installed, "previous": chooser(source)}).to_string();
    atomic_write(&paths.state, &record)?;
    if let Err(err) = atomic_write(&path, &installed) {
        let restored = if let Some(previous) = previous_state.as_deref() { atomic_write(&paths.state, previous) }
            else { fs::remove_file(&paths.state).map_err(|err| err.to_string()) };
        if let Err(restore) = restored { return Err(format!("{err}; could not restore picker recovery data: {restore}")); }
        return Err(err);
    }
    Ok(Change { note: "Filescope file picker enabled".into(), path: Some(path), before: original, after: Some(installed),
        state_before: previous_state, state_after: Some(record) })
}

pub(crate) async fn set_enabled(on: bool) -> Result<String, String> {
    let paths = Paths::user()?;
    apply(&paths, on, super::runtime::ensure_idle(), super::runtime::reload).await
}

async fn apply<F: std::future::Future<Output = Result<(), String>>>(paths: &Paths, on: bool,
    idle: impl std::future::Future<Output = Result<bool, String>>,
    mut reload: impl FnMut(bool) -> F) -> Result<String, String> {
    idle.await?;
    let change = configure(paths, on)?;
    let expected = first_chooser(paths);
    let mut reload_started = false;
    let applied: Result<(), String> = async {
        let current = expected.as_ref().map_err(Clone::clone)?;
        if (current.as_deref() == Some(BACKEND)) != on { return Err("The Open/Save dialog preference is managed outside Filescope; it could not be changed.".into()); }
        reload_started = true;
        reload(requires_chooser(paths)?).await?;
        if first_chooser(paths)?.as_ref() != current.as_ref() { return Err("The Open/Save dialog preference changed while it was being applied.".into()); }
        Ok(())
    }.await;
    let error = match applied { Ok(()) => return Ok(change.note), Err(error) => error };
    if change.path.is_none() {
        if !reload_started || first_chooser(paths).ok().as_ref() != expected.as_ref().ok() { return Err(error); }
    } else if let Err(rollback) = rollback(paths, &change) {
        return Err(match save_recovery(paths, &change) {
            Ok(path) => format!("{error} Could not restore the previous preference: {rollback}. Recovery data saved to {}", path.display()),
            Err(recovery) => format!("{error} Could not restore the previous preference: {rollback}. Could not save recovery data: {recovery}"),
        });
    }
    let recovered: Result<(), String> = async {
        let restored = first_chooser(paths)?;
        reload(requires_chooser(paths)?).await?;
        if first_chooser(paths)? != restored { return Err("The preference changed while restoring desktop dialogs".into()); }
        Ok(())
    }.await;
    match recovered {
        Ok(()) if change.path.is_none() => Ok(change.note),
        Ok(()) => Err(format!("{error} Previous picker preference and desktop dialogs restored.")),
        Err(recovery) => Err(format!("{error} Previous picker preference restored on disk, but desktop dialogs could not be recovered: {recovery}")),
    }
}

fn restore_file(path: &Path, contents: Option<&str>) -> Result<(), String> {
    if let Some(contents) = contents { return atomic_write(path, contents); }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn rollback(paths: &Paths, change: &Change) -> Result<(), String> {
    let _lock = picker_lock(paths)?;
    restore_snapshot(paths, change)
}

fn restore_snapshot(paths: &Paths, change: &Change) -> Result<(), String> {
    let Some(path) = change.path.as_deref() else { return Ok(()); };
    if read_regular(path)? != change.after || read_regular(&paths.state)? != change.state_after {
        return Err("Settings were edited by another application; its changes were kept".into());
    }
    if change.before != change.after { restore_file(path, change.before.as_deref())?; }
    if change.state_before != change.state_after { restore_file(&paths.state, change.state_before.as_deref())?; }
    Ok(())
}

fn save_recovery(paths: &Paths, change: &Change) -> Result<PathBuf, String> {
    let name = format!("file-picker-recovery-{}.json", gtk::glib::uuid_string_random());
    let contents = serde_json::json!({ "version": 1,
        "path": change.path.as_ref().map(|path| path.as_os_str().as_encoded_bytes()),
        "state_path": paths.state.as_os_str().as_encoded_bytes(),
        "before": &change.before, "after": &change.after,
        "state_before": &change.state_before, "state_after": &change.state_after }).to_string();
    let mut last_error = String::new();
    for path in [paths.state.with_file_name(&name), std::env::temp_dir().join(&name)] {
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file) => match file.write_all(contents.as_bytes()).and_then(|_| file.sync_all()) {
                Ok(()) => return Ok(path),
                Err(error) => { last_error = error.to_string(); let _ = fs::remove_file(path); }
            },
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error)
}

fn first_chooser(paths: &Paths) -> Result<Option<String>, String> {
    let Some((_, text)) = paths.effective()? else { return Ok(None); };
    let choice = chooser(&text).or_else(|| preference(&text, "default"));
    Ok(choice.and_then(|choice| choice.split(';').map(str::trim).find(|choice| !choice.is_empty()).map(str::to_owned)))
}

fn enabled_in(paths: &Paths) -> Result<bool, String> {
    Ok(first_chooser(paths)?.as_deref() == Some(BACKEND))
}

fn requires_chooser(paths: &Paths) -> Result<bool, String> {
    Ok(first_chooser(paths)?.as_deref() != Some("none"))
}

pub(crate) fn is_enabled() -> Result<bool, String> {
    enabled_in(&Paths::user()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture { root: PathBuf, paths: Paths }
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!("filescope-picker-config-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
            fs::create_dir_all(root.join("user")).unwrap();
            fs::create_dir_all(root.join("system")).unwrap();
            fs::write(root.join("backend.portal"), "[portal]\nDBusName=org.freedesktop.impl.portal.desktop.filescope\nInterfaces=org.freedesktop.impl.portal.FileChooser;\n").unwrap();
            fs::write(root.join("backend.service"), "[D-BUS Service]\nName=org.freedesktop.impl.portal.desktop.filescope\nExec=/tmp/filescope --file-picker-portal\n").unwrap();
            let paths = Paths { config: root.join("user"), state: root.join("state/picker.json"), search: vec![root.join("user"), root.join("system")], desktops: vec!["gnome".into()], service: root.join("backend.service"), descriptor: root.join("backend.portal") };
            Self { root, paths }
        }
        fn user(&self) -> PathBuf { self.paths.config.join("gnome-portals.conf") }
        fn defaults(&self, text: &str) { fs::write(self.root.join("system/gnome-portals.conf"), text).unwrap(); }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }

    #[test]
    fn picker_disable_preserves_other_portals() {
        let f = Fixture::new();
        let original = "# keep this comment\n[preferred]\ndefault=gnome;gtk;\norg.freedesktop.impl.portal.FileChooser=gtk;\norg.freedesktop.impl.portal.ScreenCast=gnome;\n";
        fs::write(f.user(), original).unwrap();
        configure(&f.paths, true).unwrap();
        let enabled = fs::read_to_string(f.user()).unwrap();
        assert!(enabled.contains(&format!("{CHOOSER}={BACKEND}")));
        assert!(enabled.contains("org.freedesktop.impl.portal.ScreenCast=gnome;"));
        configure(&f.paths, false).unwrap();
        assert_eq!(fs::read_to_string(f.user()).unwrap(), original);
        assert!(!f.paths.state.exists());
    }

    #[test]
    fn picker_setting_reads_the_first_effective_preference() {
        let f = Fixture::new();
        for (text, enabled) in [
            ("[preferred]\norg.freedesktop.impl.portal.FileChooser=zz-filescope;gtk;\n", true),
            ("[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;zz-filescope;\n", false),
            ("[preferred]\ndefault=zz-filescope;gtk;\n", true),
            ("[preferred]\ndefault=zz-filescope;\norg.freedesktop.impl.portal.FileChooser=gtk;\n", false),
            ("[preferred]\ndefault=gtk;\n[other]\norg.freedesktop.impl.portal.FileChooser=zz-filescope;\n", false),
        ] {
            fs::write(f.user(), text).unwrap();
            assert_eq!(enabled_in(&f.paths).unwrap(), enabled, "{text}");
        }
        fs::remove_file(f.user()).unwrap();
        fs::create_dir_all(f.paths.state.parent().unwrap()).unwrap(); fs::write(&f.paths.state, "stale restore record").unwrap();
        assert!(!enabled_in(&f.paths).unwrap());
        f.defaults("[preferred]\norg.freedesktop.impl.portal.FileChooser=zz-filescope;\n");
        assert!(enabled_in(&f.paths).unwrap());
        fs::write(f.paths.config.join("portals.conf"), "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;\n").unwrap();
        assert!(!enabled_in(&f.paths).unwrap());
        std::os::unix::fs::symlink(f.root.join("missing"), f.user()).unwrap();
        assert!(enabled_in(&f.paths).is_err());
    }

    #[test]
    fn picker_reenable_rebases_external_preferences_without_losing_restore_data() {
        for choice in ["gtk;", "gtk;zz-filescope;"] {
            let f = Fixture::new(); f.defaults("[preferred]\ndefault=gtk;\n");
            configure(&f.paths, true).unwrap();
            let edited = format!("[preferred]\ndefault=gtk;\n{CHOOSER}={choice}\norg.freedesktop.impl.portal.ScreenCast=kde;\n");
            fs::write(f.user(), &edited).unwrap();
            configure(&f.paths, true).expect("re-enabling must recover after choosing another backend");
            assert!(fs::read_to_string(f.user()).unwrap().contains("org.freedesktop.impl.portal.ScreenCast=kde;"));
            configure(&f.paths, false).unwrap();
            assert_eq!(fs::read_to_string(f.user()).unwrap(), edited);
        }
    }

    #[test]
    fn picker_failed_reenable_keeps_the_previous_restore_record() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new(); f.defaults("[preferred]\ndefault=gtk;\n");
        configure(&f.paths, true).unwrap();
        let previous = fs::read_to_string(&f.paths.state).unwrap();
        let edited = "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;\n";
        fs::write(f.user(), edited).unwrap();
        fs::set_permissions(&f.paths.config, fs::Permissions::from_mode(0o555)).unwrap();
        let result = configure(&f.paths, true);
        fs::set_permissions(&f.paths.config, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&f.paths.state).unwrap(), previous);
        assert_eq!(fs::read_to_string(f.user()).unwrap(), edited);
    }

    #[test]
    fn picker_reenable_rejects_damaged_restore_records() {
        for invalid_json in [true, false] {
            let f = Fixture::new(); configure(&f.paths, true).unwrap();
            let mut record: serde_json::Value = serde_json::from_str(&fs::read_to_string(&f.paths.state).unwrap()).unwrap();
            record["original"] = serde_json::json!(42);
            let damaged = if invalid_json { "not json".into() } else { record.to_string() };
            fs::write(&f.paths.state, &damaged).unwrap();
            let edited = "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;\n";
            fs::write(f.user(), edited).unwrap();
            assert!(configure(&f.paths, true).is_err());
            assert_eq!(fs::read_to_string(&f.paths.state).unwrap(), damaged);
            assert_eq!(fs::read_to_string(f.user()).unwrap(), edited);
        }
    }

    #[test]
    fn picker_reenable_preserves_another_desktops_restore_record() {
        let f = Fixture::new(); configure(&f.paths, true).unwrap();
        let old_config = f.paths.config.join("kde-portals.conf");
        fs::rename(f.user(), &old_config).unwrap();
        let old_contents = fs::read_to_string(&old_config).unwrap();
        let mut record: serde_json::Value = serde_json::from_str(&fs::read_to_string(&f.paths.state).unwrap()).unwrap();
        record["name"] = serde_json::json!("kde-portals.conf");
        let previous = record.to_string(); fs::write(&f.paths.state, &previous).unwrap();
        let edited = "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;\n"; fs::write(f.user(), edited).unwrap();
        assert!(configure(&f.paths, true).is_err());
        assert_eq!(fs::read_to_string(&f.paths.state).unwrap(), previous);
        assert_eq!(fs::read_to_string(&old_config).unwrap(), old_contents);
        assert_eq!(fs::read_to_string(f.user()).unwrap(), edited);
    }

    #[test]
    fn picker_apply_checks_busy_before_writing() {
        let f = Fixture::new();
        let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, true,
            std::future::ready(Err("Finish the active portal request first".into())),
            |_| async { panic!("busy portal must not be restarted"); }));
        assert!(result.unwrap_err().contains("active portal request"));
        assert!(!f.user().exists() && !f.paths.state.exists());
        assert!(!f.paths.state.parent().unwrap().exists());
    }

    #[test]
    fn picker_apply_failed_reload_restores_enable_and_disable_exactly() {
        for on in [true, false] {
            let f = Fixture::new();
            fs::write(f.user(), "# original bytes\n[preferred]\ndefault=gtk;\n\n").unwrap();
            if !on { configure(&f.paths, true).unwrap(); }
            let before = read_regular(&f.user()).unwrap();
            let state_before = read_regular(&f.paths.state).unwrap();
            let calls = std::cell::Cell::new(0);
            let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, on, std::future::ready(Ok(true)), |_| async {
                if calls.replace(calls.get() + 1) == 0 { assert_eq!(enabled_in(&f.paths).unwrap(), on); }
                Err("test restart failed".into())
            }));
            assert!(result.unwrap_err().contains("test restart failed"));
            assert_eq!(read_regular(&f.user()).unwrap(), before);
            assert_eq!(read_regular(&f.paths.state).unwrap(), state_before);
        }
        let f = Fixture::new(); f.defaults("[preferred]\ndefault=gtk;\n");
        let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, true,
            std::future::ready(Ok(true)), |_| std::future::ready(Err("test restart failed".into()))));
        assert!(result.is_err()); assert!(!f.user().exists() && !f.paths.state.exists());
    }

    #[test]
    fn picker_apply_success_reloads_without_a_logout() {
        let f = Fixture::new(); let original = "[preferred]\ndefault=gtk;\n"; fs::write(f.user(), original).unwrap();
        for on in [true, false] {
            let reloaded = std::cell::Cell::new(false);
            let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, on, std::future::ready(Ok(false)), |_| async {
                assert_eq!(enabled_in(&f.paths).unwrap(), on); reloaded.set(true); Ok(())
            })).unwrap();
            assert!(reloaded.get()); assert!(!result.to_lowercase().contains("sign out") && !result.to_lowercase().contains("logout"));
            assert_eq!(enabled_in(&f.paths).unwrap(), on);
        }
        assert_eq!(fs::read_to_string(f.user()).unwrap(), original);
    }

    #[test]
    fn picker_reload_requirement_follows_the_committed_preference() {
        let f = Fixture::new(); assert!(requires_chooser(&f.paths).unwrap());
        f.defaults("[preferred]\ndefault=none\n"); assert!(!requires_chooser(&f.paths).unwrap());
        for on in [true, false] {
            gtk::glib::MainContext::new().block_on(apply(&f.paths, on, std::future::ready(Ok(false)), |_| async {
                assert_eq!(requires_chooser(&f.paths).unwrap(), on); Ok(())
            })).unwrap();
        }
        for (preference, required) in [("none;gtk;", false), ("gtk;none;", true), ("zz-filescope;gtk;", true)] {
            fs::write(f.user(), format!("[preferred]\ndefault=none;\n{CHOOSER}={preference}\n")).unwrap();
            assert_eq!(requires_chooser(&f.paths).unwrap(), required);
        }
    }

    #[test]
    fn picker_apply_preserves_external_edits_and_recovery_data() {
        for on in [true, false] { for edit_state in [true, false] {
            let f = Fixture::new(); fs::write(f.user(), "[preferred]\ndefault=gtk;\n").unwrap();
            if !on { configure(&f.paths, true).unwrap(); }
            let before = read_regular(&f.user()).unwrap(); let state_before = read_regular(&f.paths.state).unwrap();
            let edited = "# edited while reload was pending\n";
            let edited_path = if edit_state { f.paths.state.clone() } else { f.user() };
            let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, on, std::future::ready(Ok(true)), |_| async {
                fs::write(&edited_path, edited).unwrap(); Err("test restart failed".into())
            }));
            let error = result.unwrap_err(); assert!(error.contains("test restart failed"));
            assert_eq!(fs::read_to_string(&edited_path).unwrap(), edited);
            let recovery = fs::read_dir(f.paths.state.parent().unwrap()).unwrap().filter_map(Result::ok)
                .find(|entry| entry.file_name().to_string_lossy().starts_with("file-picker-recovery-")).expect("failed rollback retains its snapshot");
            let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(recovery.path()).unwrap()).unwrap();
            assert_eq!(saved["before"], serde_json::json!(before));
            assert_eq!(saved["state_before"], serde_json::json!(state_before));
        } }
    }

    #[test]
    fn picker_apply_verifies_the_setting_after_reload() {
        let f = Fixture::new();
        let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, true, std::future::ready(Ok(true)), |_| async {
            fs::write(f.user(), "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;\n").unwrap(); Ok(())
        }));
        assert!(result.is_err()); assert!(!enabled_in(&f.paths).unwrap());
    }

    #[test]
    fn picker_apply_no_op_does_not_claim_to_restore_others_changes() {
        let f = Fixture::new(); configure(&f.paths, true).unwrap();
        let edited = "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;\n";
        let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, true, std::future::ready(Ok(true)), |_| async {
            fs::write(f.user(), edited).unwrap(); Err("test restart failed".into())
        }));
        let error = result.unwrap_err(); assert!(error.contains("test restart failed") && !error.contains("restored"));
        assert_eq!(fs::read_to_string(f.user()).unwrap(), edited);
        let f = Fixture::new(); fs::write(f.user(), "[preferred]\norg.freedesktop.impl.portal.FileChooser=zz-filescope;\n").unwrap();
        let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, false, std::future::ready(Ok(false)), |_| async {
            panic!("an unchanged managed preference must not restart the portal");
        }));
        assert!(result.unwrap_err().contains("managed outside Filescope")); assert!(enabled_in(&f.paths).unwrap());
        assert!(!f.paths.state.exists());
    }

    #[test]
    fn picker_apply_keeps_recovery_data_when_restoring_the_record_fails() {
        use std::os::unix::fs::PermissionsExt;
        let f = Fixture::new(); fs::write(f.user(), "[preferred]\ndefault=gtk;\n").unwrap();
        configure(&f.paths, true).unwrap();
        let before = read_regular(&f.user()).unwrap(); let state_before = read_regular(&f.paths.state).unwrap();
        let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, false, std::future::ready(Ok(true)), |_| async {
            fs::set_permissions(f.paths.state.parent().unwrap(), fs::Permissions::from_mode(0o555)).unwrap();
            Err("test restart failed".into())
        }));
        fs::set_permissions(f.paths.state.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.unwrap_err();
        let path = PathBuf::from(error.split("Recovery data saved to ").nth(1).expect("failed restore preserves recovery data"));
        let saved: serde_json::Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["state_before"], serde_json::json!(state_before));
        assert_eq!(read_regular(&f.user()).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn picker_disable_failure_restores_portal_bytes_and_preserves_record() {
        use std::os::unix::fs::PermissionsExt;
        for existing_override in [true, false] {
            let f = Fixture::new(); f.defaults("[preferred]\ndefault=gtk;\n");
            if existing_override { fs::write(f.user(), "# original\n[preferred]\ndefault=gtk;\n").unwrap(); }
            configure(&f.paths, true).unwrap();
            let before = read_regular(&f.user()).unwrap(); let record = read_regular(&f.paths.state).unwrap();
            fs::set_permissions(f.paths.state.parent().unwrap(), fs::Permissions::from_mode(0o555)).unwrap();
            let result = configure(&f.paths, false);
            fs::set_permissions(f.paths.state.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
            assert!(result.is_err()); assert_eq!(read_regular(&f.user()).unwrap(), before);
            assert_eq!(read_regular(&f.paths.state).unwrap(), record);
        }
    }

    #[test]
    fn picker_apply_restores_live_dialogs_after_failed_reload() {
        for on in [true, false] { for original in ["gtk;", "none;"] {
            let f = Fixture::new(); f.defaults(&format!("[preferred]\ndefault={original}\n"));
            if !on { configure(&f.paths, true).unwrap(); }
            let before = read_regular(&f.user()).unwrap(); let record = read_regular(&f.paths.state).unwrap();
            let calls = std::cell::Cell::new(0);
            let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, on, std::future::ready(Ok(true)), |required| {
                let call = calls.replace(calls.get() + 1);
                std::future::ready(if call == 0 { Err("new frontend has no chooser".into()) }
                    else { assert_eq!(required, !on || original != "none;"); assert_eq!(enabled_in(&f.paths).unwrap(), !on); Ok(()) })
            }));
            assert!(result.unwrap_err().contains("new frontend has no chooser"));
            assert_eq!(calls.get(), 2, "disk rollback must also reload the restored preference");
            assert_eq!(read_regular(&f.user()).unwrap(), before); assert_eq!(read_regular(&f.paths.state).unwrap(), record);
        } }
    }

    #[test]
    fn picker_apply_reports_when_live_dialog_recovery_also_fails() {
        for unchanged in [true, false] {
            let f = Fixture::new(); let calls = std::cell::Cell::new(0);
            if unchanged { configure(&f.paths, true).unwrap(); }
            let before = read_regular(&f.user()).unwrap(); let record = read_regular(&f.paths.state).unwrap();
            let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, true, std::future::ready(Ok(true)), |_| {
                calls.set(calls.get() + 1); std::future::ready(Err("portal unavailable".into()))
            }));
            let error = result.unwrap_err(); assert_eq!(calls.get(), 2);
            assert!(error.contains("desktop dialogs could not be recovered"), "{error}");
            assert_eq!(read_regular(&f.user()).unwrap(), before); assert_eq!(read_regular(&f.paths.state).unwrap(), record);
        }
    }

    #[test]
    fn picker_apply_retry_finishes_an_unchanged_setting() {
        let f = Fixture::new(); configure(&f.paths, true).unwrap();
        let before = read_regular(&f.user()).unwrap(); let record = read_regular(&f.paths.state).unwrap();
        let calls = std::cell::Cell::new(0);
        let result = gtk::glib::MainContext::new().block_on(apply(&f.paths, true, std::future::ready(Ok(true)), |required| {
            assert!(required);
            std::future::ready(if calls.replace(calls.get() + 1) == 0 { Err("frontend has no chooser".into()) } else { Ok(()) })
        }));
        assert!(result.is_ok()); assert_eq!(calls.get(), 2);
        assert_eq!(read_regular(&f.user()).unwrap(), before); assert_eq!(read_regular(&f.paths.state).unwrap(), record);
    }

    #[test]
    fn picker_new_override_preserves_desktop_defaults_and_is_removed_on_disable() {
        let f = Fixture::new();
        f.defaults("[preferred]\ndefault=gnome;gtk;\norg.freedesktop.impl.portal.Secret=gnome-keyring;\n");
        configure(&f.paths, true).unwrap();
        let enabled = fs::read_to_string(f.user()).unwrap();
        assert!(enabled.contains("default=gnome;gtk;"));
        assert!(enabled.contains("org.freedesktop.impl.portal.Secret=gnome-keyring;"));
        configure(&f.paths, true).unwrap();
        configure(&f.paths, false).unwrap();
        configure(&f.paths, false).unwrap();
        assert!(!f.user().exists());
    }

    #[test]
    fn picker_disable_keeps_intervening_unrelated_edits() {
        let f = Fixture::new();
        f.defaults("[preferred]\ndefault=gtk;\norg.freedesktop.impl.portal.FileChooser=gtk;\n");
        configure(&f.paths, true).unwrap();
        let edited = format!("{}org.freedesktop.impl.portal.ScreenCast=kde;\n", fs::read_to_string(f.user()).unwrap());
        fs::write(f.user(), edited).unwrap();
        configure(&f.paths, false).unwrap();
        let text = fs::read_to_string(f.user()).unwrap();
        assert!(text.contains("org.freedesktop.impl.portal.ScreenCast=kde;"));
        assert!(text.contains("org.freedesktop.impl.portal.FileChooser=gtk;"));
        assert!(!text.contains(BACKEND));
    }

    #[test]
    fn picker_disable_does_not_overwrite_a_new_chooser() {
        let f = Fixture::new();
        configure(&f.paths, true).unwrap();
        let edited = fs::read_to_string(f.user()).unwrap().replace(BACKEND, "kde");
        fs::write(f.user(), &edited).unwrap();
        configure(&f.paths, false).unwrap();
        assert_eq!(fs::read_to_string(f.user()).unwrap(), edited);
    }

    #[test]
    fn picker_disable_removes_filescope_from_edited_fallback_lists() {
        for edited_choice in ["zz-filescope;gtk;", "gtk;zz-filescope;", "gtk;zz-filescope;kde;zz-filescope;"] {
            let f = Fixture::new();
            configure(&f.paths, true).unwrap();
            let edited = fs::read_to_string(f.user()).unwrap().replace(BACKEND, edited_choice);
            fs::write(f.user(), edited).unwrap();
            configure(&f.paths, false).unwrap();
            let restored = fs::read_to_string(f.user()).unwrap();
            assert!(!restored.contains(BACKEND), "{restored}");
            assert_eq!(chooser(&restored).unwrap(), if edited_choice.contains("kde") { "gtk;kde;" } else { "gtk;" });
            assert!(!f.paths.state.exists());
        }
    }

    #[test]
    fn picker_enable_requires_registration_and_rejects_symlinked_config() {
        let f = Fixture::new();
        let descriptor = fs::read_to_string(&f.paths.descriptor).unwrap();
        fs::remove_file(&f.paths.descriptor).unwrap();
        assert!(configure(&f.paths, true).is_err());
        assert!(!f.paths.state.exists() && !f.user().exists());
        fs::write(&f.paths.descriptor, descriptor).unwrap();
        fs::write(f.root.join("original"), "[preferred]\ndefault=gtk;\n").unwrap();
        std::os::unix::fs::symlink(f.root.join("original"), f.user()).unwrap();
        assert!(configure(&f.paths, true).is_err());
        assert_eq!(fs::read_to_string(f.root.join("original")).unwrap(), "[preferred]\ndefault=gtk;\n");
    }

    #[test]
    fn picker_existing_generic_config_keeps_its_name_and_comments() {
        let f = Fixture::new();
        let path = f.paths.config.join("portals.conf");
        let original = "# mine\n[preferred]\ndefault=kde;\n\n[another-group]\nkeep=this exactly\n";
        fs::write(&path, original).unwrap();
        configure(&f.paths, true).unwrap();
        assert!(!f.user().exists());
        configure(&f.paths, false).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn picker_missing_config_and_damaged_restore_record_are_safe() {
        let f = Fixture::new();
        configure(&f.paths, true).unwrap();
        let installed = fs::read_to_string(f.user()).unwrap();
        let record = fs::read_to_string(&f.paths.state).unwrap();
        fs::write(&f.paths.state, "not json").unwrap();
        assert!(configure(&f.paths, false).is_err());
        assert_eq!(fs::read_to_string(f.user()).unwrap(), installed);
        fs::write(&f.paths.state, record).unwrap();
        fs::remove_file(f.user()).unwrap();
        configure(&f.paths, false).unwrap();
        assert!(!f.paths.state.exists());
    }

    #[test]
    fn picker_changes_only_the_preferred_group_and_removes_duplicate_chooser_keys() {
        let text = "[preferred]\norg.freedesktop.impl.portal.FileChooser=gtk;\n\n[other]\norg.freedesktop.impl.portal.FileChooser=leave;\n[preferred]\norg.freedesktop.impl.portal.FileChooser=kde;\n";
        let patched = patch_chooser(text, Some(BACKEND));
        assert_eq!(patched.matches(BACKEND).count(), 1);
        assert!(patched.contains("[other]\norg.freedesktop.impl.portal.FileChooser=leave;"));
        assert_eq!(chooser(&patched).as_deref(), Some(BACKEND));
    }
}
