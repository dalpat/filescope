// Sidebar: places, bookmarks, devices, This PC, and Trash.
// The window shell calls in here; file rows live in view.rs.

use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{bookmarks, format, trash};

// --- This PC (drives) -------------------------------------------------------

/// Populate and show the "This PC" page: quick access, drives, and recent files.
pub(super) fn show_computer(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    tab.search_bar.set_search_mode(false);
    let box_ = &tab.computer_box;
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
    box_.set_margin_top(18);
    box_.set_margin_bottom(24);
    box_.set_margin_start(22);
    box_.set_margin_end(22);
    box_.set_spacing(8);
    box_.set_vexpand(true);

    let (icon_px, tile_w, drive_icon, drive_w) = pc_scale(app.zoom.get());
    let list = app.is_list.get();
    let places = tile_row(list);
    for dir in [
        glib::UserDirectory::Desktop,
        glib::UserDirectory::Downloads,
        glib::UserDirectory::Documents,
        glib::UserDirectory::Pictures,
        glib::UserDirectory::Music,
        glib::UserDirectory::Videos,
    ] {
        let Some((_, label, path)) = xdg_place(dir) else { continue };
        let file = gio::File::for_path(&path);
        let icon = file_icon(&file);
        let app = app.clone();
        let tab = tab.clone();
        let window = app.window.clone();
        insert_tile(&places, &shortcut_card(&window, &icon, label, tile_w, icon_px, list, move || {
            super::navigation::navigate(&app, &tab, file.clone());
        }), tile_w, list);
    }
    {
        let trash = gio::File::for_uri("trash:///");
        let icon = file_icon(&trash);
        let app = app.clone();
        let tab = tab.clone();
        let window = app.window.clone();
        insert_tile(&places, &shortcut_card(&window, &icon, "Trash", tile_w, icon_px, list, move || {
            super::navigation::go_trash(&app, &tab);
        }), tile_w, list);
    }
    box_.append(&section("Quick access", &places));

    let drives_flow = tile_row(list);
    for item in drives() {
        insert_tile(&drives_flow, &drive_card(app, tab, &item, drive_w, drive_icon, list), drive_w, list);
    }
    box_.append(&section("Drives", &drives_flow));

    let recent = files_we_opened();
    if recent.is_empty() {
        let empty = gtk::Label::builder()
            .label("Files you've previously accessed will show up here")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .margin_top(48)
            .margin_bottom(48)
            .build();
        empty.add_css_class("dim-label");
        let section = section("Recent files", &empty);
        section.set_vexpand(true);
        box_.append(&section);
    } else {
        let flow = tile_row(list);
        for info in recent {
            let app = app.clone();
            let tab = tab.clone();
            let window = app.window.clone();
            let uri = info.uri().to_string();
            let file = gio::File::for_uri(&uri);
            let (icon, thumb) = file_face(&file);
            let card = shortcut_card(&window, &icon, &info.display_name(), tile_w, icon_px, list, move || {
                let file = gio::File::for_uri(&uri);
                if file.query_file_type(gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE)
                    == gio::FileType::Directory
                {
                    super::navigation::navigate(&app, &tab, file);
                } else {
                    super::commands::launch(&app, &file);
                }
            });
            if let Some(path) = info.uri_display() {
                card.set_tooltip_text(Some(&path));
            }
            if let Some(path) = file.path() {
                hide_if_missing(&card, path);
            }
            if let Some(image) = first_image(card.upcast_ref()) {
                let px = icon_px * window.scale_factor().max(1);
                if let Some(path) = thumb {
                    unsafe { card.set_data("fs-thumb", path.clone()) };
                    load_file_thumb(&image, path, px);
                } else if let Ok(info) = file.query_info(
                    "standard::*,thumbnail::path,thumbnail::is-valid",
                    gio::FileQueryInfoFlags::NONE,
                    gio::Cancellable::NONE,
                ) {
                    let output = crate::thumbnails::cache_path(
                        &glib::user_cache_dir().join("thumbnails"),
                        &file.uri(),
                    );
                    unsafe { card.set_data("fs-thumb", output) };
                    let token = crate::thumbnails::arm(&image);
                    crate::thumbnails::schedule(&image, &info, px, token);
                }
            }
            insert_tile(&flow, &card, tile_w, list);
        }
        box_.append(&section("Recent files", &flow));
    }

    if let Some(page) = tab.page.borrow().as_ref() {
        page.set_title("This PC");
    }
    tab.view_stack.set_visible_child_name("computer");
    super::navigation::set_path_visible(tab, false);
    sync_sidebar(app);
    super::update_chrome(app);
}

