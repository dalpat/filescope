// One theme icon per name, size, and scale. The paintable is GTK's, so a
// light/dark change can recolor it. A file path is not a theme name.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Once};

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

pub(crate) fn still_icon(display: &gdk::Display, scale: i32, icon: &gio::Icon, size: i32) -> Option<gtk::IconPaintable> {
    watch_theme(display);
    let scale = scale.max(1);
    let names = themed_names(icon);
    let flags = if names.as_ref().is_some_and(|names| wants_symbolic(names)) {
        gtk::IconLookupFlags::FORCE_SYMBOLIC
    } else {
        gtk::IconLookupFlags::FORCE_REGULAR
    };
    let label = names.map(|names| names.join("\n"));
    if let Some(name) = label.as_deref()
        && let Some(hit) = THEMED.with(|cache| cache.borrow().lookup(name, size, scale))
    {
        return Some(hit);
    }
    let paintable = lookup_paintable(display, icon, size, scale, flags);
    if let Some(name) = label.as_deref() {
        THEMED.with(|cache| cache.borrow_mut().remember(name, size, scale, paintable.clone()));
    }
    Some(paintable)
}

/// Paint `icon` on `image` and keep the image so a theme change can paint it again.
pub(crate) fn show(image: &gtk::Image, display: &gdk::Display, icon: &gio::Icon, size: i32, scale: i32) {
    if let Some(paintable) = still_icon(display, scale, icon, size) {
        image.set_paintable(Some(&paintable));
    }
    unsafe {
        image.set_data(ICON_KEY, icon.clone());
        image.set_data(SIZE_KEY, size);
        image.set_data(SCALE_KEY, scale);
    }
    SHOWN.with(|shown| {
        let mut shown = shown.borrow_mut();
        if shown.iter().any(|row| row.image.upgrade().as_ref() == Some(image)) {
            return;
        }
        let weak = glib::WeakRef::new();
        weak.set(Some(image));
        shown.push(Shown { image: weak });
    });
}

pub(crate) fn still_named(display: &gdk::Display, scale: i32, name: &str, size: i32) -> Option<gtk::IconPaintable> {
    let icon = gio::ThemedIcon::new(name);
    still_icon(display, scale, icon.upcast_ref(), size)
}

fn lookup_paintable(
    display: &gdk::Display,
    icon: &gio::Icon,
    size: i32,
    scale: i32,
    flags: gtk::IconLookupFlags,
) -> gtk::IconPaintable {
    let theme = gtk::IconTheme::for_display(display);
    let paintable = theme.lookup_by_gicon(icon, size, scale, gtk::TextDirection::Ltr, flags);
    let missing = paintable
        .file()
        .and_then(|file| file.path())
        .is_some_and(|path| path.ends_with("image-missing.svg"));
    if missing && flags.contains(gtk::IconLookupFlags::FORCE_REGULAR) {
        theme.lookup_by_gicon(icon, size, scale, gtk::TextDirection::Ltr, gtk::IconLookupFlags::empty())
    } else {
        paintable
    }
}

/// Symbolic only when the preferred name says so. A mime icon lists a symbolic
/// fallback after the regular name, and that must not win.
pub(crate) fn wants_symbolic(names: &[impl AsRef<str>]) -> bool {
    names.first().is_some_and(|name| name.as_ref().ends_with("-symbolic"))
}

fn themed_names(icon: &gio::Icon) -> Option<Vec<String>> {
    let themed = icon.clone().downcast::<gio::ThemedIcon>().ok()?;
    let names: Vec<String> = themed.names().iter().map(|name| name.to_string()).collect();
    if names.is_empty() || names.iter().any(|name| name.contains('/')) {
        None
    } else {
        Some(names)
    }
}

fn themed_key(name: &str, size: i32, scale: i32) -> Option<(String, i32, i32)> {
    if name.is_empty() || name.contains('/') {
        None
    } else {
        Some((name.to_string(), size, scale))
    }
}

struct IconCache<T> {
    entries: HashMap<(String, i32, i32), T>,
}

