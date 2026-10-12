// Sidebar: places, bookmarks, devices, This PC, and Trash.
// The window shell calls in here; file rows live in view.rs.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{bookmarks, format, trash};

// --- This PC (drives) -------------------------------------------------------

/// Populate and show the "This PC" drives view in `tab`.
pub(super) fn show_computer(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    let box_ = &tab.computer_box;
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
    box_.set_margin_top(16);
    box_.set_margin_bottom(16);
    box_.set_margin_start(16);
    box_.set_margin_end(16);
    box_.set_spacing(12);

    let heading = gtk::Label::builder().label("This PC").xalign(0.0).build();
    heading.add_css_class("title-2");
    box_.append(&heading);

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(12)
        .row_spacing(12)
        .max_children_per_line(4)
        .min_children_per_line(1)
        .homogeneous(true)
        .build();

    for item in drives() {
        flow.insert(&drive_card(app, tab, &item), -1);
    }
    box_.append(&flow);

    if let Some(page) = tab.page.borrow().as_ref() {
        page.set_title("This PC");
    }
    tab.view_stack.set_visible_child_name("computer");
    super::update_chrome(app);
}

// --- Trash ------------------------------------------------------------------

/// Populate and show the Trash view in `tab`: every trashed item with where it
/// came from, a per-item Restore, and Empty Trash.
pub(super) fn show_trash(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    let box_ = &tab.trash_box;
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
    box_.set_margin_top(16);
    box_.set_margin_bottom(16);
    box_.set_margin_start(16);
    box_.set_margin_end(16);
    box_.set_spacing(12);

    let root = trash::home_trash();
    let entries = trash::entries_in(&root);

    let heading = gtk::Label::builder().label("Trash").xalign(0.0).hexpand(true).build();
    heading.add_css_class("title-2");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    head.append(&heading);
    if !entries.is_empty() {
        let empty_btn = gtk::Button::with_label("Empty Trash");
        empty_btn.add_css_class("destructive-action");
        empty_btn.set_valign(gtk::Align::Center);
        let app = app.clone();
        let tab = tab.clone();
        empty_btn.connect_clicked(move |_| confirm_empty_trash(&app, &tab));
        head.append(&empty_btn);
    }
    box_.append(&head);

    if entries.is_empty() {
        let page = adw::StatusPage::builder()
            .icon_name("user-trash-symbolic")
            .title("Trash Is Empty")
            .description("Items you delete land here, and can be put back.")
            .vexpand(true)
            .build();
        box_.append(&page);
    } else {
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
        list.add_css_class("boxed-list");
        for entry in entries {
            list.append(&trash_entry_row(app, tab, &root, entry));
        }
        box_.append(&list);
    }

    if let Some(page) = tab.page.borrow().as_ref() {
        page.set_title("Trash");
    }
    tab.view_stack.set_visible_child_name("trash");
    super::update_chrome(app);
}

/// One row in the Trash view: name, where it came from, and Restore.
fn trash_entry_row(
    app: &Rc<super::App>,
    tab: &Rc<super::Tab>,
    root: &Path,
    entry: trash::Entry,
) -> adw::ActionRow {
    let subtitle = match &entry.deleted {
        Some(when) => format!("{} · deleted {}", entry.original.display(), when.replace('T', " ")),
        None => entry.original.display().to_string(),
    };
    let row = adw::ActionRow::builder().title(&entry.name).subtitle(&subtitle).build();
    row.add_prefix(&gtk::Image::from_icon_name("user-trash-symbolic"));

    let restore = gtk::Button::with_label("Restore");
    restore.set_valign(gtk::Align::Center);
    {
        let app = app.clone();
        let tab = tab.clone();
        let root = root.to_path_buf();
        restore.connect_clicked(move |_| match trash::restore_in(&root, &entry) {
            Ok(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                super::toast(&app, &format!("Restored “{name}”"));
                super::commands::refresh_all(&app);
                show_trash(&app, &tab);
            }
            Err(err) => super::toast(&app, &format!("Couldn't restore: {err}")),
        });
    }
    row.add_suffix(&restore);
    row
}