/// Cards keep their own size and wrap onto the next line when the window narrows.
fn tile_row(list: bool) -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(12)
        .row_spacing(if list { 4 } else { 12 })
        .max_children_per_line(if list { 1 } else { 16 })
        .min_children_per_line(1)
        .homogeneous(false)
        .hexpand(true)
        .halign(if list { gtk::Align::Fill } else { gtk::Align::Start })
        .valign(gtk::Align::Start)
        .css_classes(["pc-tiles"])
        .build()
}

fn insert_tile(flow: &gtk::FlowBox, tile: &impl IsA<gtk::Widget>, width: i32, list: bool) {
    flow.insert(tile, -1);
    if let Some(child) = flow.last_child() {
        if list {
            child.set_hexpand(true);
            child.set_halign(gtk::Align::Fill);
        } else {
            child.set_size_request(width, -1);
            child.set_hexpand(false);
            child.set_halign(gtk::Align::Start);
        }
    }
}

fn section(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Expander {
    child.set_margin_top(14);
    let exp = gtk::Expander::builder().label(title).expanded(true).child(child).hexpand(true).build();
    exp.add_css_class("pc-section");
    exp.set_margin_bottom(18);
    exp
}

/// Highlight the sidebar row for whatever the active tab is showing.
pub(super) fn sync_sidebar(app: &Rc<super::App>) {
    if app.tabs.borrow().is_empty() {
        return;
    }
    let tab = super::navigation::active_tab(app);
    let showing = tab.view_stack.visible_child_name();
    let current = tab.dir_list.file().and_then(|file| file.path());
    let list = &app.sidebar_list;
    let mut child = list.first_child();
    let mut matched = None;
    while let Some(widget) = child {
        child = widget.next_sibling();
        let Some(row) = widget.downcast_ref::<gtk::ListBoxRow>() else { continue };
        let hit = match showing.as_deref() {
            Some("computer") => unsafe { row.data::<bool>("computer") }.is_some(),
            Some("trash") => unsafe { row.data::<bool>("trash") }.is_some(),
            _ => current.as_ref().is_some_and(|path| {
                unsafe { row.data::<PathBuf>("path") }
                    .is_some_and(|stored| unsafe { stored.as_ref() } == path)
            }),
        };
        if hit {
            matched = Some(row.clone());
            break;
        }
    }
    list.select_row(matched.as_ref());
}

/// Tile sizes for the current zoom. 80 is the size the page was drawn at.
fn pc_scale(zoom: i32) -> (i32, i32, i32, i32) {
    let scale = zoom as f32 / 80.0;
    (
        (48.0 * scale).round() as i32,
        (132.0 * scale).round() as i32,
        (32.0 * scale).round() as i32,
        (300.0 * scale).round() as i32,
    )
}

/// The icon the theme gives this file. GTK picks the size from the image.
fn file_icon(file: &gio::File) -> gio::Icon {
    file_face(file).0
}

/// The file's theme icon, and whether it is a picture we can thumbnail.
fn file_face(file: &gio::File) -> (gio::Icon, Option<PathBuf>) {
    let info = file
        .query_info(
            "standard::icon,thumbnail::path,thumbnail::is-valid",
            gio::FileQueryInfoFlags::NONE,
            gio::Cancellable::NONE,
        )
        .ok();
    let icon = info
        .as_ref()
        .and_then(|info| info.icon())
        .unwrap_or_else(|| gio::ThemedIcon::new("text-x-generic").upcast());
    let thumb = info.as_ref().and_then(crate::icon::thumbnail_path);
    (icon, thumb)
}

/// Paint `icon` once, as a still texture.
///
/// A live icon paintable redraws every frame (it watches the theme and
/// recolors), which pegs a core while This PC is open.
fn theme_image(window: &adw::ApplicationWindow, icon: &gio::Icon, size: i32) -> gtk::Image {
    let image = gtk::Image::builder().pixel_size(size).halign(gtk::Align::Center).build();
    paint_icon(&image, window, icon, size);
    image
}

/// Theme icons are small files, drawn now. A photo is decoded off the UI thread,
/// the same way folder thumbnails are, so This PC does not freeze on a big jpeg.
fn paint_icon(image: &gtk::Image, window: &adw::ApplicationWindow, icon: &gio::Icon, size: i32) {
    image.set_pixel_size(size);
    if let Ok(file_icon) = icon.clone().downcast::<gio::FileIcon>()
        && let Some(path) = file_icon.file().path()
    {
        let image = image.clone();
        let px = size * window.scale_factor().max(1);
        if let Some(texture) = crate::icon::cached_photo(&path, px) {
            image.set_paintable(Some(&texture));
            return;
        }
        glib::spawn_future_local(async move {
            let Some(pixels) = crate::icon::decode_limited(path.clone(), px, None).await else {
                return;
            };
            let texture = pixels.texture();
            crate::icon::store_photo(&path, px, &texture);
            image.set_paintable(Some(&texture));
        });
        return;
    }
    crate::icon::show(image, &gtk::prelude::WidgetExt::display(window), icon, size, window.scale_factor());
}


fn load_file_thumb(image: &gtk::Image, path: std::path::PathBuf, size: i32) {
    let image = image.clone();
    let token = bump_icon(image.upcast_ref());
    if let Some(texture) = crate::icon::cached_photo(&path, size) {
        image.set_paintable(Some(&texture));
        return;
    }
    glib::spawn_future_local(async move {
        let Some(pixels) = crate::icon::decode_limited(path.clone(), size, None).await else {
            return;
        };
        if !icon_is(image.upcast_ref(), token) {
            return;
        }
        let texture = pixels.texture();
        crate::icon::store_photo(&path, size, &texture);
        image.set_paintable(Some(&texture));
    });
}

fn bump_icon(widget: &gtk::Widget) -> u64 {
    static TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let token = TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    unsafe { widget.set_data("icon-token", token) };
    token
}

fn icon_is(widget: &gtk::Widget, token: u64) -> bool {
    unsafe { widget.data::<u64>("icon-token") }
        .is_some_and(|stored| unsafe { *stored.as_ref() } == token)
}

/// A shortcut tile. The icon is the one the theme gives this file.
fn shortcut_card(
    window: &adw::ApplicationWindow,
    icon: &gio::Icon,
    label: &str,
    width: i32,
    icon_px: i32,
    list: bool,
    open: impl Fn() + 'static,
) -> gtk::Button {
    let image = theme_image(window, icon, icon_px);
    let text = gtk::Label::builder()
        .label(label)
        .xalign(if list { 0.0 } else { 0.5 })
        .justify(if list { gtk::Justification::Left } else { gtk::Justification::Center })
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .halign(if list { gtk::Align::Fill } else { gtk::Align::Center })
        .hexpand(list)
        .width_chars(if list { -1 } else { 14 })
        .max_width_chars(if list { -1 } else { 14 })
        .build();
    if !list {
        text.set_margin_top(8);
    }
    let card = gtk::Box::builder()
        .orientation(if list { gtk::Orientation::Horizontal } else { gtk::Orientation::Vertical })
        .spacing(if list { 12 } else { 0 })
        .build();
    card.append(&image);
    card.append(&text);
    let button = gtk::Button::builder()
        .child(&card)
        .width_request(if list { -1 } else { width })
        .hexpand(list)
        .build();
    button.add_css_class("flat");
    button.add_css_class("place-card");
    button.set_tooltip_text(Some(label));
    unsafe { button.set_data("fs-gicon", icon.clone()) };
    button.connect_clicked(move |_| open());
    button
}

/// Change tile size in place. Zoom must not rebuild the page.
pub(super) fn rescale_tiles(app: &Rc<super::App>, root: &impl IsA<gtk::Widget>) {
    let (icon_px, tile_w, drive_icon, drive_w) = pc_scale(app.zoom.get());
    rescale_widget(
        root.upcast_ref(),
        &app.window,
        icon_px,
        tile_w,
        drive_icon,
        drive_w,
        app.is_list.get(),
    );
}

fn rescale_widget(
    widget: &gtk::Widget,
    window: &adw::ApplicationWindow,
    icon_px: i32,
    tile_w: i32,
    drive_icon: i32,
    drive_w: i32,
    list: bool,
) {
    if let Some(button) = widget.downcast_ref::<gtk::Button>()
        && (button.has_css_class("place-card") || button.has_css_class("drive-card"))
    {
        let drive = button.has_css_class("drive-card");
        let width = if drive { drive_w } else { tile_w };
        let px = if drive { drive_icon } else { icon_px };
        if list {
            button.set_hexpand(true);
            button.set_width_request(-1);
        } else {
            button.set_hexpand(false);
            button.set_width_request(width);
        }
        if let Some(parent) = button.parent() {
            if list {
                parent.set_hexpand(true);
                parent.set_halign(gtk::Align::Fill);
                parent.set_size_request(-1, -1);
            } else {
                parent.set_size_request(width, -1);
            }
        }
        if let Some(icon) = unsafe { button.data::<gio::Icon>("fs-gicon") }
            && let Some(image) = first_image(button.upcast_ref())
        {
            paint_icon(&image, window, unsafe { icon.as_ref() }, px);
            if let Some(path) = unsafe { button.data::<PathBuf>("fs-thumb") } {
                load_file_thumb(&image, unsafe { path.as_ref().clone() }, px * window.scale_factor().max(1));
            }
        }
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        rescale_widget(&next, window, icon_px, tile_w, drive_icon, drive_w, list);
        child = next.next_sibling();
    }
}

fn first_image(widget: &gtk::Widget) -> Option<gtk::Image> {
    if let Ok(image) = widget.clone().downcast::<gtk::Image>() {
        return Some(image);
    }
    let mut child = widget.first_child();
    while let Some(next) = child {
        if let Some(image) = first_image(&next) {
            return Some(image);
        }
        child = next.next_sibling();
    }
    None
}

/// Files this app has opened, newest first. GTK's recent list is the store.
/// The page shows a handful, so missing files are only checked near the top.
fn files_we_opened() -> Vec<gtk::RecentInfo> {
    let Some(name) = glib::application_name() else { return Vec::new() };
    let mut items: Vec<_> = gtk::RecentManager::default()
        .items()
        .into_iter()
        .filter(|info| info.has_application(&name) && !info.is_private_hint())
        .collect();
    items.sort_by_key(|info| std::cmp::Reverse(info.visited().to_unix()));
    items.truncate(12);
    items
}

/// Drop a recent tile if its file is gone. The check runs off the UI thread
/// because a missing disk can stall `exists` for a long time.
fn hide_if_missing(button: &gtk::Button, path: std::path::PathBuf) {
    let button = button.clone();
    glib::spawn_future_local(async move {
        let exists = gio::spawn_blocking(move || path.exists()).await.unwrap_or(false);
        if !exists {
            button.set_visible(false);
        }
    });
}

/// Icon, label, and path for one XDG user directory, when the desktop defines it.
fn xdg_place(dir: glib::UserDirectory) -> Option<(&'static str, &'static str, PathBuf)> {
    use glib::UserDirectory::*;
    let (icon, label) = match dir {
        Desktop => ("user-desktop-symbolic", "Desktop"),
        Documents => ("folder-documents-symbolic", "Documents"),
        Downloads => ("folder-download-symbolic", "Downloads"),
        Music => ("folder-music-symbolic", "Music"),
        Pictures => ("folder-pictures-symbolic", "Pictures"),
        Videos => ("folder-videos-symbolic", "Videos"),
        _ => return None,
    };
    glib::user_special_dir(dir).map(|path| (icon, label, path))
}

// --- Trash ------------------------------------------------------------------

/// Populate and show the Trash view in `tab`: every trashed item with where it
/// came from, selection-based Restore, and Empty Trash.
pub(super) fn show_trash(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    tab.search_bar.set_search_mode(false);
    let box_ = &tab.trash_box;
    box_.insert_action_group("trash", None::<&gio::SimpleActionGroup>);
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
    box_.set_margin_top(18);
    box_.set_margin_bottom(24);
    box_.set_margin_start(22);
    box_.set_margin_end(22);
    box_.set_spacing(8);

    let root = trash::home_trash();
    let entries = trash::entries_in(&root);
    tab.trash_count.set(entries.len() as u32);

    if entries.is_empty() {
        let page = adw::StatusPage::builder()
            .title("Trash Is Empty")
            .description("Items you delete land here, and can be put back.")
            .vexpand(true)
            .build();
        crate::icon::show_status(&page, "user-trash-symbolic", 128);
        box_.append(&page);
    } else {
        let (icon_px, tile_w, _, _) = pc_scale(app.zoom.get());
        let list = app.is_list.get();
        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        bar.set_halign(gtk::Align::End);
        bar.set_margin_bottom(8);
        let restore_btn = gtk::Button::with_label("Restore");
        restore_btn.set_action_name(Some("trash.restore"));
        bar.append(&restore_btn);
        let empty_btn = gtk::Button::with_label("Empty Trash");
        empty_btn.add_css_class("destructive-action");
        let app_btn = app.clone();
        empty_btn.connect_clicked(move |_| confirm_empty_trash(&app_btn));
        bar.append(&empty_btn);
        box_.append(&bar);

        let flow = tile_row(list);
        flow.set_selection_mode(gtk::SelectionMode::Multiple);
        flow.set_activate_on_single_click(false);
        flow.add_css_class("trash-items");
        let entries = Rc::new(entries);
        let restore = gio::SimpleAction::new("restore", None);
        restore.set_enabled(false);
        {
            let app = Rc::downgrade(app);
            let tab = Rc::downgrade(tab);
            let flow = flow.downgrade();
            let entries = entries.clone();
            restore.connect_activate(move |_, _| {
                let (Some(app), Some(tab), Some(flow)) = (app.upgrade(), tab.upgrade(), flow.upgrade()) else { return; };
                let selected = trash::selected_entries(&entries, flow.selected_children().iter().map(|child| child.index() as usize));
                restore_trash_selection(&app, &tab, &selected);
            });
        }
        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&restore);
        box_.insert_action_group("trash", Some(&actions));
        flow.connect_selected_children_changed(move |flow| {
            let count = flow.selected_children().len();
            restore.set_enabled(count > 0);
            restore_btn.set_label(&if count == 0 { "Restore".into() } else { format!("Restore ({count})") });
        });
        attach_trash_menu(&flow);
        let scale = app.window.scale_factor().max(1);
        for entry in entries.iter() {
            let stored = gio::File::for_path(root.join("files").join(&entry.name));
            let (icon, thumb) = file_face(&stored);
            let label = entry
                .original
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.name.clone());
            let from = entry
                .original
                .parent()
                .and_then(|parent| parent.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| entry.original.display().to_string());
            let when = entry.deleted.as_deref().and_then(|stamp| stamp.split('T').next()).unwrap_or("");
            let tip = if when.is_empty() {
                format!("From {from}. Select, then choose Restore.")
            } else {
                format!("From {from} · {when}. Select, then choose Restore.")
            };
            let window = app.window.clone();
            let card = shortcut_card(&window, &icon, &label, tile_w, icon_px, list, || {});
            // The FlowBox owns selection and keyboard focus, not the old button.
            card.set_can_target(false);
            card.set_focusable(false);
            if let Some(path) = thumb {
                unsafe { card.set_data("fs-thumb", path.clone()) };
                if let Some(image) = first_image(card.upcast_ref()) {
                    load_file_thumb(&image, path, icon_px * scale);
                }
            }
            insert_tile(&flow, &card, tile_w, list);
            if let Some(child) = flow.last_child() {
                child.set_tooltip_text(Some(&tip));
                child.update_property(&[gtk::accessible::Property::Label(&label)]);
            }
        }
        box_.append(&flow);
    }

    if let Some(page) = tab.page.borrow().as_ref() {
        page.set_title("Trash");
    }
    tab.view_stack.set_visible_child_name("trash");
    super::navigation::set_path_visible(tab, false);
    sync_sidebar(app);
    super::update_chrome(app);
}

