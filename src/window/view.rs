// The folder grid and list: icons, columns, keyboard, drag-and-drop, inline rename.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::{fileops, format, ops, undo};

/// List-view icon size derived from the grid zoom.
pub(super) fn list_icon(grid_size: i32) -> i32 {
    (grid_size / 4).clamp(16, 40)
}
pub(super) fn attach_context_menu(app: &Rc<super::App>, widget: &impl IsA<gtk::Widget>) {
    let popover = gtk::PopoverMenu::from_model(Some(&super::context_menu()));
    popover.set_parent(widget);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    let app = app.clone();
    let popover = popover.clone();
    gesture.connect_pressed(move |g, _, x, y| {
        g.set_state(gtk::EventSequenceState::Claimed);
        super::update_chrome(&app);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    });
    widget.add_controller(gesture);
}

// --- Drag & drop ------------------------------------------------------------

/// Keys that act on the file view: Space previews, Backspace goes up, Delete
/// trashes (Shift+Delete deletes).
///
/// These are deliberately **not** window accelerators. GTK dispatches accelerators
/// before the focused widget, so binding bare keys globally would steal them from
/// any text entry — you couldn't type a space or backspace while renaming or
/// searching.
///
/// The controller runs in the capture phase, before the list item's own binding.
/// That binding uses bare Space to toggle selection, so a bubble-phase handler
/// never sees the key. Text entries inside the view (inline rename) are left
/// alone; modified Space still reaches GTK so Ctrl/Shift+Space can change the
/// selection.
pub(super) fn attach_view_keys(app: &Rc<super::App>, view: &impl IsA<gtk::Widget>) {
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let app = app.clone();
    key.connect_key_pressed(move |controller, keyval, _, state| {
        let typing = controller.widget().is_some_and(|widget| focus_is_text_input(&widget));
        if !claims_view_key(keyval, state, typing) {
            return glib::Propagation::Proceed;
        }
        match keyval {
            gtk::gdk::Key::space | gtk::gdk::Key::KP_Space => super::commands::toggle_preview(&app),
            gtk::gdk::Key::BackSpace => {
                let tab = super::navigation::active_tab(&app);
                super::navigation::go_up(&app, &tab);
            }
            gtk::gdk::Key::Delete | gtk::gdk::Key::KP_Delete => {
                if state.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                    super::commands::delete_selected(&app);
                } else {
                    super::commands::trash_selected(&app);
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    view.add_controller(key);
}

/// True when keyboard focus is in a text entry inside `widget` (inline rename).
fn focus_is_text_input(widget: &gtk::Widget) -> bool {
    let Some(focus) = widget.root().and_then(|root| root.focus()) else {
        return false;
    };
    let mut current = Some(focus);
    while let Some(w) = current {
        if w.is::<gtk::Text>() || w.is::<gtk::TextView>() || w.is::<gtk::Entry>() || w.is::<gtk::SearchEntry>() {
            return true;
        }
        current = w.parent();
    }
    false
}

/// Whether the file view should handle this key itself. Bare Space must be
/// claimed or the row toggles its selection instead of previewing.
fn claims_view_key(keyval: gdk::Key, state: gdk::ModifierType, typing: bool) -> bool {
    if typing {
        return false;
    }
    match keyval {
        gdk::Key::space | gdk::Key::KP_Space => !selection_modified(state),
        gdk::Key::BackSpace | gdk::Key::Delete | gdk::Key::KP_Delete => true,
        _ => false,
    }
}

fn selection_modified(state: gdk::ModifierType) -> bool {
    state.contains(gdk::ModifierType::CONTROL_MASK)
        || state.contains(gdk::ModifierType::SHIFT_MASK)
        || state.contains(gdk::ModifierType::ALT_MASK)
        || state.contains(gdk::ModifierType::SUPER_MASK)
}

/// Make a view draggable: dragging exports the current selection as a URI list,
/// so it can be dropped into another tab, another folder, or another app. Both
/// copy and move are offered (Ctrl forces copy, Shift forces move).
pub(super) fn attach_drag_source(tab: &Rc<super::Tab>, view: &impl IsA<gtk::Widget>) {
    let source = gtk::DragSource::new();
    source.set_actions(gdk::DragAction::COPY | gdk::DragAction::MOVE);
    let tab = tab.clone();
    source.connect_prepare(move |_, _, _| {
        let files: Vec<gio::File> = super::commands::selected(&tab).into_iter().map(|(f, _)| f).collect();
        if files.is_empty() {
            return None;
        }
        let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
        let bytes = glib::Bytes::from(format!("{}\r\n", uris.join("\r\n")).as_bytes());
        Some(gdk::ContentProvider::for_bytes(super::URI_LIST, &bytes))
    });
    view.add_controller(source);
}

/// Accept files dropped onto a view. A drop onto a folder goes *into* that folder;
/// a drop on empty space goes into the current folder. The action follows the
/// drag (move within the same filesystem by default, copy across; Ctrl/Shift
/// override), and the folder under the pointer highlights while hovering.
pub(super) fn attach_drop_target(app: &Rc<super::App>, tab: &Rc<super::Tab>, view: &impl IsA<gtk::Widget>) {
    let target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY | gdk::DragAction::MOVE);
    let highlighted: Rc<RefCell<Option<gtk::Widget>>> = Rc::new(RefCell::new(None));

    {
        let view = view.clone();
        let hl = highlighted.clone();
        target.connect_motion(move |_, x, y| {
            set_drop_highlight(&view, x, y, &hl);
            gdk::DragAction::COPY | gdk::DragAction::MOVE
        });
    }
    {
        let hl = highlighted.clone();
        target.connect_leave(move |_| clear_drop_highlight(&hl));
    }
    {
        let app = app.clone();
        let tab = tab.clone();
        let view = view.clone();
        target.connect_drop(move |target, value, x, y| {
            clear_drop_highlight(&highlighted);
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let sources: Vec<PathBuf> = list.files().iter().filter_map(|f| f.path()).collect();
            let Some(current) = tab.dir_list.file().and_then(|f| f.path()) else {
                return false;
            };
            // Into the folder under the pointer, or the current folder otherwise.
            let dest = match folder_name_at(&view, x, y) {
                Some(name) => current.join(name),
                None => current,
            };
            // Skip no-op / invalid targets: an item already in `dest`, or a folder
            // dropped into itself or one of its descendants.
            let sources: Vec<PathBuf> = sources
                .into_iter()
                .filter(|s| s.parent() != Some(dest.as_path()) && *s != dest && !dest.starts_with(s))
                .collect();
            if sources.is_empty() {
                return false;
            }
            let offered = target.current_drop().map(|d| d.actions()).unwrap_or(gdk::DragAction::COPY);
            let kind = drop_kind(offered, &sources, &dest);
            super::commands::start_job(&app, kind, sources, dest, true);
            true
        });
    }
    view.add_controller(target);
}

/// Choose move vs copy for a drop the way Nautilus does: honour an explicit
/// modifier (Ctrl → copy, Shift → move, seen as a single-action offer), else move
/// when everything is on the destination's filesystem and copy when it crosses.
fn drop_kind(offered: gdk::DragAction, sources: &[PathBuf], dest: &Path) -> ops::Kind {
    let can_copy = offered.contains(gdk::DragAction::COPY);
    let can_move = offered.contains(gdk::DragAction::MOVE);
    if can_move && !can_copy {
        return ops::Kind::Move;
    }
    if can_copy && !can_move {
        return ops::Kind::Copy;
    }
    if same_filesystem(sources, dest) { ops::Kind::Move } else { ops::Kind::Copy }
}

/// Whether every source is on the same filesystem as `dest`.
fn same_filesystem(sources: &[PathBuf], dest: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(dest_dev) = std::fs::metadata(dest).map(|m| m.dev()) else {
        return false;
    };
    sources
        .iter()
        .all(|s| std::fs::symlink_metadata(s).map(|m| m.dev() == dest_dev).unwrap_or(false))
}

/// The name of the folder cell under `(x, y)` in `view`, if any.
fn folder_name_at(view: &impl IsA<gtk::Widget>, x: f64, y: f64) -> Option<String> {
    let mut widget = view.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if let Some(name) = unsafe { widget.data::<String>("fs-dir-name") } {
            let name = unsafe { name.as_ref() };
            if !name.is_empty() {
                return Some(name.clone());
            }
        }
        widget = widget.parent()?;
    }
}

/// Highlight the folder cell under `(x, y)`, un-highlighting any previous one.
fn set_drop_highlight(view: &impl IsA<gtk::Widget>, x: f64, y: f64, hl: &Rc<RefCell<Option<gtk::Widget>>>) {
    let cell = folder_cell_at(view, x, y);
    let mut current = hl.borrow_mut();
    if current.as_ref() == cell.as_ref() {
        return;
    }
    if let Some(prev) = current.take() {
        prev.remove_css_class("fs-drop");
    }
    if let Some(cell) = &cell {
        cell.add_css_class("fs-drop");
    }
    *current = cell;
}

fn clear_drop_highlight(hl: &Rc<RefCell<Option<gtk::Widget>>>) {
    if let Some(prev) = hl.borrow_mut().take() {
        prev.remove_css_class("fs-drop");
    }
}

/// Fade names that start with a dot, matching the hidden-file filter.
fn mark_hidden(widget: &impl IsA<gtk::Widget>, info: &gio::FileInfo) {
    if is_hidden_entry(info) {
        widget.add_css_class("fs-hidden");
    } else {
        widget.remove_css_class("fs-hidden");
    }
}

fn is_hidden_entry(info: &gio::FileInfo) -> bool {
    info.is_hidden() || info.name().to_string_lossy().starts_with('.')
}

/// Keep dotfiles after normal entries. This sorter is applied before the column
/// sort, so a descending name or size does not pull hidden items back to the top.
pub(super) fn with_hidden_last(column_sorter: &gtk::Sorter) -> gtk::MultiSorter {
    let grouped = gtk::CustomSorter::new(|a, b| {
        let a = a.downcast_ref::<gio::FileInfo>().unwrap();
        let b = b.downcast_ref::<gio::FileInfo>().unwrap();
        match (is_hidden_entry(a), is_hidden_entry(b)) {
            (false, true) => gtk::Ordering::Smaller,
            (true, false) => gtk::Ordering::Larger,
            _ => gtk::Ordering::Equal,
        }
    });
    let multi = gtk::MultiSorter::new();
    multi.append(grouped);
    multi.append(column_sorter.clone());
    multi
}

/// Tag a cell with the folder name it drops into (empty for non-folders, so a
/// recycled cell never keeps a stale target).
fn mark_drop_folder(cell: &impl IsA<gtk::Widget>, info: &gio::FileInfo) {
    let name = if info.file_type() == gio::FileType::Directory {
        info.name().to_string_lossy().into_owned()
    } else {
        String::new()
    };
    unsafe { cell.set_data("fs-dir-name", name) };
}

/// The folder cell widget under `(x, y)`, walking up from the picked descendant.
fn folder_cell_at(view: &impl IsA<gtk::Widget>, x: f64, y: f64) -> Option<gtk::Widget> {
    let mut widget = view.pick(x, y, gtk::PickFlags::DEFAULT)?;
    loop {
        if let Some(name) = unsafe { widget.data::<String>("fs-dir-name") }
            && !unsafe { name.as_ref() }.is_empty()
        {
            return Some(widget);
        }
        widget = widget.parent()?;
    }
}

// --- Inline rename ----------------------------------------------------------

/// Wrap a cell's name label in a stack that can flip to an entry for renaming
/// the item in place.
fn name_stack(label: &gtk::Label) -> gtk::Stack {
    let entry = gtk::Entry::builder().xalign(0.0).width_chars(6).build();
    entry.add_css_class("fs-rename");
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.add_named(label, Some("view"));
    stack.add_named(&entry, Some("edit"));
    stack
}

/// Per-bind setup for a cell's name stack: tag the cell with its row position (so
/// a rename can find this widget) and make sure a recycled cell isn't left in
/// edit mode. Returns the label to fill in.
fn bind_name_stack(cell: &gtk::Box, position: u32) -> gtk::Label {
    unsafe { cell.set_data("fs-position", position) };
    let stack = find_name_stack(cell.upcast_ref::<gtk::Widget>()).expect("cell has a name stack");
    stack.set_visible_child_name("view");
    stack.child_by_name("view").and_downcast::<gtk::Label>().expect("name stack holds a label")
}

/// Depth-first search for the name stack inside a cell.
fn find_name_stack(widget: &gtk::Widget) -> Option<gtk::Stack> {
    let mut child = widget.first_child();
    while let Some(w) = child {
        if let Ok(stack) = w.clone().downcast::<gtk::Stack>() {
            return Some(stack);
        }
        if let Some(found) = find_name_stack(&w) {
            return Some(found);
        }
        child = w.next_sibling();
    }
    None
}

/// The cell widget currently showing row `position`, if it is realized.
fn cell_at_position(widget: &gtk::Widget, position: u32) -> Option<gtk::Widget> {
    let mut child = widget.first_child();
    while let Some(w) = child {
        if let Some(p) = unsafe { w.data::<u32>("fs-position") }
            && unsafe { *p.as_ref() } == position
        {
            return Some(w);
        }
        if let Some(found) = cell_at_position(&w, position) {
            return Some(found);
        }
        child = w.next_sibling();
    }
    None
}

/// Start renaming the selected item in place. Returns false when the row isn't
/// realized (e.g. scrolled out of view), so the caller can fall back to a dialog.
pub(super) fn start_inline_rename(app: &Rc<super::App>, tab: &Rc<super::Tab>) -> bool {
    let bitset = tab.selection.selection();
    if bitset.size() != 1 {
        return false;
    }
    let position = bitset.nth(0);
    let Some(info) = tab.selection.item(position).and_downcast::<gio::FileInfo>() else {
        return false;
    };
    let Some(dir) = tab.dir_list.file() else { return false };
    let file = dir.child(info.name());
    let name = info.display_name().to_string();
    let is_dir = info.file_type() == gio::FileType::Directory;

    // Only the visible view can host the editor.
    let view: gtk::Widget = if app.is_list.get() {
        tab.column_view.clone().upcast()
    } else {
        tab.grid_view.clone().upcast()
    };
    let Some(cell) = cell_at_position(&view, position) else {
        return false;
    };
    let Some(stack) = find_name_stack(&cell) else { return false };
    let Some(entry) = stack.child_by_name("edit").and_downcast::<gtk::Entry>() else {
        return false;
    };

    entry.set_text(&name);
    let (start, end) = rename_selection_region(&name, is_dir);
    stack.set_visible_child_name("edit");
    entry.grab_focus();
    entry.select_region(start, end);

    // Wire this edit session, disconnecting when it ends so a recycled cell never
    // accumulates handlers from previous renames.
    let handlers: Rc<RefCell<Vec<glib::SignalHandlerId>>> = Rc::new(RefCell::new(Vec::new()));
    let finish = {
        let stack = stack.clone();
        let entry = entry.clone();
        let handlers = handlers.clone();
        Rc::new(move || {
            stack.set_visible_child_name("view");
            for id in handlers.borrow_mut().drain(..) {
                entry.disconnect(id);
            }
        })
    };

    // Enter commits.
    {
        let app = app.clone();
        let finish = finish.clone();
        let file = file.clone();
        let old = name.clone();
        let id = entry.connect_activate(move |entry| {
            let new = entry.text().to_string();
            finish();
            commit_inline_rename(&app, &file, &old, &new);
        });
        handlers.borrow_mut().push(id);
    }
    // Escape cancels; so does clicking away.
    {
        let finish = finish.clone();
        let key = gtk::EventControllerKey::new();
        key.connect_key_pressed(move |_, k, _, _| {
            if k == gtk::gdk::Key::Escape {
                finish();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        entry.add_controller(key);
    }
    {
        let finish = finish.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| finish());
        entry.add_controller(focus);
    }
    true
}

/// Apply an inline rename, recording it for undo.
fn commit_inline_rename(app: &Rc<super::App>, file: &gio::File, old: &str, new: &str) {
    if new.is_empty() || new == old {
        return; // nothing to do
    }
    if new.contains('/') {
        super::toast(app, "Invalid name");
        return;
    }
    let renamed = file
        .path()
        .ok_or(())
        .and_then(|from| fileops::rename(&from, new).map(|to| (from, to)).map_err(|_| ()));
    match renamed {
        Ok((from, to)) => {
            super::commands::push_undo(app, undo::Action::Rename { from, to });
            super::navigation::refresh(&super::navigation::active_tab(app));
        }
        Err(()) => super::toast(app, "Couldn't rename"),
    }
}

// --- Views ------------------------------------------------------------------

/// Rebuild `column_view`'s columns with the given leading-icon size.
pub(super) fn fill_columns(column_view: &gtk::ColumnView, icon_size: i32) {
    while let Some(col) = column_view.columns().item(0).and_downcast::<gtk::ColumnViewColumn>() {
        column_view.remove_column(&col);
    }
    column_view.append_column(&name_column(icon_size));
    column_view.append_column(&size_column());
    column_view.append_column(&modified_column());
}

fn name_column(icon_size: i32) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let image = gtk::Image::new();
        let label =
            gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();
        row.append(&image);
        row.append(&name_stack(&label));
        item.set_child(Some(&row));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let row = item.child().and_downcast::<gtk::Box>().unwrap();
        let image = row.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = bind_name_stack(&row, item.position());
        image.set_pixel_size(icon_size);
        set_themed_icon(&image, &info);
        if icon_size >= 32 {
            set_thumbnail(&image, &info, icon_size);
        }
        label.set_label(&info.display_name());
        // The name column ellipsizes; show the full name on hover.
        row.set_tooltip_text(Some(&info.display_name()));
        mark_drop_folder(&row, &info);
        mark_hidden(&row, &info);
    });
    let col =
        gtk::ColumnViewColumn::builder().title("Name").factory(&factory).expand(true).resizable(true).build();
    col.set_sorter(Some(&info_sorter(|a, b| {
        a.display_name().to_lowercase().cmp(&b.display_name().to_lowercase())
    })));
    col
}

fn size_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let label = gtk::Label::builder().xalign(1.0).build();
        label.add_css_class("numeric");
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        if info.file_type() == gio::FileType::Directory {
            label.set_label("");
        } else {
            label.set_label(&format::human_size(info.size().max(0) as u64));
        }
        mark_hidden(&label, &info);
    });
    let col = gtk::ColumnViewColumn::builder().title("Size").factory(&factory).resizable(true).build();
    col.set_sorter(Some(&info_sorter(|a, b| a.size().cmp(&b.size()))));
    col
}

