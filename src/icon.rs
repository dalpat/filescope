// Draw a theme icon once and keep the pixels.
//
// A GtkImage given an icon name or a GIcon holds a live icon paintable. That
// object redraws whenever the theme is consulted, so a window full of them
// pins a core. Callers paint the texture and drop the paintable.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use gtk::{gdk, gio, glib, prelude::*};

/// shortcut: two decodes at a time, raise if scrolling through photos feels late
const DECODE_SLOTS: u32 = 2;
static DECODE_BUSY: AtomicU32 = AtomicU32::new(0);

pub(crate) fn display_scale(display: &gdk::Display) -> i32 {
    display
        .monitors()
        .item(0)
        .and_downcast::<gdk::Monitor>()
        .map(|monitor| monitor.scale_factor())
        .unwrap_or(1)
        .max(1)
}

pub(crate) fn still_icon(display: &gdk::Display, scale: i32, icon: &gio::Icon, size: i32) -> Option<gdk::Texture> {
    texture_of(display, scale, icon, size, gtk::IconLookupFlags::FORCE_REGULAR)
}

pub(crate) fn still_named(display: &gdk::Display, scale: i32, name: &str, size: i32) -> Option<gdk::Texture> {
    let flags = if name.ends_with("-symbolic") {
        gtk::IconLookupFlags::FORCE_SYMBOLIC
    } else {
        gtk::IconLookupFlags::FORCE_REGULAR
    };
    let icon = gio::ThemedIcon::new(name);
    texture_of(display, scale, icon.upcast_ref(), size, flags)
}

fn texture_of(
    display: &gdk::Display,
    scale: i32,
    icon: &gio::Icon,
    size: i32,
    flags: gtk::IconLookupFlags,
) -> Option<gdk::Texture> {
    let scale = scale.max(1);
    let theme = gtk::IconTheme::for_display(display);
    let paintable = theme.lookup_by_gicon(icon, size, scale, gtk::TextDirection::Ltr, flags);
    let path = paintable.file().and_then(|file| file.path())?;
    let path = if path.ends_with("image-missing.svg") {
        theme
            .lookup_by_gicon(icon, size, scale, gtk::TextDirection::Ltr, gtk::IconLookupFlags::empty())
            .file()
            .and_then(|file| file.path())
            .unwrap_or(path)
    } else {
        path
    };
    let px = size * scale;
    if let Some(hit) = cached(&ICON_CACHE, &path, px) {
        return Some(hit);
    }
    let texture = pixbuf_texture(&path, px)?;
    store(&ICON_CACHE, path, px, &texture);
    Some(texture)
}

/// The small PNG already in `~/.cache/thumbnails`, when GIO says it is current.
/// The original file is never decoded for a folder icon.
pub(crate) fn thumbnail_path(info: &gio::FileInfo) -> Option<PathBuf> {
    if !info.boolean("thumbnail::is-valid") {
        return None;
    }
    let path = info.attribute_byte_string("thumbnail::path")?;
    Some(PathBuf::from(path.as_str()))
}

pub(crate) struct Pixels {
    width: i32,
    height: i32,
    stride: i32,
    has_alpha: bool,
    bytes: Vec<u8>,
}

impl Pixels {
    pub(crate) fn texture(self) -> gdk::Texture {
        let format = if self.has_alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        gdk::MemoryTexture::new(
            self.width,
            self.height,
            format,
            &glib::Bytes::from_owned(self.bytes),
            self.stride as usize,
        )
        .upcast()
    }
}

fn decode_pixels(path: &Path, size: i32) -> Option<Pixels> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, size, size, true).ok()?;
    Some(Pixels {
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride(),
        has_alpha: pixbuf.has_alpha(),
        bytes: pixbuf.read_pixel_bytes().to_vec(),
    })
}

pub(crate) fn pixbuf_texture(path: &Path, size: i32) -> Option<gdk::Texture> {
    decode_pixels(path, size).map(Pixels::texture)
}

pub(crate) fn cached_photo(path: &Path, size: i32) -> Option<gdk::Texture> {
    cached(&PHOTO_CACHE, path, size)
}

pub(crate) fn store_photo(path: &Path, size: i32, texture: &gdk::Texture) {
    store(&PHOTO_CACHE, path.to_path_buf(), size, texture);
}

/// Decode off the UI thread. A folder of photos used to start one decode per
/// file at once and pin every core; two at a time is enough to fill the view.
pub(crate) async fn decode_limited(path: PathBuf, size: i32) -> Option<Pixels> {
    let _slot = acquire_slot().await;
    gio::spawn_blocking(move || decode_pixels(&path, size)).await.ok().flatten()
}

struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        DECODE_BUSY.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn acquire_slot() -> Slot {
    loop {
        let n = DECODE_BUSY.load(Ordering::Relaxed);
        if n < DECODE_SLOTS
            && DECODE_BUSY
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return Slot;
        }
        glib::timeout_future(std::time::Duration::from_millis(20)).await;
    }
}

thread_local! {
    static ICON_CACHE: RefCell<HashMap<(PathBuf, i32), gdk::Texture>> = RefCell::new(HashMap::new());
    static PHOTO_CACHE: RefCell<HashMap<(PathBuf, i32), gdk::Texture>> = RefCell::new(HashMap::new());
}

fn cached(
    slot: &'static std::thread::LocalKey<RefCell<HashMap<(PathBuf, i32), gdk::Texture>>>,
    path: &Path,
    size: i32,
) -> Option<gdk::Texture> {
    slot.with(|cache| cache.borrow().get(&(path.to_path_buf(), size)).cloned())
}

fn store(
    slot: &'static std::thread::LocalKey<RefCell<HashMap<(PathBuf, i32), gdk::Texture>>>,
    path: PathBuf,
    size: i32,
    texture: &gdk::Texture,
) {
    slot.with(|cache| {
        let mut cache = cache.borrow_mut();
        // shortcut: drop the whole photo cache past 256 entries, rather than an LRU
        if cache.len() >= 256 {
            cache.clear();
        }
        cache.insert((path, size), texture.clone());
    });
}

pub(crate) fn image_named(name: &str, size: i32) -> gtk::Image {
    let image = gtk::Image::builder().pixel_size(size).build();
    set_named(&image, name, size);
    image
}

pub(crate) fn set_named(image: &gtk::Image, name: &str, size: i32) {
    image.set_pixel_size(size);
    let Some(display) = gdk::Display::default() else { return };
    if let Some(texture) = still_named(&display, display_scale(&display), name, size) {
        image.set_paintable(Some(&texture));
    }
}

pub(crate) fn image_gicon(icon: &gio::Icon, size: i32) -> gtk::Image {
    let image = gtk::Image::builder().pixel_size(size).build();
    let Some(display) = gdk::Display::default() else { return image };
    if let Some(texture) = still_icon(&display, display_scale(&display), icon, size) {
        image.set_paintable(Some(&texture));
    }
    image
}

pub(crate) fn flat_button(name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_child(Some(&image_named(name, 16)));
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button
}

pub(crate) fn status_paintable(name: &str, size: i32) -> Option<gdk::Texture> {
    let display = gdk::Display::default()?;
    still_named(&display, display_scale(&display), name, size)
}