/// Confirm, then permanently empty the Trash.
fn confirm_empty_trash(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    let dialog = adw::AlertDialog::new(
        Some("Empty the Trash?"),
        Some("Everything in the Trash will be permanently deleted. This cannot be undone."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("empty", "Empty Trash")]);
    dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    let tab = tab.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "empty" {
            return;
        }
        match trash::empty_in(&trash::home_trash()) {
            Ok(()) => super::toast(&app, "Trash emptied"),
            Err(err) => super::toast(&app, &format!("Couldn't empty the Trash: {err}")),
        }
        show_trash(&app, &tab);
    });
}

/// The sidebar's Trash entry.
fn trash_row() -> gtk::ListBoxRow {
    let row = place_row("user-trash-symbolic", "Trash", None, false);
    unsafe { row.set_data("trash", true) };
    row
}

/// A drive tile's backing data: either a mounted location we can open directly,
/// or a connected-but-unmounted volume we mount on click.
enum DriveItem {
    /// A mounted location. `mount` is `Some` for a real volume (so it can be
    /// unmounted) and `None` for the always-present filesystem root. `usage` is
    /// `Some((used, total))` only for synthetic demo drives; real drives leave it
    /// `None` and their capacity is queried live.
    Mounted {
        name: String,
        icon: gio::Icon,
        file: gio::File,
        mount: Option<gio::Mount>,
        usage: Option<(u64, u64)>,
    },
    Unmounted { name: String, icon: gio::Icon, volume: gio::Volume },
}

/// The drives to show: the filesystem root, every mounted volume, and every
/// connected volume that isn't mounted yet (USB sticks, other partitions, …).
fn drives() -> Vec<DriveItem> {
    // Docs/screenshot mode: show synthetic drives instead of the machine's real
    // volumes, so generated screenshots never expose real drive labels.
    if std::env::var_os("FILESCOPE_DEMO").is_some() {
        return demo_drives();
    }

    let mut out: Vec<DriveItem> = Vec::new();
    out.push(DriveItem::Mounted {
        name: "Filesystem".to_string(),
        icon: fallback_icon(),
        file: gio::File::for_path("/"),
        mount: None,
        usage: None,
    });

    let monitor = gio::VolumeMonitor::get();
    for mount in monitor.mounts() {
        let file = mount.default_location();
        // Skip the root (already added) and anything without a real path.
        if file.path().as_deref() == Some(std::path::Path::new("/")) {
            continue;
        }
        out.push(DriveItem::Mounted {
            name: mount.name().to_string(),
            icon: mount.icon(),
            file,
            mount: Some(mount),
            usage: None,
        });
    }
    // Connected volumes with no mount yet — shown so the user can see and mount
    // them from here rather than hunting in another app.
    for volume in monitor.volumes() {
        if volume.get_mount().is_some() {
            continue; // already surfaced via its mount above
        }
        out.push(DriveItem::Unmounted {
            name: volume.name().to_string(),
            icon: volume.icon(),
            volume,
        });
    }
    out
}

fn fallback_icon() -> gio::Icon {
    gio::Icon::for_string("drive-harddisk-symbolic").expect("built-in icon name is valid")
}

/// Synthetic drives for screenshots (see `FILESCOPE_DEMO`). All point at the
/// demo folder so clicking works, but carry made-up names — no real volumes.
fn demo_drives() -> Vec<DriveItem> {
    let icon = |n: &str| gio::Icon::for_string(n).unwrap_or_else(|_| fallback_icon());
    let demo = std::env::var("FILESCOPE_SHOT_DIR").unwrap_or_else(|_| "/".to_string());
    let gb = 1_000_000_000u64;
    vec![
        DriveItem::Mounted {
            name: "Filesystem".to_string(),
            icon: fallback_icon(),
            file: gio::File::for_path("/"),
            mount: None,
            usage: Some((512 * gb, 1000 * gb)),
        },
        DriveItem::Mounted {
            name: "Local Disk".to_string(),
            icon: icon("drive-harddisk-symbolic"),
            file: gio::File::for_path(&demo),
            mount: None,
            usage: Some((340 * gb, 500 * gb)),
        },
        DriveItem::Mounted {
            name: "USB Drive".to_string(),
            icon: icon("drive-removable-media-symbolic"),
            file: gio::File::for_path(&demo),
            mount: None,
            usage: Some((13 * gb, 64 * gb)),
        },
    ]
}

