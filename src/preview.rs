// Quick-look style preview: press Space on a selected item.
// GNOME Sushi handles it when installed; otherwise a small in-app window
// (image, text, or a metadata card), dismissed with Space or Escape.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::format;

const SUSHI_NAME: &str = "org.gnome.NautilusPreviewer";
const SUSHI_PATH: &str = "/org/gnome/NautilusPreviewer";
const SUSHI_IFACE: &str = "org.gnome.NautilusPreviewer2";
const SUSHI_HANDLE_KEY: &str = "sushi-wl-handle";

/// Ask GNOME Sushi to preview `file`. `done(true)` when Sushi accepted the call.
/// A false result means Sushi is missing or the call failed, so the caller can
/// fall back to [`show`].
pub fn show_sushi(window: &adw::ApplicationWindow, file: &gio::File, done: impl FnOnce(bool) + 'static) {
    let uri = file.uri().to_string();
    let token = gtk::prelude::WidgetExt::display(window)
        .app_launch_context()
        .startup_notify_id(None::<&gio::AppInfo>, &[])
        .map(|id: glib::GString| id.to_string())
        .unwrap_or_default();
    if let Some(raw) = saved_wayland_handle(window) {
        call_show_file(uri, format!("wayland:{raw}"), token, false, done);
        return;
    }
    let Some(toplevel) = wayland_toplevel(window) else {
        done(false);
        return;
    };
    // The export callback owns `done`. If export fails immediately, call it here.
    let done = Rc::new(RefCell::new(Some(done)));
    let window = window.clone();
    let started = toplevel.export_handle({
        let done = done.clone();
        move |_, result: Result<&str, glib::BoolError>| {
            let Some(done) = done.borrow_mut().take() else { return };
            match result {
                Ok(raw) => {
                    let raw = raw.to_string();
                    let handle = format!("wayland:{raw}");
                    unsafe { window.set_data(SUSHI_HANDLE_KEY, raw) };
                    call_show_file(uri.clone(), handle, token.clone(), false, done);
                }
                Err(_) => done(false),
            }
        }
    });
    if !started {
        if let Some(done) = done.borrow_mut().take() {
            done(false);
        }
    }
}

/// Close Sushi if it is already running. Does not start it.
pub fn close_sushi(window: &adw::ApplicationWindow) {
    drop_saved_wayland_handle(window);
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, |bus| {
        let Ok(bus) = bus else { return };
        bus.call(
            Some(SUSHI_NAME),
            SUSHI_PATH,
            SUSHI_IFACE,
            "Close",
            None,
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            -1,
            gio::Cancellable::NONE,
            |_| {},
        );
    });
}

/// `token` is `Some` for the current `(ssbs)` call and `None` for older Sushi `(ssb)`.
fn show_file_parameters(uri: &str, handle: &str, close: bool, token: Option<&str>) -> glib::Variant {
    match token {
        Some(token) => (uri, handle, close, token).to_variant(),
        None => (uri, handle, close).to_variant(),
    }
}

fn call_show_file(
    uri: String,
    handle: String,
    token: String,
    legacy: bool,
    done: impl FnOnce(bool) + 'static,
) {
    let params = show_file_parameters(&uri, &handle, true, (!legacy).then_some(token.as_str()));
    gio::bus_get(gio::BusType::Session, gio::Cancellable::NONE, move |bus| {
        let bus = match bus {
            Ok(bus) => bus,
            Err(_) => {
                done(false);
                return;
            }
        };
        bus.call(
            Some(SUSHI_NAME),
            SUSHI_PATH,
            SUSHI_IFACE,
            "ShowFile",
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            move |res| match res {
                Ok(_) => done(true),
                // Sushi before the activation-token argument rejects `(ssbs)`.
                Err(err) if !legacy && err.matches(gio::DBusError::InvalidArgs) => {
                    call_show_file(uri, handle, token, true, done);
                }
                Err(_) => done(false),
            },
        );
    });
}

fn wayland_toplevel(window: &adw::ApplicationWindow) -> Option<gdk_wayland::WaylandToplevel> {
    gtk::prelude::NativeExt::surface(window)?.downcast().ok()
}

