use super::*;
use std::time::{Duration, Instant};

type Pending = Rc<RefCell<Option<Result<glib::Variant, glib::Error>>>>;

fn connection() -> gio::DBusConnection {
    gio::DBusConnection::for_address_sync(
        &std::env::var("DBUS_SESSION_BUS_ADDRESS").expect("run with dbus-run-session"),
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None, gio::Cancellable::NONE,
    ).unwrap()
}

fn until(context: &glib::MainContext, ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "D-Bus request timed out");
        while context.pending() { context.iteration(false); }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn call(client: &gio::DBusConnection, dest: &str, path: &str, interface: &str, method: &str, args: &glib::Variant) -> Pending {
    let result: Pending = Rc::new(RefCell::new(None));
    let reply = result.clone();
    client.call(Some(dest), path, interface, method, Some(args), None,
        gio::DBusCallFlags::NONE, 5_000, gio::Cancellable::NONE,
        move |result| *reply.borrow_mut() = Some(result));
    result
}

fn open(client: &gio::DBusConnection, dest: &str, path: &str, title: &str) -> Pending {
    let args = glib::Variant::tuple_from_iter([
        glib::variant::ObjectPath::try_from(path).unwrap().to_variant(),
        "test.application".to_variant(), "".to_variant(), title.to_variant(),
        glib::VariantDict::new(None).end(),
    ]);
    call(client, dest, OBJECT_PATH, INTERFACE, "OpenFile", &args)
}

