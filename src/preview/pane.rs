// The per-tab preview pane. Space/Sushi remains in the parent module.

use std::cell::{Cell, RefCell};
use std::io::{self, Read};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use adw::prelude::*;
use gtk::{gio, glib};

const TEXT_LIMIT: usize = 64 * 1024;
const WIDTH: i32 = 300;
const MIN_WIDTH: i32 = 180;

fn property_rows(file: &gio::File, info: &gio::FileInfo) -> Vec<(&'static str, String)> {
    let is_dir = info.file_type() == gio::FileType::Directory;
    let kind = if is_dir {
        "Folder".to_string()
    } else {
        info.content_type()
            .map(|ct| gio::content_type_get_description(&ct).to_string())
            .unwrap_or_else(|| "File".into())
    };
    let mut rows = vec![("Name", info.display_name().to_string()), ("Type", kind)];
    if !is_dir && info.has_attribute("standard::size") {
        let bytes = info.size().max(0) as u64;
        rows.push((
            "Size",
            format!("{} ({bytes} bytes)", crate::format::human_size(bytes)),
        ));
    }
    if let Some(parent) = file.parent() {
        let location = parent
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| parent.uri().to_string());
        rows.push(("Location", location));
    }
    if let Some(dt) = info.modification_date_time() {
        rows.push(("Modified", crate::format::modified(&dt)));
    }
    for (label, attr) in [("Accessed", "time::access"), ("Created", "time::created")] {
        if info.has_attribute(attr) {
            let secs = info.attribute_uint64(attr);
            if secs > 0
                && let Ok(dt) = glib::DateTime::from_unix_local(secs as i64)
            {
                rows.push((label, crate::format::modified(&dt)));
            }
        }
    }
    for (label, attr) in [("Owner", "owner::user"), ("Group", "owner::group")] {
        if let Some(value) = info.attribute_string(attr) {
            rows.push((label, value.to_string()));
        }
    }
    if info.has_attribute("unix::mode") {
        rows.push((
            "Access",
            crate::window::permission_string(info.attribute_uint32("unix::mode")),
        ));
    }
    rows
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreviewKind {
    Empty,
    Folder,
    Image,
    Text,
    Card,
}

fn preview_kind(count: u64, info: Option<&gio::FileInfo>) -> PreviewKind {
    let Some(info) = info.filter(|_| count == 1) else {
        return PreviewKind::Empty;
    };
    if info.file_type() == gio::FileType::Directory {
        return PreviewKind::Folder;
    }
    let content_type = info.content_type().unwrap_or_default();
    if content_type.starts_with("image/") {
        PreviewKind::Image
    } else if super::is_texty(&content_type) {
        PreviewKind::Text
    } else {
        PreviewKind::Card
    }
}

fn check_cancelled(cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        Err(io::Error::from(io::ErrorKind::Interrupted))
    } else {
        Ok(())
    }
}

fn read_text(mut reader: impl Read, cancelled: &AtomicBool) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    while bytes.len() < TEXT_LIMIT {
        check_cancelled(cancelled)?;
        let limit = chunk.len().min(TEXT_LIMIT - bytes.len());
        let read = reader.read(&mut chunk[..limit])?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn count_children(path: &Path, cancelled: &AtomicBool) -> io::Result<u64> {
    check_cancelled(cancelled)?;
    let mut count = 0;
    for entry in std::fs::read_dir(path)? {
        check_cancelled(cancelled)?;
        entry?; // A partial directory read must not look like an exact count.
        count += 1;
    }
    Ok(count)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Empty,
    Loading,
    Ready,
    Unavailable,
}

struct RequestState {
    generation: u64,
    visible: bool,
    phase: Phase,
}

impl RequestState {
    fn begin(&mut self, visible: bool, kind: PreviewKind) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.visible = visible;
        self.phase = if visible && kind != PreviewKind::Empty {
            Phase::Loading
        } else {
            Phase::Empty
        };
        self.generation
    }

    fn finish(&mut self, generation: u64, success: bool) -> bool {
        if !self.visible || generation != self.generation || self.phase != Phase::Loading {
            return false;
        }
        self.phase = if success {
            Phase::Ready
        } else {
            Phase::Unavailable
        };
        true
    }
}

enum Loaded {
    Folder(u64),
    Text(String),
    Image(crate::icon::Pixels),
    Card,
}