/// One drive tile. A mounted drive shows its capacity ("X free of Y") and opens
/// on click; an unmounted volume shows a "Not mounted" hint and mounts (then
/// opens) on click.
fn drive_card(app: &Rc<super::App>, tab: &Rc<super::Tab>, item: &DriveItem) -> gtk::Widget {
    let (label, icon) = match item {
        DriveItem::Mounted { name, icon, .. } => (name.as_str(), icon),
        DriveItem::Unmounted { name, icon, .. } => (name.as_str(), icon),
    };

    let image = gtk::Image::from_gicon(icon);
    image.set_pixel_size(32);
    let name = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    name.add_css_class("heading");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    head.append(&image);
    head.append(&name);

    let bar = gtk::ProgressBar::new();
    bar.add_css_class("drive-bar");
    let caption = gtk::Label::builder().xalign(0.0).build();
    caption.add_css_class("dim-label");
    caption.add_css_class("caption");

    match item {
        DriveItem::Mounted { file, usage, .. } => {
            // Demo drives carry canned figures; real drives are queried live.
            let capacity = usage.or_else(|| {
                let info = file.query_filesystem_info("filesystem::*", gio::Cancellable::NONE).ok()?;
                let size = info.attribute_uint64("filesystem::size");
                let free = info.attribute_uint64("filesystem::free");
                (size > 0).then_some((size.saturating_sub(free), size))
            });
            if let Some((used, size)) = capacity
                && size > 0
            {
                bar.set_fraction(used as f64 / size as f64);
                caption.set_label(&format!(
                    "{} free of {}",
                    format::human_size(size - used),
                    format::human_size(size)
                ));
            }
        }
        DriveItem::Unmounted { .. } => {
            bar.set_visible(false);
            caption.set_label("Not mounted — click to mount");
        }
    }

    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    card.add_css_class("drive-card");
    card.add_css_class("card");
    card.append(&head);
    card.append(&bar);
    card.append(&caption);

    let button = gtk::Button::builder().child(&card).build();
    button.add_css_class("flat");
    {
        let app = app.clone();
        let tab = tab.clone();
        match item {
            DriveItem::Mounted { file, .. } => {
                let file = file.clone();
                button.connect_clicked(move |_| super::navigation::navigate(&app, &tab, file.clone()));
            }
            DriveItem::Unmounted { volume, .. } => {
                let volume = volume.clone();
                button.connect_clicked(move |_| mount_and_open(&app, &tab, &volume));
            }
        }
    }
    button.upcast()
}

/// Unmount `mount`. If a tab is currently inside it, send that tab Home first so
/// it isn't left showing a directory that's about to disappear. Runs async.
fn unmount(app: &Rc<super::App>, mount: &gio::Mount) {
    // Any tab sitting inside this mount gets sent Home before it vanishes.
    if let Some(root) = mount.default_location().path() {
        for tab in app.tabs.borrow().iter() {
            if tab.dir_list.file().and_then(|f| f.path()).is_some_and(|p| p.starts_with(&root)) {
                let tab = tab.clone();
                let app = app.clone();
                super::navigation::navigate(&app, &tab, gio::File::for_path(glib::home_dir()));
            }
        }
    }

    let op = gio::MountOperation::new();
    let name = mount.name().to_string();
    let app = app.clone();
    mount.unmount_with_operation(
        gio::MountUnmountFlags::NONE,
        Some(&op),
        gio::Cancellable::NONE,
        move |res| match res {
            Ok(()) => {
                populate_sidebar(&app);
                super::toast(&app, &format!("Unmounted {name}"));
            }
            Err(err) => {
                if !err.matches(gio::IOErrorEnum::FailedHandled) {
                    super::toast(&app, &format!("Couldn't unmount {name}: {err}"));
                }
            }
        },
    );
}

