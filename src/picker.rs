//! FileChooser portal backend; requests never enter the normal file-manager window.
use gtk::{gio, glib};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

pub(crate) mod config;
pub(crate) mod runtime;

const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.filescope";
const FRONTEND: &str = "org.freedesktop.portal.Desktop";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";
const INTERFACE: &str = "org.freedesktop.impl.portal.FileChooser";
const REQUEST_INTERFACE: &str = "org.freedesktop.impl.portal.Request";
const REQUEST_XML: &str = r#"<node><interface name="org.freedesktop.impl.portal.Request"><method name="Close"/></interface></node>"#;

type Completion = Box<dyn FnOnce(u32, glib::Variant)>;
type Requests = Rc<RefCell<HashMap<String, Request>>>;

struct Request {
    identity: Rc<()>,
    owner: String,
    invocation: gio::DBusMethodInvocation,
    registration: gio::RegistrationId,
    window: Option<gtk::Window>,
}

struct Backend {
    bus: gio::DBusConnection,
    registration: Option<gio::RegistrationId>,
    subscription: Option<gio::SignalSubscriptionId>,
    requests: Requests,
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(subscription) = self.subscription.take() { self.bus.signal_unsubscribe(subscription); }
        let paths: Vec<_> = self.requests.borrow().keys().cloned().collect();
        for path in paths { finish(&self.bus, &self.requests, &path, 1, &glib::VariantDict::new(None).end()); }
        if let Some(registration) = self.registration.take() { let _ = self.bus.unregister_object(registration); }
    }
}

pub(crate) fn run() -> glib::ExitCode {
    // The backend must not activate the frontend that is still starting us.
    // SAFETY: main calls this before creating GTK objects or starting worker threads.
    unsafe {
        std::env::set_var("GDK_DEBUG", "no-portals");
        std::env::set_var("ADW_DISABLE_PORTAL", "1");
    }
    let app = adw::Application::builder().application_id("dev.filescope.Filescope.Picker")
        .flags(gio::ApplicationFlags::NON_UNIQUE).build();
    let service = Rc::new(RefCell::new(None::<Backend>));
    let owner = Rc::new(RefCell::new(None));
    let hold = Rc::new(RefCell::new(None));
    let failed = Rc::new(Cell::new(false));
    let (service_start, owner_start, hold_start, failed_start) = (service.clone(), owner.clone(), hold.clone(), failed.clone());
    app.connect_startup(move |app| {
        let setup = || -> Result<Backend, glib::Error> {
            let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
            let app_weak = app.downgrade();
            register(&bus, move |method, app_id, parent, title, options, complete| {
                app_weak.upgrade().map(|app| crate::window::picker::show(&app, method, app_id, parent, title, options, complete))
            })
        };
        match setup() {
            Ok(backend) => {
                *hold_start.borrow_mut() = Some(app.hold());
                let app_weak = app.downgrade();
                let failed = failed_start.clone();
                let id = gio::bus_own_name_on_connection(&backend.bus, BUS_NAME, gio::BusNameOwnerFlags::DO_NOT_QUEUE,
                    |_, _| {}, move |_, _| {
                        eprintln!("filescope: could not own the file-picker backend name");
                        failed.set(true);
                        if let Some(app) = app_weak.upgrade() { app.quit(); }
                    });
                *owner_start.borrow_mut() = Some(id);
                *service_start.borrow_mut() = Some(backend);
            }
            Err(error) => {
                eprintln!("filescope: file-picker backend: {error}");
                failed_start.set(true);
                app.quit();
            }
        }
    });
    app.connect_activate(|_| {});
    let status = app.run_with_args(&["filescope-file-picker"]);
    service.borrow_mut().take();
    if let Some(id) = owner.borrow_mut().take() { gio::bus_unown_name(id); }
    hold.borrow_mut().take();
    if failed.get() { glib::ExitCode::FAILURE } else { status }
}

