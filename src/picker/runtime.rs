//! Apply a chooser preference without ending the desktop session.
use gtk::gio;
#[cfg(test)]
use gtk::glib;
use gtk::prelude::*;

const FRONTEND: &str = "org.freedesktop.portal.Desktop";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const CALL_TIMEOUT: i32 = 3_000;

async fn owner(bus: &gio::DBusConnection) -> Result<Option<String>, String> {
    let reply = bus.call_future(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus",
        "GetNameOwner", Some(&(FRONTEND,).to_variant()), None, gio::DBusCallFlags::NO_AUTO_START, CALL_TIMEOUT).await;
    match reply {
        Ok(reply) => reply.get::<(String,)>().map(|(owner,)| Some(owner)).ok_or_else(|| "Invalid desktop portal owner response".into()),
        Err(error) if gio::DBusError::remote_error(&error).as_deref() == Some("org.freedesktop.DBus.Error.NameHasNoOwner") => Ok(None),
        Err(error) => Err(format!("Could not check the desktop file-dialog service: {error}")),
    }
}

async fn introspect(bus: &gio::DBusConnection, owner: &str) -> Result<gio::DBusNodeInfo, String> {
    let reply = bus.call_future(Some(owner), OBJECT_PATH, "org.freedesktop.DBus.Introspectable", "Introspect",
        Some(&().to_variant()), None, gio::DBusCallFlags::NO_AUTO_START, CALL_TIMEOUT).await
        .map_err(|error| format!("Could not check for active desktop dialogs: {error}"))?;
    let (xml,) = reply.get::<(String,)>().ok_or("Invalid desktop portal introspection response")?;
    gio::DBusNodeInfo::for_xml(&xml).map_err(|error| format!("Invalid desktop portal introspection: {error}"))
}

async fn idle_owner(bus: &gio::DBusConnection) -> Result<Option<String>, String> {
    let Some(owner) = owner(bus).await? else { return Ok(None); };
    let node = introspect(bus, &owner).await?;
    if node.nodes().iter().any(|node| matches!(node.path(), Some("request" | "session"))) {
        return Err("Finish active desktop dialogs or screen sharing before changing the file picker.".into());
    }
    Ok(Some(owner))
}

pub(crate) async fn ensure_idle() -> Result<bool, String> {
    let bus = gio::bus_get_future(gio::BusType::Session).await.map_err(|error| error.to_string())?;
    Ok(idle_owner(&bus).await?.is_some())
}

pub(crate) async fn reload(require_chooser: bool) -> Result<(), String> {
    let bus = gio::bus_get_future(gio::BusType::Session).await.map_err(|error| error.to_string())?;
    // shortcut: idle inspection cannot freeze new requests; use a native reload API if portals add one.
    let Some(previous) = idle_owner(&bus).await? else { return Ok(()); };
    let argv: Vec<_> = ["systemctl", "--user", "--no-ask-password", "restart", "xdg-desktop-portal.service"]
        .iter().map(std::ffi::OsStr::new).collect();
    let child = gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE)
        .map_err(|error| format!("Could not restart the desktop file-dialog service: {error}"))?;
    let (_, error) = child.communicate_utf8_future(None).await.map_err(|error| error.to_string())?;
    if !child.is_successful() {
        return Err(format!("Could not restart the desktop file-dialog service: {}", error.as_deref().unwrap_or("restart failed").trim()));
    }
    let current = owner(&bus).await?.ok_or("The desktop file-dialog service did not start")?;
    if current == previous { return Err("The desktop file-dialog service did not reload the new preference".into()); }
    let node = introspect(&bus, &current).await?;
    if node.interfaces().is_empty() {
        return Err("The desktop portal service is not ready".into());
    }
    if require_chooser {
        let ready = bus.call_future(Some(&current), OBJECT_PATH, "org.freedesktop.DBus.Properties", "Get",
            Some(&("org.freedesktop.portal.FileChooser", "version").to_variant()), None,
            gio::DBusCallFlags::NO_AUTO_START, CALL_TIMEOUT).await
            .map_err(|error| format!("The desktop file-dialog service is not ready: {error}"))?;
        if ready.n_children() != 1 || ready.child_value(0).as_variant().and_then(|version| version.get::<u32>()).is_none_or(|version| version == 0) {
            return Err("Invalid desktop file-dialog service version response".into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod runtime_tests;