/// Mount `volume`, then open it in `tab`. Runs asynchronously; the platform
/// shows a prompt (for passwords etc.) when the volume needs one.
pub(super) fn mount_and_open(app: &Rc<super::App>, tab: &Rc<super::Tab>, volume: &gio::Volume) {
    let op = gio::MountOperation::new();
    let app = app.clone();
    let tab = tab.clone();
    let volume = volume.clone();
    volume.clone().mount(
        gio::MountMountFlags::NONE,
        Some(&op),
        gio::Cancellable::NONE,
        move |res| match res {
            Ok(()) => {
                populate_sidebar(&app);
                match volume.get_mount() {
                    Some(mount) => super::navigation::navigate(&app, &tab, mount.default_location()),
                    None => super::toast(&app, "Mounted, but couldn't locate the drive"),
                }
            }
            Err(err) => {
                // The user dismissing the mount prompt reports FailedHandled.
                if err.matches(gio::IOErrorEnum::FailedHandled) {
                    return;
                }
                // GIO couldn't mount it (commonly an NTFS volume on a system
                // without gvfs/udisks NTFS support) — fall back to ntfs-3g.
                ntfs3g_fallback(&app, &tab, &volume, &err);
            }
        },
    );
}

/// Mount `volume` with ntfs-3g directly, then open it. Used when GIO's own mount
/// fails — chiefly for NTFS partitions. Prefers `pkexec` so no terminal is
/// needed; the mount is owned by the current user (uid/gid options) and mounted
/// under a user-writable point in the runtime dir. Runs asynchronously.
fn ntfs3g_fallback(app: &Rc<super::App>, tab: &Rc<super::Tab>, volume: &gio::Volume, err: &glib::Error) {
    let Some(device) = volume.identifier("unix-device") else {
        super::toast(app, &format!("Couldn't mount: {err}"));
        return;
    };
    if !super::has_binary("ntfs-3g") {
        super::toast(app, "This drive needs NTFS support — install the ntfs-3g package.");
        return;
    }

    // A user-owned mount point: <runtime>/filescope-mounts/<sanitized name>.
    let safe: String = volume
        .name()
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let mut mount_point = glib::user_runtime_dir();
    mount_point.push("filescope-mounts");
    mount_point.push(if safe.is_empty() { "drive".to_string() } else { safe });
    if let Err(e) = std::fs::create_dir_all(&mount_point) {
        super::toast(app, &format!("Couldn't prepare a mount point: {e}"));
        return;
    }

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };
    let options = format!("uid={uid},gid={gid},umask=022");

    // pkexec raises a graphical auth prompt; without it, try ntfs-3g directly
    // (works when it is configured to let the user mount).
    let mut argv: Vec<std::ffi::OsString> = Vec::new();
    if super::has_binary("pkexec") {
        argv.push("pkexec".into());
    }
    argv.push("ntfs-3g".into());
    argv.push("-o".into());
    argv.push(options.into());
    argv.push(device.as_str().into());
    argv.push(mount_point.clone().into_os_string());
    let argv_refs: Vec<&std::ffi::OsStr> = argv.iter().map(|s| s.as_os_str()).collect();

    match gio::Subprocess::newv(&argv_refs, gio::SubprocessFlags::NONE) {
        Ok(process) => {
            let app = app.clone();
            let tab = tab.clone();
            process.wait_check_async(gio::Cancellable::NONE, move |res| match res {
                Ok(()) => super::navigation::navigate(&app, &tab, gio::File::for_path(&mount_point)),
                Err(e) => super::toast(&app, &format!("ntfs-3g couldn't mount the drive: {e}")),
            });
        }
        Err(e) => super::toast(app, &format!("Couldn't run ntfs-3g: {e}")),
    }
}

