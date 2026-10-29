use super::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

const FRONTEND: &str = "org.freedesktop.portal.Desktop";
const ROOT: &str = "/org/freedesktop/portal/desktop";

fn connection() -> gio::DBusConnection {
    let bus = gio::DBusConnection::for_address_sync(
        &std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None, gio::Cancellable::NONE,
    ).unwrap();
    bus.set_exit_on_close(false);
    bus
}

fn name(bus: &gio::DBusConnection, method: &str, value: &str) {
    let args = if method == "RequestName" { (value, 4u32).to_variant() } else { (value,).to_variant() };
    bus.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", "org.freedesktop.DBus", method,
        Some(&args), None, gio::DBusCallFlags::NONE, 2_000, gio::Cancellable::NONE).unwrap();
}

struct Frontend { bus: gio::DBusConnection, registration: Option<gio::RegistrationId> }

impl Frontend {
    fn new(chooser: bool) -> Self {
        let bus = connection();
        let xml = if chooser { "<node><interface name='org.freedesktop.portal.FileChooser'><property name='version' type='u' access='read'/></interface></node>" }
            else { "<node><interface name='org.freedesktop.portal.MemoryMonitor'><property name='version' type='u' access='read'/></interface></node>" };
        let node = gio::DBusNodeInfo::for_xml(xml).unwrap();
        let info = &node.interfaces()[0];
        let registration = bus.register_object(ROOT, info).property(|_, _, _, _, _| 4u32.to_variant()).build().unwrap();
        name(&bus, "RequestName", FRONTEND);
        Self { bus, registration: Some(registration) }
    }
}

impl Drop for Frontend {
    fn drop(&mut self) {
        name(&self.bus, "ReleaseName", FRONTEND);
        if let Some(registration) = self.registration.take() { let _ = self.bus.unregister_object(registration); }
        self.bus.close_sync(gio::Cancellable::NONE).unwrap();
    }
}

#[derive(Clone, Copy)]
enum Restart { Ready, Fail, NoChange, NoChooser, NoRoot }

#[test]
#[ignore = "requires a fresh dbus-run-session and FILESCOPE_PRIVATE_PORTAL_TEST=1"]
fn picker_reload_checks_idle_restarts_and_verifies_readiness() {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::env::var("FILESCOPE_PRIVATE_PORTAL_TEST").as_deref(), Ok("1"));
    let root: PathBuf = std::env::temp_dir().join(format!("filescope portal runtime {}", glib::uuid_string_random()));
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let args = root.join("arguments");
    let command = root.join("bin/systemctl");
    std::fs::write(&command, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FILESCOPE_RUNTIME_ARGUMENTS\"\nexec /usr/bin/gdbus call --session --dest dev.filescope.PortalReloadTest --object-path /test --method dev.filescope.PortalReloadTest.Restart\n").unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o755)).unwrap();
    // This isolated test sets its child-command environment before starting GIO workers.
    unsafe {
        std::env::set_var("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display()));
        std::env::set_var("FILESCOPE_RUNTIME_ARGUMENTS", &args);
    }
    let context = glib::MainContext::new();
    context.with_thread_default(|| context.block_on(async {
        assert!(!ensure_idle().await.unwrap());
        reload(true).await.unwrap();
        assert!(!args.exists(), "an inactive frontend needs no restart");

        let frontend = Rc::new(RefCell::new(Some(Frontend::new(true))));
        assert!(ensure_idle().await.unwrap());
        let interaction = gio::DBusNodeInfo::for_xml("<node><interface name='dev.filescope.Interaction'><method name='Close'/></interface></node>").unwrap();
        for kind in ["request", "session"] {
            let registration = frontend.borrow().as_ref().unwrap().bus.register_object(&format!("{ROOT}/{kind}/caller/token"), &interaction.interfaces()[0]).build().unwrap();
            assert!(ensure_idle().await.is_err(), "{kind} must block preference changes");
            assert!(reload(true).await.is_err(), "{kind} must block restart");
            assert!(!args.exists());
            frontend.borrow().as_ref().unwrap().bus.unregister_object(registration).unwrap();
            assert!(ensure_idle().await.unwrap());
        }

        let introspection = gio::DBusNodeInfo::for_xml("<node><interface name='org.freedesktop.DBus.Introspectable'><method name='Introspect'><arg type='s' direction='out'/></method></interface></node>").unwrap();
        let invalid = frontend.borrow().as_ref().unwrap().bus.register_object(ROOT, &introspection.interfaces()[0])
            .method_call(|_, _, _, _, _, _, call| call.return_value(Some(&("not XML",).to_variant()))).build().unwrap();
        assert!(ensure_idle().await.is_err(), "unreadable activity must not be treated as idle");
        assert!(reload(true).await.is_err());
        assert!(!args.exists());
        frontend.borrow().as_ref().unwrap().bus.unregister_object(invalid).unwrap();

        let control = connection();
        name(&control, "RequestName", "dev.filescope.PortalReloadTest");
        let mode = Rc::new(Cell::new(Restart::Ready));
        let active = frontend.clone(); let restart = mode.clone();
        let info = gio::DBusNodeInfo::for_xml("<node><interface name='dev.filescope.PortalReloadTest'><method name='Restart'/></interface></node>").unwrap();
        let registration = control.register_object("/test", &info.interfaces()[0]).method_call(move |_, _, _, _, _, _, call| {
            match restart.get() {
                Restart::Fail => { call.return_dbus_error("dev.filescope.Test.Failed", "fixture restart failed"); return; }
                Restart::NoChange => {},
                Restart::Ready | Restart::NoChooser | Restart::NoRoot => {
                    active.borrow_mut().take();
                    let mut frontend = Frontend::new(matches!(restart.get(), Restart::Ready));
                    if matches!(restart.get(), Restart::NoRoot) { frontend.bus.unregister_object(frontend.registration.take().unwrap()).unwrap(); }
                    *active.borrow_mut() = Some(frontend);
                }
            }
            call.return_value(Some(&().to_variant()));
        }).build().unwrap();

        reload(true).await.unwrap();
        assert_eq!(std::fs::read_to_string(&args).unwrap(), "--user\n--no-ask-password\nrestart\nxdg-desktop-portal.service\n");
        mode.set(Restart::Fail);
        assert!(reload(true).await.unwrap_err().contains("fixture restart failed"));
        mode.set(Restart::NoChange);
        assert!(reload(true).await.is_err(), "exit status alone does not prove replacement");
        mode.set(Restart::NoChooser);
        assert!(reload(true).await.is_err(), "an enabled chooser must expose FileChooser");
        reload(false).await.expect("restoring an explicit 'none' preference may remove FileChooser");
        mode.set(Restart::NoRoot);
        assert!(reload(false).await.is_err(), "even a disabled chooser needs the portal service ready");

        control.unregister_object(registration).unwrap();
        name(&control, "ReleaseName", "dev.filescope.PortalReloadTest");
        control.close_sync(gio::Cancellable::NONE).unwrap();
        frontend.borrow_mut().take();
    })).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