fn interface_info() -> gio::DBusInterfaceInfo {
    let mut xml = format!("<node><interface name=\"{INTERFACE}\"><property name=\"version\" type=\"u\" access=\"read\"/>");
    for method in ["OpenFile", "SaveFile", "SaveFiles"] {
        xml.push_str(&format!(r#"<method name="{method}"><arg type="o" name="handle" direction="in"/><arg type="s" name="app_id" direction="in"/><arg type="s" name="parent_window" direction="in"/><arg type="s" name="title" direction="in"/><arg type="a{{sv}}" name="options" direction="in"/><arg type="u" name="response" direction="out"/><arg type="a{{sv}}" name="results" direction="out"/></method>"#));
    }
    xml.push_str("</interface></node>");
    gio::DBusNodeInfo::for_xml(&xml).expect("static FileChooser XML").lookup_interface(INTERFACE).unwrap()
}

fn trusted_sender(bus: &gio::DBusConnection, sender: &str) -> bool {
    bus.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus", "GetNameOwner",
        Some(&(FRONTEND,).to_variant()), None, gio::DBusCallFlags::NONE, 2_000, gio::Cancellable::NONE)
        .ok().and_then(|reply| reply.child_value(0).get::<String>()).as_deref() == Some(sender)
}

fn register<F>(bus: &gio::DBusConnection, show: F) -> Result<Backend, glib::Error>
where F: Fn(&str, &str, &str, &str, &glib::Variant, Completion) -> Option<gtk::Window> + 'static {
    let requests: Requests = Rc::new(RefCell::new(HashMap::new()));
    let active = Rc::downgrade(&requests);
    let request_info = gio::DBusNodeInfo::for_xml(REQUEST_XML)?.lookup_interface(REQUEST_INTERFACE).unwrap();
    let registration = bus.register_object(OBJECT_PATH, &interface_info())
        .property(|_, _, _, _, _| 4u32.to_variant())
        .method_call(move |bus, sender, _, _, method, params, invocation| {
            let Some(active) = active.upgrade() else {
                invocation.return_dbus_error("org.freedesktop.DBus.Error.Failed", "The picker is stopping");
                return;
            };
            let Some(sender) = sender.filter(|sender| trusted_sender(&bus, sender)) else {
                invocation.return_dbus_error("org.freedesktop.DBus.Error.AccessDenied", "Only the desktop portal can request files");
                return;
            };
            let handle = params.child_value(0).str().unwrap().to_owned();
            if !handle.starts_with("/org/freedesktop/portal/desktop/request/") || active.borrow().contains_key(&handle) {
                invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "Invalid or already active request handle");
                return;
            }
            let close_active = Rc::downgrade(&active);
            let close_path = handle.clone();
            let close_registration = bus.register_object(&handle, &request_info)
                .method_call(move |bus, sender, _, _, _, _, close| {
                    let Some(active) = close_active.upgrade() else {
                        close.return_value(Some(&().to_variant()));
                        return;
                    };
                    let owner = active.borrow().get(&close_path).map(|request| request.owner.clone());
                    if owner.as_deref() != sender {
                        close.return_dbus_error("org.freedesktop.DBus.Error.AccessDenied", "The request belongs to another caller");
                        return;
                    }
                    finish(&bus, &active, &close_path, 1, &glib::VariantDict::new(None).end());
                    close.return_value(Some(&().to_variant()));
                }).build();
            let registration = match close_registration {
                Ok(registration) => registration,
                Err(error) => {
                    invocation.return_dbus_error("org.freedesktop.DBus.Error.Failed", &error.to_string());
                    return;
                }
            };
            let identity = Rc::new(());
            active.borrow_mut().insert(handle.clone(), Request {
                identity: identity.clone(), owner: sender.to_owned(), invocation, registration, window: None,
            });
            let completed = Rc::downgrade(&active);
            let completed_path = handle.clone();
            let reply_bus = bus.clone();
            let complete = Box::new(move |response, results: glib::Variant| {
                if let Some(active) = completed.upgrade() {
                    let current = active.borrow().get(&completed_path).is_some_and(|request| Rc::ptr_eq(&request.identity, &identity));
                    if current { finish(&reply_bus, &active, &completed_path, response, &results); }
                }
            });
            let window = show(method, params.child_value(1).str().unwrap(), params.child_value(2).str().unwrap(),
                params.child_value(3).str().unwrap(), &params.child_value(4), complete);
            if let Some(request) = active.borrow_mut().get_mut(&handle) {
                request.window = window;
            } else if let Some(window) = window {
                window.destroy();
            }
        }).build()?;
    let active = Rc::downgrade(&requests);
    let subscription = bus.signal_subscribe(Some("org.freedesktop.DBus"), Some("org.freedesktop.DBus"),
        Some("NameOwnerChanged"), Some("/org/freedesktop/DBus"), None, gio::DBusSignalFlags::NONE,
        move |bus, _, _, _, _, params| {
            let Some((name, previous, current)) = params.get::<(String, String, String)>() else { return; };
            if previous.is_empty() || !(current.is_empty() || name == FRONTEND) { return; }
            let Some(active) = active.upgrade() else { return; };
            let paths: Vec<_> = active.borrow().iter().filter(|(_, request)| request.owner == previous)
                .map(|(path, _)| path.clone()).collect();
            for path in paths { finish(bus, &active, &path, 1, &glib::VariantDict::new(None).end()); }
        });
    Ok(Backend { bus: bus.clone(), registration: Some(registration), subscription: Some(subscription), requests })
}