pub(super) fn bookmark_current(app: &Rc<super::App>) {
    let Some(path) = super::navigation::active_tab(app).dir_list.file().and_then(|f| f.path()) else { return };
    {
        let mut marks = app.bookmarks.borrow_mut();
        if marks.contains(&path) {
            super::toast(app, "Already bookmarked");
            return;
        }
        marks.push(path.clone());
        bookmarks::save(&marks);
    }
    populate_sidebar(app);
    super::toast(app, "Bookmark added");
}

fn remove_bookmark(app: &Rc<super::App>, path: &PathBuf) {
    {
        let mut marks = app.bookmarks.borrow_mut();
        marks.retain(|p| p != path);
        bookmarks::save(&marks);
    }
    populate_sidebar(app);
}
// --- Sidebar ----------------------------------------------------------------

pub(super) fn populate_sidebar(app: &Rc<super::App>) {
    let list = &app.sidebar_list;
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    // Places.
    use glib::UserDirectory::*;
    let mut places: Vec<(&str, String, PathBuf)> =
        vec![("user-home-symbolic", "Home".into(), glib::home_dir())];
    for (icon, label, dir) in [
        ("user-desktop-symbolic", "Desktop", Desktop),
        ("folder-documents-symbolic", "Documents", Documents),
        ("folder-download-symbolic", "Downloads", Downloads),
        ("folder-music-symbolic", "Music", Music),
        ("folder-pictures-symbolic", "Pictures", Pictures),
        ("folder-videos-symbolic", "Videos", Videos),
    ] {
        if let Some(path) = glib::user_special_dir(dir) {
            places.push((icon, label.into(), path));
        }
    }
    for (icon, label, path) in places {
        list.append(&place_row(icon, &label, Some(path), false));
    }

    // This PC (special row → drives view).
    list.append(&place_row("computer-symbolic", "This PC", None, true));
    // Trash — browse what you've deleted, restore it, or empty it.
    list.append(&trash_row());

    // Devices: the filesystem root and each drive/partition, mounted or not.
    // Mounted ones open on click and carry an eject button; unmounted ones mount
    // on click.
    let items = drives();
    if items.len() > 1 {
        list.append(&section_header("Devices"));
        for item in items {
            list.append(&device_row(app, item));
        }
    }

    // Bookmarks.
    let marks = app.bookmarks.borrow().clone();
    if !marks.is_empty() {
        list.append(&section_header("Bookmarks"));
        for path in marks {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            list.append(&bookmark_row(app, &name, path));
        }
    }
}

fn section_header(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder().label(text).xalign(0.0).build();
    label.add_css_class("dim-label");
    label.add_css_class("caption-heading");
    label.set_margin_top(10);
    label.set_margin_bottom(2);
    label.set_margin_start(12);
    gtk::ListBoxRow::builder().child(&label).selectable(false).activatable(false).build()
}

fn place_row(icon: &str, label: &str, path: Option<PathBuf>, computer: bool) -> gtk::ListBoxRow {
    let image = gtk::Image::from_icon_name(icon);
    let text = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build();
    let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.set_margin_start(6);
    row_box.set_margin_end(6);
    row_box.append(&image);
    row_box.append(&text);
    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    if let Some(path) = path {
        unsafe { row.set_data("path", path) };
    }
    if computer {
        unsafe { row.set_data("computer", true) };
    }
    row
}