/// The Trash selection belongs to this view, never the hidden folder model.
pub(super) fn trash_flow(tab: &super::Tab) -> Option<gtk::FlowBox> {
    let mut child = tab.trash_box.first_child();
    while let Some(widget) = child {
        if let Some(flow) = widget.downcast_ref::<gtk::FlowBox>() {
            return Some(flow.clone());
        }
        child = widget.next_sibling();
    }
    None
}

fn attach_trash_menu(flow: &gtk::FlowBox) {
    // A popover is parented manually, so remove it before the view is disposed.
    flow.connect_unmap(|flow| unsafe {
        if let Some(popover) = flow.steal_data::<gtk::PopoverMenu>("fs-context-menu") {
            popover.unparent();
        }
    });
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_SECONDARY);
    let weak = flow.downgrade();
    click.connect_pressed(move |gesture, _, x, y| {
        let Some(flow) = weak.upgrade() else { return; };
        let Some(child) = flow.child_at_pos(x as i32, y as i32) else { return; };
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if !child.is_selected() {
            flow.unselect_all();
            flow.select_child(&child);
        }
        child.grab_focus();
        trash_context_menu(&flow, x as i32, y as i32);
    });
    flow.add_controller(click);
    let key = gtk::EventControllerKey::new();
    let weak = flow.downgrade();
    key.connect_key_pressed(move |_, key, _, modifiers| {
        if key == gdk::Key::Menu || (key == gdk::Key::F10 && modifiers.contains(gdk::ModifierType::SHIFT_MASK)) {
            if let Some(flow) = weak.upgrade() {
                let bounds = flow.selected_children().first().and_then(|child| child.compute_bounds(&flow));
                let (x, y) = bounds.map(|rect| ((rect.x() + rect.width() / 2.0) as i32, (rect.y() + rect.height()) as i32))
                    .unwrap_or((0, 0));
                trash_context_menu(&flow, x, y);
            }
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    flow.add_controller(key);
}

fn trash_context_menu(flow: &gtk::FlowBox, x: i32, y: i32) {
    unsafe {
        if let Some(old) = flow.steal_data::<gtk::PopoverMenu>("fs-context-menu") {
            old.unparent();
        }
    }
    let menu = gio::Menu::new();
    menu.append(Some("Restore"), Some("trash.restore"));
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(flow);
    popover.set_has_arrow(false);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x, y, 0, 0)));
    unsafe { flow.set_data("fs-context-menu", popover.clone()); }
    popover.popup();
}