fn modified_column() -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_child(Some(&gtk::Label::builder().xalign(0.0).build()));
    });
    factory.connect_bind(|_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let label = item.child().and_downcast::<gtk::Label>().unwrap();
        match info.modification_date_time() {
            Some(dt) => label.set_label(&format::modified(&dt)),
            None => label.set_label(""),
        }
        mark_hidden(&label, &info);
    });
    let col =
        gtk::ColumnViewColumn::builder().title("Modified").factory(&factory).resizable(true).build();
    col.set_sorter(Some(&info_sorter(|a, b| {
        let key = |i: &gio::FileInfo| i.modification_date_time().map(|d| d.to_unix()).unwrap_or(0);
        key(a).cmp(&key(b))
    })));
    col
}

pub(super) fn grid_factory(size: i32) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let image = gtk::Image::builder().pixel_size(size).build();
        let label = gtk::Label::builder()
            .justify(gtk::Justification::Center)
            .wrap(true)
            // Character-level wrapping: long unbroken filenames (e.g.
            // `_OceanofPDF.com_Everyday_Ayurveda_..._Bhattacharya.pdf`, common in
            // Downloads) have no spaces to break on. With the default word wrap
            // the label's *minimum* width becomes the whole filename, which blows
            // each grid cell out to full width and collapses the grid to a single
            // column. WordChar lets it break mid-word, keeping cells narrow.
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars((size / 6).max(8))
            .width_chars((size / 8).max(6))
            .build();
        label.add_css_class("fs-name");
        let cell = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        cell.set_halign(gtk::Align::Center);
        cell.append(&image);
        cell.append(&name_stack(&label));
        item.set_child(Some(&cell));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let info = item.item().and_downcast::<gio::FileInfo>().unwrap();
        let cell = item.child().and_downcast::<gtk::Box>().unwrap();
        let image = cell.first_child().and_downcast::<gtk::Image>().unwrap();
        let label = bind_name_stack(&cell, item.position());
        image.set_pixel_size(size);
        set_themed_icon(&image, &info);
        set_thumbnail(&image, &info, size);
        label.set_label(&info.display_name());
        // Names are wrapped/ellipsized in the grid, so surface the full name on
        // hover.
        cell.set_tooltip_text(Some(&info.display_name()));
        mark_drop_folder(&cell, &info);
        mark_hidden(&cell, &info);
    });
    factory
}