/// A sidebar device row. Mounted → opens on click, with an eject button that
/// unmounts. Unmounted → mounts (then opens) on click.
fn device_row(app: &Rc<super::App>, item: DriveItem) -> gtk::ListBoxRow {
    let (label, icon) = match &item {
        DriveItem::Mounted { name, icon, .. } => (name.clone(), icon.clone()),
        DriveItem::Unmounted { name, icon, .. } => (name.clone(), icon.clone()),
    };
    let image = gtk::Image::from_gicon(&icon);
    let text = gtk::Label::builder()
        .label(&label)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.set_margin_start(6);
    row_box.set_margin_end(6);
    row_box.append(&image);
    row_box.append(&text);

    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    match item {
        DriveItem::Mounted { file, mount, .. } => {
            if let Some(path) = file.path() {
                unsafe { row.set_data("path", path) };
            }
            // Real volumes get an eject/unmount button; the root does not.
            if let Some(mount) = mount {
                text.set_tooltip_text(None);
                let eject = gtk::Button::builder()
                    .icon_name("media-eject-symbolic")
                    .tooltip_text("Unmount")
                    .build();
                eject.add_css_class("flat");
                eject.add_css_class("bookmark-remove");
                let app = app.clone();
                eject.connect_clicked(move |_| unmount(&app, &mount));
                row_box.append(&eject);
            }
        }
        DriveItem::Unmounted { volume, .. } => {
            text.add_css_class("dim-label");
            unsafe { row.set_data("volume", volume) };
        }
    }
    row
}

fn bookmark_row(app: &Rc<super::App>, label: &str, path: PathBuf) -> gtk::ListBoxRow {
    let image = gtk::Image::from_icon_name("folder-symbolic");
    let text = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let remove = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Remove bookmark").build();
    remove.add_css_class("flat");
    remove.add_css_class("bookmark-remove");
    remove.add_css_class("bookmark-x");
    {
        let app = app.clone();
        let path = path.clone();
        remove.connect_clicked(move |_| remove_bookmark(&app, &path));
    }
    let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    row_box.set_margin_top(6);
    row_box.set_margin_bottom(6);
    row_box.set_margin_start(6);
    row_box.set_margin_end(6);
    row_box.append(&image);
    row_box.append(&text);
    row_box.append(&remove);
    let row = gtk::ListBoxRow::builder().child(&row_box).build();
    unsafe { row.set_data("path", path.clone()) };
    attach_bookmark_reorder(app, &row, path);
    row
}

/// Drag a bookmark row onto another to reorder the list. The drop lands before
/// or after the target depending on which half of the row the pointer is in.
fn attach_bookmark_reorder(app: &Rc<super::App>, row: &gtk::ListBoxRow, path: PathBuf) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::MOVE);
    let dragged = path.to_string_lossy().into_owned();
    source.connect_prepare(move |_, _, _| {
        Some(gdk::ContentProvider::for_value(&dragged.to_value()))
    });
    row.add_controller(source);

    let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    {
        let row = row.clone();
        target.connect_motion(move |_, _, _| {
            row.add_css_class("bookmark-drop");
            gdk::DragAction::MOVE
        });
    }
    {
        let row = row.clone();
        target.connect_leave(move |_| row.remove_css_class("bookmark-drop"));
    }
    {
        let app = app.clone();
        let row = row.clone();
        target.connect_drop(move |_, value, _, y| {
            row.remove_css_class("bookmark-drop");
            let Ok(from_s) = value.get::<String>() else { return false };
            let from = PathBuf::from(from_s);
            if !app.bookmarks.borrow().iter().any(|p| p == &from) {
                return false;
            }
            let after = y > row.height() as f64 / 2.0;
            let to = path.clone();
            let app = app.clone();
            // Rebuild after this drop returns — the sidebar rows are the drop
            // widgets, so tearing them down inside the callback is unsafe.
            glib::idle_add_local_once(move || {
                let changed = {
                    let mut marks = app.bookmarks.borrow_mut();
                    let changed = bookmarks::reorder(&mut marks, &from, &to, after);
                    if changed {
                        bookmarks::save(&marks);
                    }
                    changed
                };
                if changed {
                    populate_sidebar(&app);
                }
            });
            true
        });
    }
    row.add_controller(target);
}