fn restore_trash_selection(app: &Rc<super::App>, tab: &Rc<super::Tab>, entries: &[trash::Entry]) {
    if entries.is_empty() || tab.view_stack.visible_child_name().as_deref() != Some("trash") {
        return;
    }
    let results = trash::restore_selected_in(&trash::home_trash(), entries);
    let restored = results.iter().filter(|(_, result)| result.is_ok()).count();
    let failed = results.len() - restored;
    let summary = format!("Restored {restored} item{}; {failed} failed", super::plural(restored));
    let needs_details = results.iter().any(|(entry, result)| !result.as_ref().is_ok_and(|path| path == &entry.original));
    if needs_details {
        let details: Vec<_> = results.iter().map(|(entry, result)| match result {
            Ok(path) if path != &entry.original => format!("{} → {} (original name was occupied)", entry.original.display(), path.display()),
            Ok(path) => format!("Restored: {}", path.display()),
            Err(error) => format!("Couldn’t restore {}: {error}", entry.original.display()),
        }).collect();
        let dialog = adw::AlertDialog::new(Some(&summary), Some(&details.join("\n\n")));
        dialog.add_response("close", "Close");
        dialog.set_close_response("close");
        dialog.present(Some(&app.window));
    } else {
        super::toast(app, &summary);
    }
    super::commands::refresh_all(app);
}