fn set_themed_icon(image: &gtk::Image, info: &gio::FileInfo) {
    if let Some(icon) = info.icon() {
        image.set_from_gicon(&icon);
    } else {
        image.set_icon_name(Some("text-x-generic"));
    }
}

/// For image files, replace the themed icon with a scaled thumbnail — decoded on
/// a background thread so a folder full of images never blocks the UI.
///
/// Each request tags the reused `Image` widget with a fresh token; when the
/// decode returns we only apply it if the widget still carries that token, so a
/// recycled cell (scrolled to a different file) is never overwritten by a stale
/// thumbnail.
fn set_thumbnail(image: &gtk::Image, info: &gio::FileInfo, size: i32) {
    static THUMB_GEN: AtomicU64 = AtomicU64::new(0);

    let is_image = info.content_type().map(|c| c.starts_with("image/")).unwrap_or(false);
    if !is_image {
        return;
    }
    let Some(file) = info.attribute_object("standard::file").and_downcast::<gio::File>() else {
        return;
    };
    let Some(path) = file.path() else { return };

    let token = THUMB_GEN.fetch_add(1, AtomicOrdering::Relaxed) + 1;
    unsafe { image.set_data("thumb-token", token) };

    let image = image.clone();
    glib::spawn_future_local(async move {
        let decoded = gio::spawn_blocking(move || decode_thumbnail(&path, size)).await;
        let Ok(Some((width, height, stride, has_alpha, pixels))) = decoded else {
            return;
        };
        // Only apply if this widget is still showing the file we decoded for.
        let current = unsafe { image.data::<u64>("thumb-token") };
        if current.map(|p| unsafe { *p.as_ref() }) != Some(token) {
            return;
        }
        let bytes = glib::Bytes::from_owned(pixels);
        let format =
            if has_alpha { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
        let texture = gdk::MemoryTexture::new(width, height, format, &bytes, stride as usize);
        image.set_paintable(Some(&texture));
    });
}

/// Decode and scale an image to raw RGBA(/RGB) bytes off the main thread.
/// Returns `(width, height, stride, has_alpha, pixels)`.
fn decode_thumbnail(path: &std::path::Path, size: i32) -> Option<(i32, i32, i32, bool, Vec<u8>)> {
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(path, size, size, true).ok()?;
    let pixels = pixbuf.read_pixel_bytes().to_vec();
    Some((pixbuf.width(), pixbuf.height(), pixbuf.rowstride(), pixbuf.has_alpha(), pixels))
}

// --- Small helpers ----------------------------------------------------------

fn info_sorter<F>(cmp: F) -> gtk::CustomSorter
where
    F: Fn(&gio::FileInfo, &gio::FileInfo) -> Ordering + 'static,
{
    gtk::CustomSorter::new(move |a, b| {
        let a = a.downcast_ref::<gio::FileInfo>().unwrap();
        let b = b.downcast_ref::<gio::FileInfo>().unwrap();
        let a_dir = a.file_type() == gio::FileType::Directory;
        let b_dir = b.file_type() == gio::FileType::Directory;
        let ord = match (a_dir, b_dir) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            _ => cmp(a, b),
        };
        ord.into()
    })
}