impl<T: Clone> IconCache<T> {
    fn new() -> Self {
        Self { entries: HashMap::new() }
    }

    fn remember(&mut self, name: &str, size: i32, scale: i32, value: T) -> bool {
        let Some(key) = themed_key(name, size, scale) else { return false };
        self.entries.insert(key, value);
        true
    }

    fn lookup(&self, name: &str, size: i32, scale: i32) -> Option<T> {
        let key = themed_key(name, size, scale)?;
        self.entries.get(&key).cloned()
    }

    fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

const ICON_KEY: &str = "fs-theme-icon";
const SIZE_KEY: &str = "fs-theme-size";
const SCALE_KEY: &str = "fs-theme-scale";

struct Shown {
    image: glib::WeakRef<gtk::Image>,
}

thread_local! {
    static THEMED: RefCell<IconCache<gtk::IconPaintable>> = RefCell::new(IconCache::new());
    static SHOWN: RefCell<Vec<Shown>> = RefCell::new(Vec::new());
    static STATUS: RefCell<Vec<(glib::WeakRef<adw::StatusPage>, String, i32)>> = RefCell::new(Vec::new());
}

fn watch_theme(display: &gdk::Display) {
    static WATCH: Once = Once::new();
    WATCH.call_once(|| {
        let refresh = || refresh_shown();
        gtk::IconTheme::for_display(display).connect_changed(move |_| refresh());
        if let Some(settings) = gtk::Settings::default() {
            settings.connect_gtk_theme_name_notify(move |_| refresh());
            settings.connect_gtk_icon_theme_name_notify(move |_| refresh());
            settings.connect_gtk_application_prefer_dark_theme_notify(move |_| refresh());
        }
    });
}

fn refresh_shown() {
    THEMED.with(|cache| cache.borrow_mut().clear());
    let alive = SHOWN.with(|shown| {
        let mut shown = shown.borrow_mut();
        shown.retain(|row| row.image.upgrade().is_some());
        shown.iter().filter_map(|row| row.image.upgrade()).collect::<Vec<_>>()
    });
    for image in alive {
        // Decoded photos do not depend on the icon theme.
        if image.paintable().is_some_and(|paintable| paintable.is::<gdk::Texture>()) { continue; }
        let Some(icon) = (unsafe { image.data::<gio::Icon>(ICON_KEY) }) else { continue };
        let icon = unsafe { icon.as_ref() }.clone();
        let size = unsafe { image.data::<i32>(SIZE_KEY) }.map(|p| unsafe { *p.as_ref() }).unwrap_or(16);
        let scale = unsafe { image.data::<i32>(SCALE_KEY) }.map(|p| unsafe { *p.as_ref() }).unwrap_or(1);
        let Some(display) = gdk::Display::default() else { continue };
        if let Some(paintable) = still_icon(&display, scale, &icon, size) {
            image.set_paintable(Some(&paintable));
        }
    }
    let pages = STATUS.with(|pages| {
        let mut pages = pages.borrow_mut();
        pages.retain(|(page, _, _)| page.upgrade().is_some());
        pages.iter().filter_map(|(page, name, size)| page.upgrade().map(|page| (page, name.clone(), *size))).collect::<Vec<_>>()
    });
    for (page, name, size) in pages {
        page.set_paintable(status_paintable(&name, size).as_ref());
    }
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

pub(crate) fn cached_photo(path: &Path, size: i32) -> Option<gdk::Texture> {
    cached(&PHOTO_CACHE, path, size)
}

pub(crate) fn store_photo(path: &Path, size: i32, texture: &gdk::Texture) {
    store(&PHOTO_CACHE, path.to_path_buf(), size, texture);
}

pub(crate) fn invalidate_photo(path: &Path) {
    PHOTO_CACHE.with(|cache| cache.borrow_mut().retain(|(cached, _), _| cached != path));
}

/// Decode off the UI thread. A folder of photos used to start one decode per
/// file at once and pin every core; two at a time is enough to fill the view.
pub(crate) async fn decode_limited(path: PathBuf, size: i32, cancel: Option<Arc<AtomicBool>>) -> Option<Pixels> {
    let slot = acquire_slot(cancel.as_deref()).await?;
    gio::spawn_blocking(move || {
        // Keep the slot until the worker finishes, even if its waiter is dropped.
        let _slot = slot;
        if cancel.as_ref().is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return None;
        }
        decode_pixels(&path, size)
    }).await.ok().flatten()
}

struct Slot;

impl Drop for Slot {
    fn drop(&mut self) {
        DECODE_BUSY.fetch_sub(1, Ordering::AcqRel);
    }
}

async fn acquire_slot(cancel: Option<&AtomicBool>) -> Option<Slot> {
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return None;
        }
        let n = DECODE_BUSY.load(Ordering::Relaxed);
        if n < DECODE_SLOTS
            && DECODE_BUSY
                .compare_exchange_weak(n, n + 1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            return Some(Slot);
        }
        glib::timeout_future(std::time::Duration::from_millis(20)).await;
    }
}

