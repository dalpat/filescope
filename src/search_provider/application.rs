// Register before acquiring the application bus name, so the first search
// delivered by D-Bus activation cannot race the interface export.
use adw::prelude::*;
use gtk::{gio, glib};

mod imp {
    use super::*;
    use adw::subclass::prelude::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct Application {
        registration: RefCell<Option<gio::RegistrationId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Application {
        const NAME: &'static str = "FilescopeApplication";
        type Type = super::Application;
        type ParentType = adw::Application;
    }
    impl ObjectImpl for Application {}
    impl GtkApplicationImpl for Application {}
    impl AdwApplicationImpl for Application {}
    impl ApplicationImpl for Application {
        fn dbus_register(
            &self,
            connection: &gio::DBusConnection,
            path: &str,
        ) -> Result<(), glib::Error> {
            self.parent_dbus_register(connection, path)?;
            if std::env::var_os("FILESCOPE_SHOT").is_none() {
                match crate::search_provider::register(
                    self.obj().upcast_ref::<adw::Application>(),
                    connection,
                ) {
                    Ok(id) => *self.registration.borrow_mut() = Some(id),
                    Err(error) => {
                        self.parent_dbus_unregister(connection, path);
                        return Err(error);
                    }
                }
            }
            Ok(())
        }

        fn dbus_unregister(&self, connection: &gio::DBusConnection, path: &str) {
            if let Some(id) = self.registration.borrow_mut().take() {
                let _ = connection.unregister_object(id);
            }
            self.parent_dbus_unregister(connection, path);
        }
    }
}

glib::wrapper! {
    pub struct Application(ObjectSubclass<imp::Application>)
        @extends adw::Application, gtk::Application, gio::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

pub(crate) fn new(flags: gio::ApplicationFlags) -> adw::Application {
    glib::Object::builder::<Application>()
        .property("application-id", crate::APP_ID)
        .property("flags", flags)
        .property("inactivity-timeout", 30_000u32)
        .build()
        .upcast()
}