/// The region of a name an inline rename should pre-select: just the stem for a
/// file with an extension (so typing replaces the name but keeps ".jpg"), and the
/// whole thing for folders, dotfiles, and names without an extension.
fn rename_selection_region(name: &str, is_dir: bool) -> (i32, i32) {
    if is_dir {
        return (0, -1);
    }
    let (stem, ext) = fileops::split_name(name);
    if ext.is_empty() { (0, -1) } else { (0, stem.chars().count() as i32) }
}

#[cfg(test)]
mod tests {
    use super::{claims_view_key, drop_kind, rename_selection_region};
    use crate::ops::Kind;
    use gtk::gdk::DragAction;

    #[test]
    fn bare_space_previews_and_does_not_toggle_selection() {
        let bare = gtk::gdk::ModifierType::empty();
        assert!(claims_view_key(gtk::gdk::Key::space, bare, false));
        assert!(claims_view_key(gtk::gdk::Key::KP_Space, bare, false));
        // Ctrl/Shift+Space stay with GTK, which uses them to change the selection.
        assert!(!claims_view_key(gtk::gdk::Key::space, gtk::gdk::ModifierType::CONTROL_MASK, false));
        assert!(!claims_view_key(gtk::gdk::Key::space, gtk::gdk::ModifierType::SHIFT_MASK, false));
        // Rename and search must still receive the key.
        assert!(!claims_view_key(gtk::gdk::Key::space, bare, true));
        assert!(!claims_view_key(gtk::gdk::Key::BackSpace, bare, true));
    }
    #[test]
    fn inline_rename_preselects_the_stem_not_the_extension() {
        // Typing immediately replaces "photo" but keeps ".jpg".
        assert_eq!(rename_selection_region("photo.jpg", false), (0, 5));
        assert_eq!(rename_selection_region("archive.tar.gz", false), (0, 11)); // "archive.tar"
    }