#[test]
#[ignore = "requires isolated D-Bus/GTK and FILESCOPE_PICKER_BINARY"]
fn picker_backend_starts_without_consuming_the_frontend() {
    use std::process::{Command, Stdio};
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    struct Service(std::process::Child);
    impl Drop for Service {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }

    let frontend = connection();
    let reply = frontend.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus",
        "org.freedesktop.DBus", "RequestName", Some(&(FRONTEND, 4u32).to_variant()), None,
        gio::DBusCallFlags::NONE, 5_000, gio::Cancellable::NONE).unwrap();
    assert_eq!(reply.child_value(0).get::<u32>(), Some(1), "run in a fresh D-Bus session");
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let filter = frontend.add_filter(move |_, message, incoming| {
        if incoming && message.message_type() == gio::DBusMessageType::MethodCall {
            observed.fetch_add(1, Ordering::Relaxed);
        }
        Some(message.clone())
    });
    let mut backend = Service(Command::new(std::env::var_os("FILESCOPE_PICKER_BINARY").expect("build the backend first"))
        .arg("--file-picker-portal").env("GDK_DEBUG", "portals").env("ADW_DISABLE_PORTAL", "0")
        .stdout(Stdio::null()).stderr(Stdio::inherit()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while Instant::now() < deadline {
        assert!(backend.0.try_wait().unwrap().is_none(), "the backend exited during startup");
        let owner = frontend.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus",
            "org.freedesktop.DBus", "NameHasOwner", Some(&(BUS_NAME,).to_variant()), None,
            gio::DBusCallFlags::NONE, 1_000, gio::Cancellable::NONE).unwrap();
        if owner.child_value(0).get::<bool>() == Some(true) { ready = true; break; }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(backend);
    frontend.remove_filter(filter);
    frontend.close_sync(gio::Cancellable::NONE).unwrap();
    assert!(ready, "backend startup depended on the frontend it is supposed to serve");
    assert_eq!(calls.load(Ordering::Relaxed), 0, "the backend must not consume desktop portals");
}

#[test]
#[ignore = "requires an isolated dbus-run-session"]
fn picker_requests_do_not_share_selection() {
    let context = glib::MainContext::new();
    context.with_thread_default(|| {
        let server = connection();
        let portal = connection();
        let outsider = connection();
        let owner_reply = portal.call_sync(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus",
            "org.freedesktop.DBus", "RequestName", Some(&(FRONTEND, 4u32).to_variant()), None,
            gio::DBusCallFlags::NONE, 5_000, gio::Cancellable::NONE).unwrap();
        assert_eq!(owner_reply.child_value(0).get::<u32>(), Some(1));
        let completions = Rc::new(RefCell::new(HashMap::<String, Completion>::new()));
        let show = completions.clone();
        let backend = register(&server, move |_, _, _, title, _, complete| {
            show.borrow_mut().insert(title.to_owned(), complete);
            None
        }).unwrap();
        let destination = server.unique_name().unwrap();
        let first_path = "/org/freedesktop/portal/desktop/request/test/first";
        let second_path = "/org/freedesktop/portal/desktop/request/test/second";

        let denied = open(&outsider, &destination, first_path, "intruder");
        until(&context, || denied.borrow().is_some());
        assert!(denied.borrow().as_ref().unwrap().is_err());
        assert!(backend.requests.borrow().is_empty());

        let first = open(&portal, &destination, first_path, "first");
        let second = open(&portal, &destination, second_path, "second");
        until(&context, || completions.borrow().len() == 2);
        assert_eq!(backend.requests.borrow().len(), 2);

        let duplicate = open(&portal, &destination, first_path, "duplicate");
        until(&context, || duplicate.borrow().is_some());
        assert!(duplicate.borrow().as_ref().unwrap().is_err());
        assert_eq!(backend.requests.borrow().len(), 2);

        let denied_close = call(&outsider, &destination, first_path, REQUEST_INTERFACE, "Close", &().to_variant());
        until(&context, || denied_close.borrow().is_some());
        assert!(denied_close.borrow().as_ref().unwrap().is_err());
        assert_eq!(backend.requests.borrow().len(), 2);

        completions.borrow_mut().remove("second").unwrap()(0, super::tests::selected("file:///tmp/second"));
        until(&context, || second.borrow().is_some());
        let second_reply = second.borrow_mut().take().unwrap().unwrap();
        assert_eq!(second_reply.child_value(0).get::<u32>(), Some(0));
        assert_eq!(glib::VariantDict::new(Some(&second_reply.child_value(1))).lookup::<Vec<String>>("uris").unwrap(),
            Some(vec!["file:///tmp/second".into()]));
        assert!(first.borrow().is_none());

        let closed = call(&portal, &destination, first_path, REQUEST_INTERFACE, "Close", &().to_variant());
        until(&context, || closed.borrow().is_some() && first.borrow().is_some());
        assert!(closed.borrow_mut().take().unwrap().is_ok());
        let first_reply = first.borrow_mut().take().unwrap().unwrap();
        assert_eq!(first_reply.child_value(0).get::<u32>(), Some(1));
        assert!(!glib::VariantDict::new(Some(&first_reply.child_value(1))).contains("uris"));
        assert!(backend.requests.borrow().is_empty());

        let after_close = call(&portal, &destination, first_path, REQUEST_INTERFACE, "Close", &().to_variant());
        until(&context, || after_close.borrow().is_some());
        assert!(after_close.borrow_mut().take().unwrap().is_err());

        // A late callback must not complete a newer request reusing the same handle.
        let replacement = open(&portal, &destination, first_path, "replacement");
        until(&context, || completions.borrow().contains_key("replacement"));
        completions.borrow_mut().remove("first").unwrap()(0, super::tests::selected("file:///tmp/stale"));
        assert!(replacement.borrow().is_none());
        assert_eq!(backend.requests.borrow().len(), 1);
        completions.borrow_mut().remove("replacement").unwrap()(0, super::tests::selected("file:///tmp/fresh"));
        until(&context, || replacement.borrow().is_some());
        let replacement_reply = replacement.borrow_mut().take().unwrap().unwrap();
        assert_eq!(glib::VariantDict::new(Some(&replacement_reply.child_value(1))).lookup::<Vec<String>>("uris").unwrap(),
            Some(vec!["file:///tmp/fresh".into()]));
        assert!(backend.requests.borrow().is_empty());

        let abandoned = open(&portal, &destination, first_path, "abandoned");
        until(&context, || backend.requests.borrow().len() == 1);
        portal.close_sync(gio::Cancellable::NONE).unwrap();
        until(&context, || backend.requests.borrow().is_empty());
        until(&context, || abandoned.borrow().is_some());
        completions.borrow_mut().clear();
        drop(backend);
        server.close_sync(gio::Cancellable::NONE).unwrap();
        outsider.close_sync(gio::Cancellable::NONE).unwrap();
    }).unwrap();
}
