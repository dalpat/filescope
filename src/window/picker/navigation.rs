//! Picker-local history and places never use ordinary window tabs or actions.
use super::*;

pub(super) struct State {
    pub app_id: String,
    pub back: gtk::Button,
    pub forward: gtk::Button,
    pub places: gtk::MenuButton,
    back_paths: RefCell<Vec<PathBuf>>,
    forward_paths: RefCell<Vec<PathBuf>>,
}

impl State {
    pub fn new(app_id: &str) -> Self {
        let back = crate::icon::flat_button("go-previous-symbolic", "Back (Alt+Left)");
        let forward = crate::icon::flat_button("go-next-symbolic", "Forward (Alt+Right)");
        back.set_sensitive(false);
        forward.set_sensitive(false);
        Self {
            app_id: app_id.to_owned(),
            back,
            forward,
            places: gtk::MenuButton::builder().label("Places").build(),
            back_paths: RefCell::new(Vec::new()),
            forward_paths: RefCell::new(Vec::new()),
        }
    }

    pub fn initial_folder(&self, options: &Options) -> Vec<PathBuf> {
        options.requested_folders.clone()
    }

    fn update(&self) {
        self.back
            .set_sensitive(!self.back_paths.borrow().is_empty());
        self.forward
            .set_sensitive(!self.forward_paths.borrow().is_empty());
    }

    pub fn visit(&self, previous: Option<PathBuf>) {
        if let Some(previous) = previous {
            self.back_paths.borrow_mut().push(previous);
        }
        self.forward_paths.borrow_mut().clear();
        self.update();
    }
}

fn history(picker: &Rc<Picker>, back: bool) {
    let state = &picker.navigation;
    let from = if back {
        &state.back_paths
    } else {
        &state.forward_paths
    };
    let target = from.borrow().last().cloned();
    if let (Some(target), Some(current)) = (target, picker.folder()) {
        location::open(
            picker,
            vec![target],
            location::Intent::History { back, current },
        );
    }
}

impl State {
    pub fn complete_history(&self, back: bool, current: PathBuf) {
        let (from, to) = if back {
            (&self.back_paths, &self.forward_paths)
        } else {
            (&self.forward_paths, &self.back_paths)
        };
        from.borrow_mut().pop();
        to.borrow_mut().push(current);
        self.update();
    }
}

pub(super) fn section(
    picker: &Rc<Picker>,
    container: &gtk::Box,
    title: &str,
    paths: Vec<PathBuf>,
    popover: &gtk::Popover,
) {
    let title = gtk::Label::builder().label(title).xalign(0.0).build();
    title.add_css_class("heading");
    container.append(&title);
    if paths.is_empty() {
        let empty = gtk::Label::builder()
            .label("No saved folders")
            .xalign(0.0)
            .build();
        empty.add_css_class("dim-label");
        container.append(&empty);
    }
    for path in paths {
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy();
        let button = gtk::Button::with_label(&name);
        button.set_halign(gtk::Align::Fill);
        button.set_tooltip_text(Some(&path.to_string_lossy()));
        let weak = Rc::downgrade(picker);
        let popover = popover.downgrade();
        button.connect_clicked(move |_| {
            if let Some(picker) = weak.upgrade() {
                picker.navigate(&path);
            }
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
        });
        container.append(&button);
    }
}

pub(super) fn editing(picker: &Picker) -> bool {
    picker
        .window
        .upgrade()
        .and_then(|window| gtk::prelude::RootExt::focus(&window))
        .is_some_and(|focus| {
            focus.is::<gtk::Text>() || focus.is::<gtk::TextView>() || focus.is::<gtk::Editable>()
        })
}

pub(super) fn key(
    picker: &Rc<Picker>,
    key: gtk::gdk::Key,
    modifiers: gtk::gdk::ModifierType,
) -> glib::Propagation {
    let editing = editing(picker);
    match key {
        gtk::gdk::Key::f if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
            picker.search.entry.grab_focus();
        }
        gtk::gdk::Key::F4 => {
            picker
                .preview_button
                .set_active(!picker.preview_button.is_active());
        }
        gtk::gdk::Key::Escape if picker.search.active.get() => {
            search::stop(picker, true);
            picker.list.grab_focus();
        }
        gtk::gdk::Key::l if modifiers.contains(gtk::gdk::ModifierType::CONTROL_MASK) => {
            picker.path.grab_focus();
            picker.path.select_region(0, -1);
        }
        gtk::gdk::Key::Escape if editing => {
            if let Some(folder) = picker.folder() {
                picker.path.set_text(&folder.to_string_lossy());
            }
            picker.list.grab_focus();
        }
        gtk::gdk::Key::Escape => picker.finish(1, empty_results()),
        gtk::gdk::Key::Left if !editing && modifiers.contains(gtk::gdk::ModifierType::ALT_MASK) => {
            history(picker, true)
        }
        gtk::gdk::Key::Right
            if !editing && modifiers.contains(gtk::gdk::ModifierType::ALT_MASK) =>
        {
            history(picker, false)
        }
        gtk::gdk::Key::Up if modifiers.contains(gtk::gdk::ModifierType::ALT_MASK) => {
            if let Some(parent) = picker.folder().as_deref().and_then(Path::parent) {
                picker.navigate(parent);
            }
        }
        gtk::gdk::Key::BackSpace if !editing => {
            if let Some(parent) = picker.folder().as_deref().and_then(Path::parent) {
                picker.navigate(parent);
            }
        }
        _ => return glib::Propagation::Proceed,
    }
    glib::Propagation::Stop
}

pub(super) fn connect(picker: &Rc<Picker>) {
    let weak = Rc::downgrade(picker);
    picker.navigation.back.connect_clicked(move |_| {
        if let Some(picker) = weak.upgrade() {
            history(&picker, true);
        }
    });
    let weak = Rc::downgrade(picker);
    picker.navigation.forward.connect_clicked(move |_| {
        if let Some(picker) = weak.upgrade() {
            history(&picker, false);
        }
    });
    let popover = gtk::Popover::new();
    let places = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    places.append(&picker.location.favorites);
    places.append(&picker.location.recents);
    popover.set_child(Some(&places));
    picker.navigation.places.set_popover(Some(&popover));
    let weak = Rc::downgrade(picker);
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, pressed, _, modifiers| {
        weak.upgrade()
            .map(|picker| key(&picker, pressed, modifiers))
            .unwrap_or(glib::Propagation::Proceed)
    });
    if let Some(window) = picker.window.upgrade() {
        window.add_controller(keys);
    }
}
