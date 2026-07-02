// Quick-look style preview: press Space on a selected item to pop a preview
// window (image, text, or a metadata card), dismissed with Space or Escape.

use adw::prelude::*;
use gtk::{gio, glib};

use crate::format;

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
