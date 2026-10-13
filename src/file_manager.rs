// Default-file-manager integration.
//
// Other apps ask `org.freedesktop.FileManager1` to show a folder or select a
// file. That name is claimed only after the user checks "Default File Manager".
// The service file in the user data dir is what makes the session bus start
// filescope for those calls; removing it hands the name back to whoever shipped
// a system service (usually Nautilus).

use std::path::{Path, PathBuf};

use gtk::{gio, glib};
use gtk::prelude::*;

pub const BUS_NAME: &str = "org.freedesktop.FileManager1";
const OBJECT_PATH: &str = "/org/freedesktop/FileManager1";
const DESKTOP_ID: &str = "dev.filescope.Filescope.desktop";
/// `DBUS_NAME_FLAG_DO_NOT_QUEUE`: if another manager already owns the name, do
/// not sit in a queue and take it when they quit unexpectedly.
const DO_NOT_QUEUE: u32 = 4;
const REG_KEY: &str = "filescope-file-manager";

const INTERFACE_XML: &str = r#"
<node>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItems">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItemProperties">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
  </interface>
</node>
"#;

/// One folder to open, optionally with files in it to select.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    pub dir: PathBuf,
    /// Empty when the request is the folder itself.
    pub names: Vec<String>,
    pub properties: bool,
}

pub fn is_enabled() -> bool {
    service_file().is_file()
}

/// Turn the default-manager hook on or off. The returned note is for a toast.
pub fn set_enabled(on: bool) -> Result<String, String> {
    if on {
        install_service()?;
        match set_folder_handler() {
            Ok(()) => Ok("filescope is the default file manager".to_string()),
            Err(err) => Ok(format!(
                "filescope will answer Show in Folder. Folder opening could not be set: {err}"
            )),
        }
    } else {
        let _ = std::fs::remove_file(service_file());
        restore_folder_handler();
        Ok("filescope is no longer the default file manager".to_string())
    }
}

/// Export the interface and take the bus name when the user has opted in.
pub fn own(app: &adw::Application) {
    if std::env::var_os("FILESCOPE_SHOT").is_some() || !is_enabled() {
        return;
    }
    let Some(bus) = app.dbus_connection() else { return };
    if let Err(err) = export_object(app, &bus) {
        eprintln!("filescope: file manager interface: {err}");
        return;
    }
    request_name(&bus);
}

/// Drop the bus name and the exported object. The service file is left alone;
/// [`set_enabled`] removes that.
pub fn release(app: &adw::Application) {
    let Some(bus) = app.dbus_connection() else { return };
    let _ = bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "ReleaseName",
        Some(&BUS_NAME.to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
    );
    if let Some(id) = unsafe { app.steal_data::<gio::RegistrationId>(REG_KEY) } {
        let _ = bus.unregister_object(id);
    }
}

fn export_object(app: &adw::Application, bus: &gio::DBusConnection) -> Result<(), glib::Error> {
    if unsafe { app.data::<gio::RegistrationId>(REG_KEY) }.is_some() {
        return Ok(());
    }
    let node = gio::DBusNodeInfo::for_xml(INTERFACE_XML)?;
    let info = node.lookup_interface(BUS_NAME).expect("interface is in the xml");
    let calls = app.clone();
    let id = bus
        .register_object(OBJECT_PATH, &info)
        .method_call(move |_conn, _sender, _path, _iface, method, params, invocation| {
            let app = calls.clone();
            match method {
                "ShowFolders" => {
                    if let Some((uris, _)) = uris_of(&params) {
                        let targets = folders_from_uris(&uris);
                        let app = app.clone();
                        glib::idle_add_local_once(move || crate::window::open_targets(&app, &targets));
                    }
                    invocation.return_value(Some(&().to_variant()));
                }
                "ShowItems" => {
                    if let Some((uris, _)) = uris_of(&params) {
                        let targets = items_from_uris(&uris, false);
                        let app = app.clone();
                        glib::idle_add_local_once(move || crate::window::open_targets(&app, &targets));
                    }
                    invocation.return_value(Some(&().to_variant()));
                }
                "ShowItemProperties" => {
                    if let Some((uris, _)) = uris_of(&params) {
                        let targets = items_from_uris(&uris, true);
                        let app = app.clone();
                        glib::idle_add_local_once(move || crate::window::open_targets(&app, &targets));
                    }
                    invocation.return_value(Some(&().to_variant()));
                }
                other => {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.UnknownMethod",
                        &format!("No such method {other}"),
                    );
                }
            }
        })
        .build()?;
    unsafe { app.set_data(REG_KEY, id) };
    Ok(())
}