fn saved_wayland_handle(window: &adw::ApplicationWindow) -> Option<String> {
    let data = unsafe { window.data::<String>(SUSHI_HANDLE_KEY) }?;
    Some(unsafe { data.as_ref() }.clone())
}

fn drop_saved_wayland_handle(window: &adw::ApplicationWindow) {
    let Some(raw) = (unsafe { window.steal_data::<String>(SUSHI_HANDLE_KEY) }) else {
        return;
    };
    if let Some(toplevel) = wayland_toplevel(window) {
        toplevel.drop_exported_handle(&raw);
    }
}

/// Build and present a preview window for `file`/`info`, transient for `parent`.
/// Returns the window so the caller can close it (Space toggles the preview).
pub fn show(
    parent: &adw::ApplicationWindow,
    file: &gio::File,
    info: &gio::FileInfo,
) -> adw::Window {
    let name = info.display_name().to_string();
    let content_type = info.content_type().map(|c| c.to_string()).unwrap_or_default();

    let body: gtk::Widget = if content_type.starts_with("image/") {
        let picture = gtk::Picture::for_file(file);
        picture.set_vexpand(true);
        picture.set_hexpand(true);
        picture.upcast()
    } else if is_texty(&content_type) {
        text_preview(file)
    } else {
        metadata_card(info, &content_type)
    };

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new(&name, "")));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));

    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(false)
        .default_width(760)
        .default_height(620)
        .title(&name)
        .content(&toolbar)
        .build();

    // Space or Escape closes the preview (Space is the toggle, like macOS).
    let key = gtk::EventControllerKey::new();
    let win = window.clone();
    key.connect_key_pressed(move |_, k, _, _| {
        if matches!(k, gtk::gdk::Key::space | gtk::gdk::Key::Escape) {
            win.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key);

    window.present();
    window
}

/// True for text-ish content types worth showing as text.
fn is_texty(content_type: &str) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-shellscript"
                | "application/x-desktop"
        )
}

/// A scrolled, monospaced view of the file's first chunk of text.
fn text_preview(file: &gio::File) -> gtk::Widget {
    let text = file
        .path()
        .and_then(|p| std::fs::read(p).ok())
        .map(|bytes| {
            let cut = bytes.len().min(256 * 1024);
            String::from_utf8_lossy(&bytes[..cut]).into_owned()
        })
        .unwrap_or_default();

    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&text);
    let view = gtk::TextView::builder()
        .buffer(&buffer)
        .editable(false)
        .monospace(true)
        .cursor_visible(false)
        .left_margin(12)
        .right_margin(12)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    gtk::ScrolledWindow::builder().vexpand(true).hexpand(true).child(&view).build().upcast()
}

/// A centered card for files with no inline preview: big icon, name, type, size.
fn metadata_card(info: &gio::FileInfo, content_type: &str) -> gtk::Widget {
    let icon = gtk::Image::builder().pixel_size(128).build();
    if let Some(gicon) = info.icon() {
        icon.set_from_gicon(&gicon);
    }
    let kind = if content_type.is_empty() {
        "File".to_string()
    } else {
        gio::content_type_get_description(content_type).to_string()
    };
    let size = if info.file_type() == gio::FileType::Directory {
        "Folder".to_string()
    } else {
        format::human_size(info.size().max(0) as u64)
    };
    let title = gtk::Label::new(Some(&info.display_name()));
    title.add_css_class("title-2");
    let caption = gtk::Label::new(Some(&format!("{kind} · {size}")));
    caption.add_css_class("dim-label");

    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    card.set_valign(gtk::Align::Center);
    card.set_halign(gtk::Align::Center);
    card.set_vexpand(true);
    card.append(&icon);
    card.append(&title);
    card.append(&caption);
    card.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sushi_show_file_matches_current_and_legacy_signatures() {
        let current = show_file_parameters("file:///tmp/a", "wayland:1", true, Some("tok"));
        assert_eq!(current.type_().as_str(), "(ssbs)");
        let legacy = show_file_parameters("file:///tmp/a", "wayland:1", true, None);
        assert_eq!(legacy.type_().as_str(), "(ssb)");
    }
}
