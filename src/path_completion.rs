//! Shared asynchronous folder completion for location and picker entries.
use gtk::{gio, glib, prelude::*};
use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;

const MAX_MATCHES: usize = 64;
const MAX_SCANNED: usize = 8192;

pub fn resolve(input: &str, base: &gio::File) -> Option<gio::File> {
    if input.is_empty() || input.contains('\0') {
        return None;
    }
    if let Some(scheme) = glib::uri_parse_scheme(input)
        && !scheme.eq_ignore_ascii_case("file") && !scheme.eq_ignore_ascii_case("trash")
    {
        return None;
    }
    if input == "~" {
        return Some(gio::File::for_path(glib::home_dir()));
    }
    if let Some(rest) = input.strip_prefix("~/") {
        return Some(gio::File::for_path(glib::home_dir().join(rest)));
    }
    if input.starts_with('/') || glib::uri_parse_scheme(input).is_some() {
        Some(gio::File::for_commandline_arg(input))
    } else {
        Some(base.resolve_relative_path(input))
    }
}

fn query(input: &str) -> Option<(gio::File, OsString)> {
    // Relative submission is owned by the caller; completion needs an explicit root.
    if !(input.starts_with('/')
        || input.starts_with("~/")
        || input == "~"
        || glib::uri_parse_scheme(input).is_some())
    {
        return None;
    }
    let file = resolve(input, &gio::File::for_path(glib::home_dir()))?;
    file.path()?;
    if input.ends_with('/') || input == "~" {
        Some((file, OsString::new()))
    } else {
        Some((file.parent()?, file.basename()?.into_os_string()))
    }
}

fn value(file: &gio::File) -> String {
    let mut text = file
        .path()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| file.uri().into());
    if !text.ends_with('/') {
        text.push('/');
    }
    text
}

struct Completion {
    entry: glib::WeakRef<gtk::Entry>,
    generation: Cell<u64>,
    task: RefCell<Option<glib::JoinHandle<()>>>,
    popup: RefCell<Option<gtk::Popover>>,
    list: glib::WeakRef<gtk::ListBox>,
    values: RefCell<Vec<String>>,
    accepting: Cell<bool>,
    focused: Cell<bool>,
}