fn finish(bus: &gio::DBusConnection, requests: &Requests, handle: &str, response: u32, results: &glib::Variant) {
    let request = requests.borrow_mut().remove(handle);
    if let Some(request) = request {
        let _ = bus.unregister_object(request.registration);
        request.invocation.return_value(Some(&reply_value(response, results)));
        if let Some(window) = request.window { window.destroy(); }
    }
}

fn reply_value(response: u32, results: &glib::Variant) -> glib::Variant {
    let mut response = response;
    if response == 0 {
        let uris = results.get::<HashMap<String, glib::Variant>>().and_then(|values| values.get("uris").and_then(|v| v.get::<Vec<String>>()));
        if uris.as_ref().is_none_or(|uris| uris.is_empty() || uris.iter().any(|uri| {
            !uri.starts_with("file://") || glib::filename_from_uri(uri)
                .map(|(path, host)| !path.is_absolute() || host.is_some_and(|host| !host.eq_ignore_ascii_case("localhost")))
                .unwrap_or(true)
        })) {
            response = 2;
        }
    }
    glib::Variant::tuple_from_iter([response.to_variant(), if response == 0 { results.clone() } else { glib::VariantDict::new(None).end() }])
}

#[cfg(test)]
mod portal_tests;

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn selected(uri: &str) -> glib::Variant {
        let results = glib::VariantDict::new(None);
        results.insert("uris", vec![uri]);
        results.end()
    }

    #[test]
    fn picker_cancel_returns_no_files() {
        let stale = selected("file:///tmp/previous-selection");
        for response in [1, 2] {
            let reply = reply_value(response, &stale);
            assert_eq!(reply.type_().as_str(), "(ua{sv})");
            assert_eq!(reply.child_value(0).get::<u32>(), Some(response));
            assert!(!glib::VariantDict::new(Some(&reply.child_value(1))).contains("uris"));
        }
    }

    #[test]
    fn picker_rejects_non_file_and_empty_success() {
        for invalid in [selected("https://example.com/private"), selected("file://remote.example/private"), glib::VariantDict::new(None).end()] {
            let reply = reply_value(0, &invalid);
            assert_eq!(reply.child_value(0).get::<u32>(), Some(2));
            assert!(!glib::VariantDict::new(Some(&reply.child_value(1))).contains("uris"));
        }
    }

    #[test]
    fn picker_preserves_encoded_local_uris_and_filters() {
        for uri in ["file:///tmp/a%20b%23c.txt", "file://localhost/tmp/a%20b.txt"] {
            let results = glib::VariantDict::new(Some(&selected(uri)));
            let filter = ("Text", vec![(0u32, "*.txt")]);
            results.insert("current_filter", &filter);
            let results = results.end();
            let reply = reply_value(0, &results);
            assert_eq!(reply.child_value(0).get::<u32>(), Some(0));
            assert_eq!(reply.child_value(1), results);
        }
    }
}