// The shared decoder takes a longest edge; convert pane width for portrait images too.
async fn image_at_width(
    path: std::path::PathBuf,
    width: i32,
    cancel: Arc<AtomicBool>,
) -> Option<crate::icon::Pixels> {
    let header_path = path.clone();
    let (_, source_width, source_height) =
        gio::spawn_blocking(move || gtk::gdk_pixbuf::Pixbuf::file_info(header_path))
            .await
            .ok()??;
    if cancel.load(Ordering::Relaxed) || source_width <= 0 {
        return None;
    }
    let edge = (i64::from(source_width.max(source_height)) * i64::from(width.min(source_width))
        / i64::from(source_width))
    .clamp(1, i64::from(i32::MAX)) as i32;
    crate::icon::decode_limited(path, edge, Some(cancel)).await
}

// GTK Box delegates allocation to its layout manager. This thin container reports
// actual width changes without polling a frame clock or decoding on every drag.
mod allocation {
    use super::*;
    use gtk::subclass::prelude::*;

    #[derive(Default)]
    pub struct PreviewBox {
        pub content: RefCell<Option<gtk::Box>>,
        pub resized: RefCell<Option<Box<dyn Fn()>>>,
        width: Cell<i32>,
    }
    #[glib::object_subclass]
    impl ObjectSubclass for PreviewBox {
        const NAME: &'static str = "FilescopePreviewBox";
        type Type = super::PreviewBox;
        type ParentType = gtk::Widget;
    }
    impl ObjectImpl for PreviewBox {
        fn constructed(&self) {
            self.parent_constructed();
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.set_parent(&*self.obj());
            *self.content.borrow_mut() = Some(content);
        }
        fn dispose(&self) {
            if let Some(content) = self.content.borrow_mut().take() {
                content.unparent();
            }
        }
    }
    impl WidgetImpl for PreviewBox {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.content.borrow().as_ref().unwrap().request_mode()
        }
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.content
                .borrow()
                .as_ref()
                .unwrap()
                .measure(orientation, for_size)
        }
        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            self.content
                .borrow()
                .as_ref()
                .unwrap()
                .allocate(width, height, baseline, None);
            if self.width.replace(width) != width {
                if let Some(resized) = self.resized.borrow().as_ref() {
                    resized();
                }
            }
        }
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj()
                .snapshot_child(self.content.borrow().as_ref().unwrap(), snapshot);
        }
    }
}