impl Completion {
    fn hide(&self) {
        let popup = self.popup.borrow_mut().take();
        self.list.set(None::<&gtk::ListBox>);
        if let Some(popup) = popup {
            popup.popdown();
            if popup.parent().is_some() {
                popup.unparent();
            }
        }
    }

    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(task) = self.task.borrow_mut().take() {
            task.abort();
        }
        self.hide();
        self.values.borrow_mut().clear();
    }

    fn request(self: &Rc<Self>) {
        if self.accepting.get() {
            return;
        }
        self.cancel();
        if !self.focused.get() {
            return;
        }
        let Some(entry) = self.entry.upgrade() else {
            return;
        };
        let Some((folder, prefix)) = query(entry.text().as_str()) else {
            return;
        };
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        let task = glib::spawn_future_local(async move {
            let Ok(enumerator) = folder
                .enumerate_children_future(
                    "standard::name,standard::type",
                    gio::FileQueryInfoFlags::NONE,
                    glib::Priority::DEFAULT,
                )
                .await
            else {
                return;
            };
            let mut matches = Vec::new();
            let mut scanned = 0;
            loop {
                if !weak.upgrade().is_some_and(|state| {
                    state.generation.get() == generation && state.entry.upgrade().is_some()
                }) {
                    break;
                }
                let Ok(items) = enumerator
                    .next_files_future(128, glib::Priority::DEFAULT)
                    .await
                else {
                    break;
                };
                if items.is_empty() {
                    break;
                }
                scanned += items.len();
                for info in items {
                    if info.file_type() == gio::FileType::Directory
                        && info
                            .name()
                            .as_os_str()
                            .as_bytes()
                            .starts_with(prefix.as_bytes())
                    {
                        matches.push(value(&folder.child(info.name())));
                        if matches.len() == MAX_MATCHES {
                            break;
                        }
                    }
                }
                if matches.len() == MAX_MATCHES || scanned >= MAX_SCANNED {
                    break;
                }
            }
            let _ = enumerator.close_future(glib::Priority::DEFAULT).await;
            if let Some(state) = weak.upgrade() {
                if state.generation.get() == generation {
                    matches.sort();
                    matches.dedup();
                    state.values.replace(matches);
                    state.present(scanned >= MAX_SCANNED);
                }
            }
        });
        self.task.replace(Some(task));
    }

    fn present(self: &Rc<Self>, limited: bool) {
        self.hide();
        if !self.focused.get() {
            return;
        }
        let Some(entry) = self.entry.upgrade().filter(|entry| entry.is_mapped()) else {
            return;
        };
        if self.values.borrow().is_empty() {
            return;
        }
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .activate_on_single_click(true)
            .focusable(false)
            .build();
        for text in self.values.borrow().iter() {
            let label = gtk::Label::builder()
                .label(text)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(70)
                .margin_start(10)
                .margin_end(10)
                .margin_top(6)
                .margin_bottom(6)
                .build();
            label.set_tooltip_text(Some(text));
            list.append(&label);
            if let Some(row) = list.last_child() {
                row.set_focusable(false);
            }
        }
        list.unselect_all();
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(
            &gtk::ScrolledWindow::builder()
                .child(&list)
                .hscrollbar_policy(gtk::PolicyType::Never)
                .max_content_height(240)
                .propagate_natural_height(true)
                .build(),
        );
        if limited || self.values.borrow().len() == MAX_MATCHES {
            content.append(
                &gtk::Label::builder()
                    .label("Keep typing to narrow the folders")
                    .margin_top(4)
                    .margin_bottom(4)
                    .build(),
            );
        }
        let popup = gtk::Popover::builder()
            .child(&content)
            .has_arrow(false)
            .autohide(false)
            .focusable(false)
            .build();
        popup.set_size_request(entry.width().max(240), -1);
        popup.set_parent(&entry);
        let weak = Rc::downgrade(self);
        list.connect_row_activated(move |_, row| {
            if let Some(state) = weak.upgrade() {
                state.choose(row.index() as usize);
            }
        });
        popup.connect_closed(|popup| {
            if popup.parent().is_some() {
                popup.unparent();
            }
        });
        self.list.set(Some(&list));
        self.popup.replace(Some(popup.clone()));
        popup.popup();
    }

    fn choose(&self, index: usize) -> bool {
        let Some(text) = self.values.borrow().get(index).cloned() else {
            return false;
        };
        let Some(entry) = self.entry.upgrade() else {
            return false;
        };
        self.cancel();
        self.accepting.set(true);
        entry.set_text(&text);
        entry.set_position(-1);
        entry.grab_focus();
        self.accepting.set(false);
        true
    }

    fn key(&self, key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType) -> glib::Propagation {
        use gtk::gdk::{Key, ModifierType as M};
        if modifiers.intersects(M::CONTROL_MASK | M::ALT_MASK | M::SUPER_MASK) {
            return glib::Propagation::Proceed;
        }
        let Some(list) = self.list.upgrade() else {
            return glib::Propagation::Proceed;
        };
        match key {
            Key::Escape => self.cancel(),
            Key::Tab if !modifiers.contains(M::SHIFT_MASK) => {
                let index = list.selected_row().map_or(0, |row| row.index() as usize);
                self.choose(index);
            }
            Key::Down | Key::Up => {
                let length = self.values.borrow().len() as i32;
                let current = list.selected_row().map(|row| row.index());
                let next = if key == Key::Down {
                    current.map_or(0, |index| (index + 1).min(length - 1))
                } else {
                    current.map_or(length - 1, |index| (index - 1).max(0))
                };
                list.select_row(list.row_at_index(next).as_ref());
            }
            Key::Return | Key::KP_Enter => {
                let Some(row) = list.selected_row() else {
                    return glib::Propagation::Proceed;
                };
                if self.choose(row.index() as usize) {
                    if let Some(entry) = self.entry.upgrade() {
                        entry.emit_activate();
                    }
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }
}

impl Drop for Completion {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
        }
    }
}