thread_local! {
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

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires an isolated graphical session"]
    fn theme_refresh_preserves_photo_thumbnails() {
        use super::*;
        gtk::init().unwrap();
        let image = image_named("image-x-generic", 80);
        let pixels = glib::Bytes::from_static(&[255, 80, 20, 255]);
        let photo = gdk::MemoryTexture::new(1, 1, gdk::MemoryFormat::R8g8b8a8, &pixels, 4);
        image.set_paintable(Some(&photo));
        refresh_shown();
        assert_eq!(image.paintable(), Some(photo.upcast::<gdk::Paintable>()));
    }

    #[test]
    #[ignore = "requires an isolated graphical session"]
    fn theme_change_refreshes_images_and_status_pages() {
        use super::*;
        adw::init().unwrap();
        let root = std::env::temp_dir().join(format!("filescope-icon-themes-{}", std::process::id()));
        for (name, color) in [("Before", "red"), ("After", "blue")] {
            let dir = root.join(name);
            std::fs::create_dir_all(dir.join("scalable/actions")).unwrap();
            std::fs::write(dir.join("index.theme"), format!("[Icon Theme]\nName={name}\nDirectories=scalable/actions\n[scalable/actions]\nSize=128\nType=Scalable\nMinSize=1\nMaxSize=256\nContext=Actions\n")).unwrap();
            std::fs::write(dir.join("scalable/actions/folder-symbolic.svg"), format!("<svg xmlns='http://www.w3.org/2000/svg' width='128' height='128'><rect width='128' height='128' fill='{color}'/></svg>")).unwrap();
        }
        let display = gdk::Display::default().unwrap();
        gtk::IconTheme::for_display(&display).add_search_path(&root);
        let settings = gtk::Settings::default().unwrap();
        settings.set_gtk_icon_theme_name(Some("Before"));
        let first = image_named("folder-symbolic", 128);
        let second = image_named("folder-symbolic", 128);
        let page = adw::StatusPage::new();
        show_status(&page, "folder-symbolic", 128);
        assert_eq!(first.paintable(), second.paintable());
        assert_eq!(page.paintable(), first.paintable());
        let context = glib::MainContext::default();
        context.block_on(async {
            settings.set_gtk_icon_theme_name(Some("After"));
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
        });
        let current = status_paintable("folder-symbolic", 128).unwrap();
        assert!(current.file().unwrap().path().unwrap().starts_with(root.join("After")));
        assert_eq!(first.paintable(), Some(current.clone().upcast()));
        assert_eq!(second.paintable(), first.paintable());
        assert_eq!(page.paintable(), first.paintable());
        drop(page);
        refresh_shown();
        STATUS.with(|pages| assert!(pages.borrow().is_empty()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancelled_image_waiter_exits_before_decoder_slots_are_freed() {
        use super::*;
        use std::cell::Cell;
        use std::rc::Rc;
        use std::time::Duration;

        let context = glib::MainContext::new();
        context.block_on(async {
            let slots = Rc::new(RefCell::new(Vec::new()));
            for _ in 0..DECODE_SLOTS {
                let slot = acquire_slot(None).await.unwrap();
                slots.borrow_mut().push(slot);
            }
            let cancelled = Arc::new(AtomicBool::new(false));
            let cancel_signal = cancelled.clone();
            context.spawn_local(async move {
                glib::timeout_future(Duration::from_millis(10)).await;
                cancel_signal.store(true, Ordering::Relaxed);
            });
            let freed = Rc::new(Cell::new(false));
            let deadline_freed = freed.clone();
            let deadline_slots = slots.clone();
            let deadline = context.spawn_local(async move {
                glib::timeout_future(Duration::from_secs(1)).await;
                deadline_freed.set(true);
                deadline_slots.borrow_mut().clear();
            });
            let result = decode_limited(
                std::env::temp_dir().join("filescope-cancelled-preview.png"),
                32,
                Some(cancelled),
            ).await;
            deadline.abort();
            slots.borrow_mut().clear();
            assert!(result.is_none());
            assert!(!freed.get(), "cancelled request waited for decoder slots instead of exiting");
            assert_eq!(DECODE_BUSY.load(Ordering::Relaxed), 0);
        });
    }

    use super::{wants_symbolic, IconCache, themed_key};

    #[test]
    fn same_themed_icon_is_one_cache_entry() {
        let mut cache = IconCache::new();
        assert!(cache.remember("video-x-generic", 48, 1, ()));
        assert!(cache.lookup("video-x-generic", 48, 1).is_some());
        assert!(cache.lookup("video-x-generic", 48, 1).is_some());
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn size_and_scale_are_different_entries() {
        let mut cache = IconCache::new();
        assert!(cache.remember("video-x-generic", 48, 1, ()));
        assert!(cache.remember("video-x-generic", 48, 2, ()));
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn theme_change_drops_the_cache() {
        let mut cache = IconCache::new();
        cache.remember("video-x-generic", 48, 1, ());
        cache.clear();
        assert!(cache.lookup("video-x-generic", 48, 1).is_none());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn a_file_icon_is_not_a_themed_entry() {
        let path = "/usr/share/icons/Adwaita/scalable/mimetypes/video-x-generic.svg";
        assert!(themed_key(path, 48, 1).is_none());
        let mut cache = IconCache::new();
        assert!(!cache.remember(path, 48, 1, ()));
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn file_icon_names_are_regular_and_a_symbolic_name_is_not() {
        assert!(!wants_symbolic(&[
            "video-mp4",
            "video-x-generic",
            "video-mp4-symbolic",
            "video-x-generic-symbolic",
        ]));
        assert!(wants_symbolic(&["folder-symbolic"]));
    }
}

pub(crate) fn image_named(name: &str, size: i32) -> gtk::Image {
    let image = gtk::Image::builder().pixel_size(size).build();
    set_named(&image, name, size);
    image
}

pub(crate) fn set_named(image: &gtk::Image, name: &str, size: i32) {
    image.set_pixel_size(size);
    let Some(display) = gdk::Display::default() else { return };
    let icon = gio::ThemedIcon::new(name);
    show(image, &display, icon.upcast_ref(), size, display_scale(&display));
}

pub(crate) fn image_gicon(icon: &gio::Icon, size: i32) -> gtk::Image {
    let image = gtk::Image::builder().pixel_size(size).build();
    let Some(display) = gdk::Display::default() else { return image };
    show(&image, &display, icon, size, display_scale(&display));
    image
}

pub(crate) fn flat_button(name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_child(Some(&image_named(name, 16)));
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button
}

pub(crate) fn status_paintable(name: &str, size: i32) -> Option<gtk::IconPaintable> {
    let display = gdk::Display::default()?;
    still_named(&display, display_scale(&display), name, size)
}

pub(crate) fn show_status(page: &adw::StatusPage, name: &str, size: i32) {
    page.set_paintable(status_paintable(name, size).as_ref());
    STATUS.with(|pages| {
        let mut pages = pages.borrow_mut();
        pages.retain(|(weak, _, _)| weak.upgrade().is_some_and(|old| old != *page));
        pages.push((page.downgrade(), name.to_owned(), size));
    });
}