glib::wrapper! {
    pub struct PreviewBox(ObjectSubclass<allocation::PreviewBox>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl PreviewBox {
    fn append(&self, child: &impl IsA<gtk::Widget>) {
        use gtk::subclass::prelude::ObjectSubclassIsExt;
        self.imp().content.borrow().as_ref().unwrap().append(child);
    }
}

/// One load at a time per tab. Changes replace the pending target, not a queue.
pub(crate) struct Pane {
    pub(crate) widget: PreviewBox,
    pub(crate) split: gtk::Paned,
    wrap: gtk::ToggleButton,
    saved_width: Cell<i32>,
    restore_width: Cell<bool>,
    layout_queued: Cell<bool>,
    image_pixels: Cell<i32>,
    resize_timer: RefCell<Option<glib::SourceId>>,
    title: gtk::Label,
    body: gtk::ScrolledWindow,
    target: RefCell<Option<(gio::File, gio::FileInfo)>>,
    state: RefCell<RequestState>,
    running: Cell<bool>,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
}

impl Pane {
    pub(crate) fn new(visible: bool) -> Rc<Self> {
        let title = gtk::Label::new(Some("Preview"));
        title.add_css_class("heading");
        title.set_hexpand(true);
        title.set_halign(gtk::Align::Start);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_max_width_chars(28);
        let close = crate::icon::flat_button("window-close-symbolic", "Close Preview Pane");
        close.set_action_name(Some("win.preview-pane"));
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_margin_start(12);
        header.set_margin_end(6);
        header.set_margin_top(6);
        header.set_margin_bottom(6);
        header.append(&title);
        let wrap = gtk::ToggleButton::builder()
            .label("Wrap")
            .tooltip_text("Wrap text to the preview width")
            .visible(false)
            .build();
        wrap.add_css_class("flat");
        header.append(&wrap);
        header.append(&close);
        let body = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        let widget: PreviewBox = glib::Object::builder()
            .property("width-request", MIN_WIDTH)
            .property("hexpand", false)
            .property("visible", visible)
            .build();
        widget.add_css_class("fs-preview-pane");
        widget.append(&header);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&body);
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .resize_start_child(true)
            .resize_end_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .wide_handle(true)
            .hexpand(true)
            .vexpand(true)
            .end_child(&widget)
            .build();
        let pane = Rc::new(Self {
            widget,
            split: split.clone(),
            wrap: wrap.clone(),
            saved_width: Cell::new(WIDTH),
            restore_width: Cell::new(true),
            layout_queued: Cell::new(false),
            image_pixels: Cell::new(0),
            resize_timer: RefCell::new(None),
            title,
            body,
            target: RefCell::new(None),
            state: RefCell::new(RequestState {
                generation: 0,
                visible: false,
                phase: Phase::Empty,
            }),
            running: Cell::new(false),
            cancel: RefCell::new(None),
        });
        let weak = Rc::downgrade(&pane);
        pane.widget.connect_map(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.restore_width.set(true);
                pane.queue_layout();
                pane.refresh();
            }
        });
        let weak = Rc::downgrade(&pane);
        pane.widget.connect_unmap(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.refresh();
            }
        });
        let weak = Rc::downgrade(&pane);
        use gtk::subclass::prelude::ObjectSubclassIsExt;
        *pane.widget.imp().resized.borrow_mut() = Some(Box::new(move || {
            if let Some(pane) = weak.upgrade() {
                pane.queue_layout();
            }
        }));
        let weak = Rc::downgrade(&pane);
        pane.widget.connect_scale_factor_notify(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.queue_layout();
            }
        });
        let weak = Rc::downgrade(&pane);
        wrap.connect_toggled(move |_| {
            if let Some(pane) = weak.upgrade() {
                pane.apply_wrap();
            }
        });
        pane.message("Select one item to preview");
        pane
    }

    pub(crate) fn set_visible(self: &Rc<Self>, visible: bool) {
        if self.widget.is_visible() == visible {
            return;
        }
        if !visible && self.widget.is_mapped() {
            self.saved_width.set(self.widget.width());
        }
        if visible {
            self.restore_width.set(true);
        }
        self.widget.set_visible(visible);
    }

    fn queue_layout(self: &Rc<Self>) {
        if self.layout_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(pane) = weak.upgrade() else {
                return;
            };
            pane.layout_queued.set(false);
            if !pane.widget.is_mapped() || pane.split.width() <= 0 || pane.widget.width() <= 0 {
                return;
            }
            if pane.restore_width.replace(false) {
                let start = pane
                    .split
                    .start_child()
                    .map(|child| child.width())
                    .unwrap_or(0);
                let handle = (pane.split.width() - start - pane.widget.width()).max(0);
                pane.split
                    .set_position((pane.split.width() - pane.saved_width.get() - handle).max(160));
                return;
            }
            if pane.widget.width() > 0 {
                pane.saved_width.set(pane.widget.width());
                pane.resize_image();
            }
        });
    }

    fn image_width(&self) -> i32 {
        (self.widget.width().max(MIN_WIDTH) - 24) * self.widget.scale_factor().max(1)
    }

    fn resize_image(self: &Rc<Self>) {
        if preview_kind(1, self.target.borrow().as_ref().map(|(_, info)| info))
            != PreviewKind::Image
            || !self.widget.is_mapped()
        {
            return;
        }
        if let Some(picture) = self
            .body
            .child()
            .and_downcast::<gtk::Box>()
            .and_then(|content| content.first_child())
            .and_downcast::<gtk::Picture>()
        {
            self.fit_picture(&picture);
        }
        let pixels = self.image_width();
        if self.image_pixels.replace(pixels) == pixels {
            return;
        }
        if let Some(cancel) = self.cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.state.borrow_mut().begin(true, PreviewKind::Image);
        if let Some(timer) = self.resize_timer.borrow_mut().take() {
            timer.remove();
        }
        let weak = Rc::downgrade(self);
        *self.resize_timer.borrow_mut() = Some(glib::timeout_add_local_once(
            std::time::Duration::from_millis(120),
            move || {
                if let Some(pane) = weak.upgrade() {
                    pane.resize_timer.borrow_mut().take();
                    pane.start_pending();
                }
            },
        ));
    }

    fn fit_picture(&self, picture: &gtk::Picture) {
        if let Some(paintable) = picture.paintable().filter(|p| p.intrinsic_width() > 0) {
            let width = (self.widget.width() - 24).max(1);
            let height = (i64::from(width) * i64::from(paintable.intrinsic_height())
                / i64::from(paintable.intrinsic_width()))
            .clamp(1, i64::from(i32::MAX)) as i32;
            picture.set_height_request(height);
        }
    }

    fn apply_wrap(&self) {
        if let Some(view) = self.body.child().and_downcast::<gtk::TextView>() {
            view.set_wrap_mode(if self.wrap.is_active() {
                gtk::WrapMode::WordChar
            } else {
                gtk::WrapMode::None
            });
            self.body.set_hscrollbar_policy(if self.wrap.is_active() {
                gtk::PolicyType::Never
            } else {
                gtk::PolicyType::Automatic
            });
        }
    }

    pub(crate) fn set_target(self: &Rc<Self>, target: Option<(gio::File, gio::FileInfo)>) {
        if *self.target.borrow() == target {
            return;
        }
        let file_changed = match (self.target.borrow().as_ref(), target.as_ref()) {
            (Some((previous, _)), Some((next, _))) => !previous.equal(next),
            _ => true,
        };
        *self.target.borrow_mut() = target;
        if file_changed {
            self.body.hadjustment().set_value(0.0);
            self.body.vadjustment().set_value(0.0);
        }
        self.refresh();
    }

    fn refresh(self: &Rc<Self>) {
        if let Some(timer) = self.resize_timer.borrow_mut().take() {
            timer.remove();
        }
        self.image_pixels.set(self.image_width());
        if let Some(cancel) = self.cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Relaxed);
        }
        let kind = preview_kind(1, self.target.borrow().as_ref().map(|(_, info)| info));
        let visible = self.widget.is_visible() && self.widget.is_mapped();
        self.state.borrow_mut().begin(visible, kind);
        if self.state.borrow().phase == Phase::Loading {
            self.message("Loading…");
            self.start_pending();
        } else {
            self.message("Select one item to preview");
        }
    }

    fn start_pending(self: &Rc<Self>) {
        if self.running.get()
            || self.resize_timer.borrow().is_some()
            || self.state.borrow().phase != Phase::Loading
        {
            return;
        }
        let Some((file, info)) = self.target.borrow().clone() else {
            return;
        };
        let generation = self.state.borrow().generation;
        let kind = preview_kind(1, Some(&info));
        let pixels = self.image_pixels.get();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.cancel.borrow_mut() = Some(cancel.clone());
        self.running.set(true);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let loaded = if let Some(path) = file.path() {
                if kind == PreviewKind::Image {
                    image_at_width(path, pixels, cancel.clone())
                        .await
                        .map(Loaded::Image)
                } else {
                    let cancel = cancel.clone();
                    gio::spawn_blocking(move || -> io::Result<Loaded> {
                        check_cancelled(&cancel)?;
                        match kind {
                            PreviewKind::Text => {
                                let file = std::fs::File::open(path)?;
                                if !file.metadata()?.is_file() {
                                    return Err(io::Error::from(io::ErrorKind::InvalidInput));
                                }
                                read_text(file, &cancel).map(Loaded::Text)
                            }
                            PreviewKind::Folder => {
                                count_children(&path, &cancel).map(Loaded::Folder)
                            }
                            _ => {
                                std::fs::metadata(path)?;
                                Ok(Loaded::Card)
                            }
                        }
                    })
                    .await
                    .ok()
                    .and_then(Result::ok)
                }
            } else {
                None
            };
            // Properties needs attributes absent from the listing. GIO queries them
            // asynchronously, and the generation check still guards the result.
            let full =
                if !matches!(loaded, Some(Loaded::Text(_))) && !cancel.load(Ordering::Relaxed) {
                    file.query_info_future(
                        "standard::*,time::*,unix::*,owner::*",
                        gio::FileQueryInfoFlags::NONE,
                        glib::Priority::DEFAULT,
                    )
                    .await
                    .ok()
                } else {
                    None
                };
            let Some(pane) = weak.upgrade() else { return };
            pane.running.set(false);
            let accepted = pane.state.borrow_mut().finish(generation, loaded.is_some());
            if accepted {
                pane.show(&file, full.as_ref().unwrap_or(&info), loaded);
            }
            pane.start_pending();
        });
    }

    fn message(&self, text: &str) {
        self.wrap.set_visible(false);
        self.title.set_text("Preview");
        self.title.set_tooltip_text(None);
        self.body.set_hscrollbar_policy(gtk::PolicyType::Never);
        let label = gtk::Label::new(Some(text));
        label.set_wrap(true);
        label.set_margin_top(24);
        label.set_margin_start(12);
        label.set_margin_end(12);
        label.add_css_class("dim-label");
        self.body.set_child(Some(&label));
    }

    fn show(&self, file: &gio::File, info: &gio::FileInfo, loaded: Option<Loaded>) {
        self.wrap
            .set_visible(matches!(loaded, Some(Loaded::Text(_))));
        self.body.set_hscrollbar_policy(gtk::PolicyType::Never);
        self.title.set_text(&info.display_name());
        self.title.set_tooltip_text(Some(&info.display_name()));
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(12);
        content.set_margin_start(12);
        content.set_margin_end(12);
        let child_count = match &loaded {
            Some(Loaded::Folder(count)) => Some(*count),
            _ => None,
        };
        match loaded {
            Some(Loaded::Image(pixels)) => {
                let texture = pixels.texture();
                let picture = gtk::Picture::for_paintable(&texture);
                picture.set_can_shrink(true);
                picture.set_content_fit(gtk::ContentFit::Contain);
                self.fit_picture(&picture);
                content.append(&picture);
            }
            Some(Loaded::Text(text)) => {
                let view = gtk::TextView::builder()
                    .editable(false)
                    .cursor_visible(false)
                    .monospace(true)
                    .left_margin(12)
                    .right_margin(12)
                    .top_margin(12)
                    .bottom_margin(12)
                    .build();
                view.buffer().set_text(&text);
                // Direct scrolling keeps long lines from sizing the whole pane.
                self.body.set_hscrollbar_policy(gtk::PolicyType::Automatic);
                self.body.set_child(Some(&view));
                self.apply_wrap();
                return;
            }
            details => {
                if details.is_none() {
                    let note =
                        gtk::Label::new(Some("Preview unavailable — file details shown below"));
                    note.set_wrap(true);
                    note.set_max_width_chars(28);
                    note.add_css_class("dim-label");
                    content.append(&note);
                }
                if let Some(icon) = info.icon() {
                    content.append(&crate::icon::image_gicon(&icon, 64));
                }
            }
        }
        let mut rows = property_rows(file, info);
        if let Some(count) = child_count {
            rows.insert(
                2,
                (
                    "Contains",
                    format!("{count} item{}", if count == 1 { "" } else { "s" }),
                ),
            );
        }
        for (label, value) in rows {
            let row = gtk::Box::new(gtk::Orientation::Vertical, 3);
            let caption = gtk::Label::builder().label(label).xalign(0.0).build();
            caption.add_css_class("dim-label");
            caption.add_css_class("caption");
            let value = gtk::Label::builder()
                .label(value)
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(28)
                .selectable(true)
                .build();
            row.append(&caption);
            row.append(&value);
            content.append(&row);
        }
        self.body.set_child(Some(&content));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn item(kind: gio::FileType, mime: &str) -> gio::FileInfo {
        let info = gio::FileInfo::new();
        info.set_file_type(kind);
        info.set_content_type(mime);
        info
    }

    #[test]
    fn fallback_properties_include_file_details() {
        let file = gio::File::for_path("/tmp/report.pdf");
        let info = item(gio::FileType::Regular, "application/pdf");
        info.set_display_name("report.pdf");
        info.set_size(1000);
        info.set_attribute_uint64("time::modified", 1_700_000_000);
        info.set_attribute_uint64("time::access", 1_700_000_001);
        info.set_attribute_uint64("time::created", 1_700_000_002);
        info.set_attribute_string("owner::user", "alice");
        info.set_attribute_string("owner::group", "staff");
        info.set_attribute_uint32("unix::mode", 0o100640);
        let rows = property_rows(&file, &info);
        for (key, value) in [
            ("Name", "report.pdf"),
            ("Size", "1.0 kB (1000 bytes)"),
            ("Location", "/tmp"),
            ("Owner", "alice"),
            ("Group", "staff"),
            ("Access", "rw-r-----"),
        ] {
            assert!(
                rows.iter().any(|(k, v)| *k == key && v == value),
                "missing {key}"
            );
        }
        for key in ["Type", "Modified", "Accessed", "Created"] {
            assert!(
                rows.iter().any(|(k, v)| *k == key && !v.is_empty()),
                "missing {key}"
            );
        }
    }

    #[test]
    fn fallback_properties_omit_unavailable_metadata_and_folder_byte_size() {
        let info = item(gio::FileType::Directory, "inode/directory");
        info.set_display_name("Photos");
        info.set_size(4096); // This is directory storage, not the size of its contents.
        let rows = property_rows(&gio::File::for_path("/tmp/Photos"), &info);
        assert!(rows.contains(&("Type", "Folder".into())));
        for key in [
            "Size", "Modified", "Accessed", "Created", "Owner", "Group", "Access",
        ] {
            assert!(!rows.iter().any(|(k, _)| *k == key), "invented {key}");
        }
    }

    #[test]
    fn preview_kind_matches_the_selection() {
        let png = item(gio::FileType::Regular, "image/png");
        assert_eq!(preview_kind(0, None), PreviewKind::Empty);
        assert_eq!(preview_kind(2, Some(&png)), PreviewKind::Empty);
        for (kind, mime, expected) in [
            (
                gio::FileType::Directory,
                "inode/directory",
                PreviewKind::Folder,
            ),
            (gio::FileType::Regular, "image/png", PreviewKind::Image),
            (gio::FileType::Regular, "text/plain", PreviewKind::Text),
            (
                gio::FileType::Regular,
                "application/json",
                PreviewKind::Text,
            ),
            (gio::FileType::Regular, "application/pdf", PreviewKind::Card),
        ] {
            assert_eq!(preview_kind(1, Some(&item(kind, mime))), expected);
        }
    }

    #[test]
    fn text_preview_stops_at_64kib() {
        let mut bytes = vec![b'a'; TEXT_LIMIT - 1];
        bytes.extend_from_slice("€and more".as_bytes());
        let mut reader = Cursor::new(bytes);
        let text = read_text(&mut reader, &AtomicBool::new(false)).unwrap();
        assert_eq!(reader.position(), TEXT_LIMIT as u64);
        assert!(text.ends_with('\u{fffd}'));
        assert_eq!(text.chars().count(), TEXT_LIMIT);
        assert_eq!(
            read_text(Cursor::new([0xff]), &AtomicBool::new(false)).unwrap(),
            "\u{fffd}"
        );
    }

    #[test]
    fn folder_preview_counts_children_not_bytes() {
        let dir = std::env::temp_dir().join(format!("filescope-preview-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("a"), b"a").unwrap();
        std::fs::write(dir.join("b"), b"b").unwrap();
        std::fs::write(dir.join("nested/ignored"), b"c").unwrap();
        let result = count_children(&dir, &AtomicBool::new(false));
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(result.unwrap(), 3);
    }

    #[test]
    fn preview_rejects_obsolete_results() {
        let mut state = RequestState {
            generation: 0,
            visible: false,
            phase: Phase::Empty,
        };
        let a = state.begin(true, PreviewKind::Image);
        let b = state.begin(true, PreviewKind::Text);
        assert!(!state.finish(a, true));
        assert!(state.finish(b, true));
        assert_eq!(state.phase, Phase::Ready);
        let pending = state.begin(true, PreviewKind::Image);
        state.begin(false, PreviewKind::Empty); // Hiding, unmapping, or closing the tab.
        assert!(!state.finish(pending, true));
        assert_eq!(state.phase, Phase::Empty);
        let pending = state.begin(true, PreviewKind::Image);
        state.begin(true, PreviewKind::Empty); // Clearing or expanding the selection.
        assert!(!state.finish(pending, true));
    }

    #[test]
    fn preview_failure_clears_old_content() {
        let mut state = RequestState {
            generation: 0,
            visible: false,
            phase: Phase::Empty,
        };
        let first = state.begin(true, PreviewKind::Text);
        assert!(state.finish(first, true));
        let next = state.begin(true, PreviewKind::Text);
        assert_eq!(state.phase, Phase::Loading);
        assert!(state.finish(next, false));
        assert_eq!(state.phase, Phase::Unavailable);
    }

    #[test]
    fn cancelled_preview_stops_reading() {
        let mut reader = Cursor::new(b"unread");
        assert_eq!(
            read_text(&mut reader, &AtomicBool::new(true))
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(reader.position(), 0);
    }
}