pub fn attach(entry: &gtk::Entry) {
    let state = Rc::new(Completion {
        entry: entry.downgrade(),
        generation: Cell::new(0),
        task: RefCell::new(None),
        popup: RefCell::new(None),
        list: glib::WeakRef::new(),
        values: RefCell::new(Vec::new()),
        accepting: Cell::new(false),
        focused: Cell::new(false),
    });
    let changed = state.clone();
    entry.connect_changed(move |_| changed.request());
    let state_for_close = state.clone();
    entry.connect_unrealize(move |_| state_for_close.cancel());
    let focus = gtk::EventControllerFocus::new();
    let entered = state.clone();
    focus.connect_enter(move |_| {
        entered.focused.set(true);
        entered.request();
    });
    let state_for_focus = state.clone();
    focus.connect_leave(move |_| {
        state_for_focus.focused.set(false);
        state_for_focus.cancel();
    });
    entry.add_controller(focus);
    let keys = gtk::EventControllerKey::new();
    keys.set_name(Some("filescope-path-completion"));
    let keyed = state.clone();
    keys.connect_key_pressed(move |_, key, _, modifiers| keyed.key(key, modifiers));
    entry.add_controller(keys);
    state.request();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn path_inputs_preserve_uri_and_home_identity() {
        let base = gio::File::for_path("/tmp");
        assert!(resolve("", &base).is_none());
        assert!(resolve("bad\0name", &base).is_none());
        assert_eq!(
            resolve("child with space", &base).unwrap().path().unwrap(),
            std::path::PathBuf::from("/tmp/child with space")
        );
        assert_eq!(
            resolve("~/Documents", &base).unwrap().path().unwrap(),
            glib::home_dir().join("Documents")
        );
        assert!(
            resolve("file:///tmp/a%20b", &base)
                .unwrap()
                .equal(&gio::File::for_path("/tmp/a b"))
        );
        assert!(query("relative").is_none());
        assert_eq!(query("/tmp/alpha").unwrap().1, OsString::from("alpha"));
        assert!(resolve("sftp://example.test/share", &base).is_none());
        assert!(resolve("https://example.test/share", &base).is_none());
        assert!(resolve("trash:///", &base).is_some());
        assert!(resolve("TRASH:///", &base).is_some());
        assert!(resolve("FILE:///tmp/a%20b", &base).unwrap().equal(&gio::File::for_path("/tmp/a b")));
        assert!(query("trash:///").is_none());

    }

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut items = vec![widget.clone()];
        let mut child = widget.first_child();
        while let Some(widget) = child {
            items.extend(descendants(&widget));
            child = widget.next_sibling();
        }
        items
    }

    async fn wait_for(condition: impl Fn() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(3),
                "folder completion did not arrive"
            );
            glib::timeout_future(Duration::from_millis(20)).await;
        }
    }

    #[test]
    #[ignore = "requires an isolated GTK session"]
    fn path_completion_preserves_exact_folder_targets() {
        gtk::init().unwrap();
        let root = std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join("completion");
        std::fs::create_dir_all(root.join("alpha space")).unwrap();
        std::fs::create_dir_all(root.join("alpha-é")).unwrap();
        std::fs::write(root.join("alpha-file"), "not a directory").unwrap();
        let entry = gtk::Entry::new();
        attach(&entry);
        let window = gtk::Window::builder()
            .child(&entry)
            .default_width(600)
            .build();
        window.present();
        entry.grab_focus();
        glib::MainContext::default().block_on(async {
            entry.set_text(&format!("{}/al", root.display()));
            wait_for(|| {
                descendants(entry.upcast_ref())
                    .iter()
                    .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                    .filter(|label| label.text().contains("alpha"))
                    .count()
                    == 2
            })
            .await;
            let list = descendants(entry.upcast_ref())
                .into_iter()
                .find_map(|widget| widget.downcast::<gtk::ListBox>().ok())
                .unwrap();
            list.emit_by_name::<()>("row-activated", &[&list.row_at_index(0).unwrap()]);
            assert_eq!(entry.text(), format!("{}/alpha space/", root.display()));
            assert!(query(entry.text().as_str()).unwrap().1.is_empty());
            window.close();
        });
    }

    #[test]
    #[ignore = "requires an isolated GTK session"]
    fn path_completion_rejects_obsolete_results() {
        gtk::init().unwrap();
        let root = std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap())
            .join("completion-obsolete");
        std::fs::create_dir_all(root.join("older")).unwrap();
        std::fs::create_dir_all(root.join("newer")).unwrap();
        let entry = gtk::Entry::new();
        attach(&entry);
        let weak = entry.downgrade();
        let window = gtk::Window::builder().child(&entry).build();
        window.present();
        entry.grab_focus();
        glib::MainContext::default().block_on(async {
            entry.set_text(&format!("{}/old", root.display()));
            entry.set_text(&format!("{}/new", root.display()));
            wait_for(|| {
                descendants(entry.upcast_ref()).iter().any(|widget| {
                    widget
                        .downcast_ref::<gtk::Label>()
                        .is_some_and(|label| label.text().contains("/newer/"))
                })
            })
            .await;
            assert!(!descendants(entry.upcast_ref()).iter().any(|widget| {
                widget
                    .downcast_ref::<gtk::Label>()
                    .is_some_and(|label| label.text().contains("/older/"))
            }));
            entry.set_text(&format!("{}/o", root.display()));
            window.close();
        });
        drop(entry);
        drop(window);
        glib::MainContext::default().block_on(async {
            wait_for(|| weak.upgrade().is_none()).await;
        });
    }

    #[test]
    #[ignore = "requires an isolated GTK session"]
    fn path_completion_keyboard_preserves_editing() {
        use glib::translate::IntoGlib;
        gtk::init().unwrap();
        let root =
            std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join("completion-keys");
        std::fs::create_dir_all(root.join("alpha child")).unwrap();
        let entry = gtk::Entry::new();
        attach(&entry);
        let other = gtk::Entry::new();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&entry);
        body.append(&other);
        let window = gtk::Window::builder()
            .child(&body)
            .default_width(600)
            .build();
        window.present();
        other.grab_focus();
        let activated = Rc::new(Cell::new(0));
        let count = activated.clone();
        entry.connect_activate(move |_| count.set(count.get() + 1));
        glib::MainContext::default().block_on(async {
            entry.set_text(&format!("{}/al", root.display()));
            glib::timeout_future(Duration::from_millis(80)).await;
            assert!(
                !descendants(entry.upcast_ref())
                    .iter()
                    .any(|widget| widget.is::<gtk::Popover>()),
                "unfocused programmatic navigation showed completion"
            );
            entry.grab_focus();
            wait_for(|| {
                descendants(entry.upcast_ref())
                    .iter()
                    .any(|widget| widget.is::<gtk::ListBox>())
            })
            .await;
            let controllers = entry.observe_controllers();
            let controller = (0..controllers.n_items())
                .find_map(|index| {
                    controllers
                        .item(index)
                        .and_downcast::<gtk::EventControllerKey>()
                        .filter(|controller| {
                            controller.name().as_deref() == Some("filescope-path-completion")
                        })
                })
                .unwrap();
            let press = |key: gtk::gdk::Key, modifiers: gtk::gdk::ModifierType| {
                let key: u32 = key.into_glib();
                controller.emit_by_name::<bool>("key-pressed", &[&key, &0u32, &modifiers])
            };
            let none = gtk::gdk::ModifierType::empty();
            assert!(!press(gtk::gdk::Key::space, none));
            assert!(!press(
                gtk::gdk::Key::l,
                gtk::gdk::ModifierType::CONTROL_MASK
            ));
            assert!(press(gtk::gdk::Key::Tab, none));
            assert_eq!(entry.text(), format!("{}/alpha child/", root.display()));
            assert_eq!(activated.get(), 0, "completion must not submit a location");
            entry.set_text(&format!("{}/al", root.display()));
            wait_for(|| {
                descendants(entry.upcast_ref())
                    .iter()
                    .any(|widget| widget.is::<gtk::ListBox>())
            })
            .await;
            assert!(press(gtk::gdk::Key::Down, none));
            assert!(press(gtk::gdk::Key::Return, none));
            assert_eq!(activated.get(), 1);
            window.close();
        });
    }

    #[test]
    #[ignore = "requires an isolated GTK session"]
    fn path_completion_preserves_raw_byte_targets() {
        use std::os::unix::ffi::OsStringExt;
        gtk::init().unwrap();
        let root =
            std::path::PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join("completion-raw");
        let first = root.join(OsString::from_vec(b"raw-\xff".to_vec()));
        let second = root.join(OsString::from_vec(b"raw-\xfe".to_vec()));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let entry = gtk::Entry::new();
        attach(&entry);
        let window = gtk::Window::builder()
            .child(&entry)
            .default_width(600)
            .build();
        window.present();
        entry.grab_focus();
        glib::MainContext::default().block_on(async {
            entry.set_text(&format!("{}/raw-", root.display()));
            wait_for(|| {
                descendants(entry.upcast_ref())
                    .iter()
                    .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                    .filter(|label| label.text().starts_with("file://"))
                    .count()
                    == 2
            })
            .await;
            let targets: Vec<_> = descendants(entry.upcast_ref())
                .iter()
                .filter_map(|widget| widget.downcast_ref::<gtk::Label>())
                .map(|label| label.text().to_string())
                .filter(|text| text.starts_with("file://"))
                .collect();
            assert_ne!(targets[0], targets[1]);
            assert!(
                targets
                    .iter()
                    .any(|uri| gio::File::for_uri(uri).equal(&gio::File::for_path(&first)))
            );
            assert!(
                targets
                    .iter()
                    .any(|uri| gio::File::for_uri(uri).equal(&gio::File::for_path(&second)))
            );
            window.close();
        });
    }
}