    #[test]
    fn inline_rename_selects_everything_when_there_is_no_extension_to_keep() {
        assert_eq!(rename_selection_region("README", false), (0, -1));
        assert_eq!(rename_selection_region(".bashrc", false), (0, -1)); // dotfile, not an ext
        assert_eq!(rename_selection_region("My Folder", true), (0, -1)); // folders: whole name
        assert_eq!(rename_selection_region("src.old", true), (0, -1)); // even a dotted folder
    }

    #[test]
    fn inline_rename_region_counts_characters_not_bytes() {
        // Selection offsets are in characters; a multi-byte stem must not overrun.
        assert_eq!(rename_selection_region("résumé.pdf", false), (0, 6));
    }
    #[test]
    fn drop_kind_honours_modifier_then_filesystem() {
        // A drag next to a real dir on the same filesystem.
        let tmp = std::env::temp_dir().join(format!("filescope-drop-{}", std::process::id()));
        let dest = tmp.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let src = tmp.join("f");
        std::fs::write(&src, b"x").unwrap();
        let sources = [src];

        // An explicit single action wins regardless of filesystem.
        assert_eq!(drop_kind(DragAction::MOVE, &sources, &dest), Kind::Move);
        assert_eq!(drop_kind(DragAction::COPY, &sources, &dest), Kind::Copy);
        // No modifier (both offered) + same filesystem → move, like Nautilus.
        assert_eq!(drop_kind(DragAction::COPY | DragAction::MOVE, &sources, &dest), Kind::Move);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