/// Confirm, then permanently empty the Trash.
fn confirm_empty_trash(app: &Rc<super::App>) {
    let dialog = adw::AlertDialog::new(
        Some("Empty the Trash?"),
        Some("Everything in the Trash will be permanently deleted. This cannot be undone."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("empty", "Empty Trash")]);
    dialog.set_response_appearance("empty", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let app = app.clone();
    dialog.choose(&app.window.clone(), gio::Cancellable::NONE, move |resp| {
        if resp != "empty" {
            return;
        }
        match trash::empty_in(&trash::home_trash()) {
            Ok(()) => super::toast(&app, "Trash emptied"),
            Err(err) => super::toast(&app, &format!("Couldn't empty the Trash: {err}")),
        }
        super::commands::refresh_all(&app);
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
fn drive_card(
    app: &Rc<super::App>,
    tab: &Rc<super::Tab>,
    item: &DriveItem,
    width: i32,
    icon_px: i32,
    list: bool,
) -> gtk::Widget {
    let (label, icon) = match item {
        DriveItem::Mounted { name, icon, .. } => (name.as_str(), icon),
        DriveItem::Unmounted { name, icon, .. } => (name.as_str(), icon),
    };

    let image = theme_image(&app.window, icon, icon_px);
    let name = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
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
    card.append(&head);
    card.append(&bar);
    card.append(&caption);

    let button = gtk::Button::builder()
        .child(&card)
        .width_request(if list { -1 } else { width })
        .hexpand(list)
        .build();
    button.add_css_class("flat");
    button.add_css_class("drive-card");
    unsafe { button.set_data("fs-gicon", icon.clone()) };
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
    let mut places = vec![("user-home-symbolic", "Home", glib::home_dir())];
    for dir in [Desktop, Documents, Downloads, Music, Pictures, Videos] {
        if let Some(place) = xdg_place(dir) {
            places.push(place);
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
    sync_sidebar(app);
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
    let image = crate::icon::image_named(icon, 16);
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
    let image = crate::icon::image_gicon(&icon, 16);
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
                let eject = crate::icon::flat_button("media-eject-symbolic", "Unmount");
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
    let image = crate::icon::image_named("folder-symbolic", 16);
    let text = gtk::Label::builder().label(label).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let remove = crate::icon::flat_button("window-close-symbolic", "Remove bookmark");
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