fn request_name(bus: &gio::DBusConnection) {
    let bus = bus.clone();
    bus.call(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "RequestName",
        Some(&(BUS_NAME.to_string(), DO_NOT_QUEUE).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
        |res| {
            let reply = res.ok().and_then(|value| value.child_value(0).get::<u32>());
            if reply == Some(3) {
                eprintln!(
                    "filescope: {BUS_NAME} is already taken. Quit the other file manager and toggle Default File Manager again."
                );
            }
        },
    );
}

fn uris_of(params: &glib::Variant) -> Option<(Vec<String>, String)> {
    if params.n_children() < 2 {
        return None;
    }
    let uris = params.child_value(0).get::<Vec<String>>()?;
    let startup = params.child_value(1).get::<String>().unwrap_or_default();
    Some((uris, startup))
}

/// Folders to open for `ShowFolders`. A file URI opens its parent.
pub fn folders_from_uris(uris: &[String]) -> Vec<Target> {
    uris.iter()
        .filter_map(|uri| file_uri_to_path(uri))
        .map(|path| {
            let dir = if path.is_file() {
                path.parent().map(Path::to_path_buf).unwrap_or(path)
            } else {
                path
            };
            Target { dir, names: Vec::new(), properties: false }
        })
        .collect()
}

/// Files to select, grouped by parent folder, for `ShowItems`.
pub fn items_from_uris(uris: &[String], properties: bool) -> Vec<Target> {
    let mut groups: Vec<Target> = Vec::new();
    for uri in uris {
        let Some(path) = file_uri_to_path(uri) else { continue };
        if path.is_dir() {
            groups.push(Target { dir: path, names: Vec::new(), properties: false });
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { continue };
        let Some(dir) = path.parent().map(Path::to_path_buf) else { continue };
        if let Some(group) = groups.iter_mut().find(|g| g.dir == dir && !g.names.is_empty()) {
            group.names.push(name);
            group.properties = properties && group.names.len() == 1;
        } else {
            groups.push(Target { dir, names: vec![name], properties });
        }
    }
    groups
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let path = if let Some(rest) = uri.strip_prefix("file://") {
        percent_decode(rest)
    } else if uri.starts_with('/') {
        uri.to_string()
    } else {
        return None;
    };
    if path.is_empty() { None } else { Some(PathBuf::from(path)) }
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn service_file() -> PathBuf {
    let mut path = glib::user_data_dir();
    path.push("dbus-1");
    path.push("services");
    path.push("org.freedesktop.FileManager1.service");
    path
}

fn install_service() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|err| format!("Could not find this program: {err}"))?;
    let body = format!("[D-BUS Service]\nName={BUS_NAME}\nExec={} --gapplication-service\n", exe.display());
    let path = service_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| format!("Could not create {parent:?}: {err}"))?;
    }
    std::fs::write(&path, body).map_err(|err| format!("Could not write {}: {err}", path.display()))
}

fn set_folder_handler() -> Result<(), String> {
    let status = std::process::Command::new("xdg-mime")
        .args(["default", DESKTOP_ID, "inode/directory"])
        .status()
        .map_err(|err| format!("{err}"))?;
    if status.success() { Ok(()) } else { Err(format!("xdg-mime exited {status}")) }
}

fn restore_folder_handler() {
    let current = std::process::Command::new("xdg-mime")
        .args(["query", "default", "inode/directory"])
        .output();
    let Ok(current) = current else { return };
    if String::from_utf8_lossy(&current.stdout).trim() != DESKTOP_ID {
        return;
    }
    let Some(nautilus) = nautilus_desktop() else { return };
    let _ = std::process::Command::new("xdg-mime")
        .args(["default", nautilus, "inode/directory"])
        .status();
}

fn nautilus_desktop() -> Option<&'static str> {
    const CANDIDATES: &[(&str, &str)] = &[
        ("/usr/share/applications/org.gnome.Nautilus.desktop", "org.gnome.Nautilus.desktop"),
        ("/usr/share/applications/nautilus.desktop", "nautilus.desktop"),
    ];
    CANDIDATES.iter().find(|(path, _)| Path::new(path).is_file()).map(|(_, id)| *id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_items_groups_files_that_share_a_folder() {
        let groups = items_from_uris(
            &[
                "file:///home/a/b.txt".into(),
                "file:///home/a/c.txt".into(),
                "file:///home/d/e.txt".into(),
                "file:///home/a/my%20file".into(),
            ],
            false,
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].dir, PathBuf::from("/home/a"));
        assert_eq!(groups[0].names, ["b.txt", "c.txt", "my file"]);
        assert_eq!(groups[1].dir, PathBuf::from("/home/d"));
        assert_eq!(groups[1].names, ["e.txt"]);
    }

    #[test]
    fn show_folders_opens_a_file_parent() {
        let folders = folders_from_uris(&["file:///etc/hosts".into(), "file:///tmp".into()]);
        assert_eq!(folders[0].dir, PathBuf::from("/etc"));
        assert!(folders[0].names.is_empty());
        assert_eq!(folders[1].dir, PathBuf::from("/tmp"));
    }
}
