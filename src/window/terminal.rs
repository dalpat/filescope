//! A shell inside the window. "Open Terminal Here" stays a separate window
//! and lives in commands.rs.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use vte::prelude::*;

mod links;

const MIN_HEIGHT: i32 = 120;

struct Session {
    terminal: vte::Terminal,
    shell_exe: Option<PathBuf>,
    spawn: gtk::gio::Cancellable,
    pid: Cell<Option<gtk::glib::Pid>>,
    closing: Cell<bool>,
    exited: Cell<bool>,
}

impl Session {
    fn signal(&self, signal: i32) {
        if let Some(pid) = self.pid.get() {
            // VTE starts the shell in its own session/process group and reaps it.
            unsafe {
                libc::kill(-pid.0, signal);
                libc::kill(pid.0, signal);
            }
        }
    }

    fn close(self: &Rc<Self>) {
        if self.closing.replace(true) { return; }
        self.spawn.cancel();
        self.signal(libc::SIGHUP);
        let session = self.clone();
        let hold = gtk::gio::Application::default().map(|app| app.hold());
        gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(500), move || {
            // child-exited clears pid before this runs, avoiding signals to a reused PID.
            session.signal(libc::SIGKILL);
            // Dropping the terminal releases its PTY; it may already be disposed.
            drop(hold);
        });
    }
}

/// One tab's bottom pane. The shell is started once and kept when the pane hides.
pub(super) struct Pane {
    revealer: gtk::Revealer,
    split: gtk::Paned,
    viewport: gtk::ScrolledWindow,
    note: gtk::Label,
    shell: Rc<RefCell<Option<vte::Terminal>>>,
    session: RefCell<Option<Rc<Session>>>,
    open: links::Open,
    closed: Cell<bool>,
}

impl Pane {
    pub(super) fn new() -> Self {
        let note = gtk::Label::builder()
            .label("No folder to open a terminal in")
            .margin_top(16)
            .margin_bottom(16)
            .build();
        note.add_css_class("dim-label");

        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_vexpand(true);
        body.append(&note);
        // VTE's current rows/columns must not become the window's minimum size.
        let viewport = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::External)
            .min_content_height(MIN_HEIGHT)
            .max_content_height(200)
            .propagate_natural_height(true)
            .vexpand(true)
            .visible(false)
            .build();
        body.append(&viewport);

        let title = gtk::Label::builder().label("Terminal").xalign(0.0).hexpand(true).build();
        let close = gtk::Button::new();
        close.set_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_tooltip_text(Some("Hide terminal"));
        let bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        bar.add_css_class("fs-terminal-bar");
        bar.append(&title);
        bar.append(&close);

        let shell: Rc<RefCell<Option<vte::Terminal>>> = Rc::new(RefCell::new(None));

        let wrap = gtk::Box::new(gtk::Orientation::Vertical, 0);
        wrap.add_css_class("fs-terminal");
        wrap.set_vexpand(true);
        wrap.append(&bar);
        wrap.append(&body);

        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::None)
            .child(&wrap)
            .reveal_child(false)
            .vexpand(true)
            .build();
        // A hidden end child also removes the divider and gives the browser all space.
        revealer.bind_property("reveal-child", &revealer, "visible").sync_create().build();
        let split = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .resize_start_child(true).resize_end_child(false)
            .shrink_start_child(false).shrink_end_child(false)
            .wide_handle(true).vexpand(true)
            .end_child(&revealer).build();

        let pane = Self {
            revealer: revealer.clone(),
            split,
            viewport,
            note,
            shell,
            session: RefCell::new(None),
            open: Rc::new(RefCell::new(None)),
            closed: Cell::new(false),
        };
        let hide = pane.revealer.downgrade();
        let shell = Rc::downgrade(&pane.shell);
        close.connect_clicked(move |_| {
            let (Some(hide), Some(shell)) = (hide.upgrade(), shell.upgrade()) else { return; };
            hide.set_reveal_child(false);
            if let Some(term) = shell.borrow().as_ref() {
                set_shell_accels(term, true);
            }
        });
        pane
    }

    pub(super) fn widget(&self) -> gtk::Paned {
        self.split.clone()
    }

    pub(super) fn shutdown(&self) {
        if self.closed.replace(true) { return; }
        let term = self.shell.borrow_mut().take();
        if let Some(term) = term {
            release_focus(&term);
            self.viewport.set_child(None::<&gtk::Widget>);
        }
        if let Some(session) = self.session.borrow_mut().take() { session.close(); }
        self.revealer.set_reveal_child(false);
    }

    /// Show or hide the pane. A shell starts only the first time `cwd` is a folder.
    pub(super) fn toggle(&self, cwd: Option<&Path>) {
        if self.closed.get() { return; }
        if self.revealer.reveals_child() {
            self.revealer.set_reveal_child(false);
            if let Some(term) = self.shell.borrow().as_ref() {
                set_shell_accels(term, true);
            }
            return;
        }
        self.revealer.set_reveal_child(true);
        if let Some(term) = self.shell.borrow().as_ref() {
            term.grab_focus();
            set_shell_accels(term, false);
            return;
        }
        let Some(dir) = cwd.and_then(|path| path.to_str()) else {
            self.note.set_label("No folder to open a terminal in");
            self.note.set_visible(true);
            self.viewport.set_visible(false);
            return;
        };
        self.note.set_visible(false);
        let term = vte::Terminal::new();
        term.set_vexpand(true);
        term.set_hexpand(true);
        paint_colors(&term);
        watch_focus(&term);
        self.viewport.set_child(Some(&term));
        self.viewport.set_visible(true);
        *self.shell.borrow_mut() = Some(term.clone());

        let shell_bin = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let session = Rc::new(Session {
            terminal: term.clone(), shell_exe: std::fs::canonicalize(&shell_bin).ok(), spawn: gtk::gio::Cancellable::new(),
            pid: Cell::new(None), closing: Cell::new(false), exited: Cell::new(false),
        });
        *self.session.borrow_mut() = Some(session.clone());
        links::install(&term, &session, &self.open);
        let viewport = self.viewport.downgrade();
        let revealer = self.revealer.downgrade();
        let shell = Rc::downgrade(&self.shell);
        let child = Rc::downgrade(&session);
        term.connect_child_exited(move |term, _status| {
            let Some(child) = child.upgrade() else { return; };
            child.pid.set(None);
            child.exited.set(true);
            if child.closing.get() { return; }
            let (Some(viewport), Some(revealer), Some(shell)) = (viewport.upgrade(), revealer.upgrade(), shell.upgrade()) else { return; };
            // Ctrl+D (or `exit`) ended the shell. Drop it so the next open starts fresh.
            release_focus(term);
            viewport.set_child(None::<&gtk::Widget>);
            viewport.set_visible(false);
            revealer.set_reveal_child(false);
            *shell.borrow_mut() = None;
        });

        let env = process_env();
        let env_refs: Vec<&str> = env.iter().map(String::as_str).collect();
        let host = self.viewport.downgrade();
        let note = self.note.downgrade();
        let shell = Rc::downgrade(&self.shell);
        let child = Rc::downgrade(&session);
        term.spawn_async(
            vte::PtyFlags::DEFAULT,
            Some(dir),
            &[&shell_bin],
            &env_refs,
            gtk::glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            Some(&session.spawn),
            move |result| {
                let Some(child) = child.upgrade() else { return; };
                match result {
                Ok(pid) => {
                    // spawn_async already installs VTE's child watch.
                    if !child.exited.get() {
                        child.pid.set(Some(pid));
                        if child.closing.get() { child.signal(libc::SIGHUP); }
                    }
                }
                Err(_) => {
                    if child.closing.get() { return; }
                    let (Some(host), Some(note), Some(shell)) = (host.upgrade(), note.upgrade(), shell.upgrade()) else { return; };
                    release_focus(&child.terminal);
                    host.set_child(None::<&gtk::Widget>);
                    host.set_visible(false);
                    note.set_label("Couldn't start the shell");
                    note.set_visible(true);
                    *shell.borrow_mut() = None;
                }
            }},
        );
        term.grab_focus();
        set_shell_accels(&term, false);
    }
}

fn inspect_path(source: PathBuf) -> std::io::Result<(PathBuf, bool)> {
    // Resolve symlink/.. parents, retaining the final entry even if its target is missing.
    let source = match (source.parent(), source.file_name()) {
        (Some(parent), Some(name)) => parent.canonicalize()?.join(name),
        _ => source.canonicalize()?,
    };
    let entry = std::fs::symlink_metadata(&source)?;
    let is_dir = entry.is_dir() || entry.file_type().is_symlink()
        && std::fs::metadata(&source).is_ok_and(|target| target.is_dir());
    Ok((source, is_dir))
}

/// Route every terminal action back to its owning tab, even if focus changes.
pub(super) fn connect(app: &Rc<super::App>, tab: &Rc<super::Tab>) {
    let app = Rc::downgrade(app);
    let tab_ref = Rc::downgrade(tab);
    *tab.terminal.open.borrow_mut() = Some(Box::new(move |result| {
        let (Some(app), Some(tab)) = (app.upgrade(), tab_ref.upgrade()) else { return; };
        if tab.terminal.closed.get() { return; }
        let generation = tab.begin_navigation();
        match result {
            Err(error) => super::toast(&app, &error),
            Ok(links::Target::Web(uri)) => {
                let weak = Rc::downgrade(&app);
                gtk::UriLauncher::new(&uri).launch(Some(&app.window), gtk::gio::Cancellable::NONE, move |result| {
                    if let (Err(error), Some(app)) = (result, weak.upgrade()) { super::toast(&app, &format!("Couldn't open link: {error}")); }
                });
            }
            Ok(links::Target::Path(path)) => {
                let app = Rc::downgrade(&app);
                let tab = Rc::downgrade(&tab);
                gtk::glib::spawn_future_local(async move {
                    let source = path.clone();
                    let result = gtk::gio::spawn_blocking(move || inspect_path(source)).await;
                    let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) else { return; };
                    if tab.terminal.closed.get() || tab.navigation_generation.get() != generation { return; }
                    match result {
                        Ok(Ok((path, true))) => super::navigation::navigate(&app, &tab, gtk::gio::File::for_path(&path)),
                        Ok(Ok((path, false))) => {
                            if let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|s| s.to_str())) {
                                if name.starts_with('.') && !app.show_hidden.get()
                                    && let Some(action) = app.window.lookup_action("toggle-hidden") {
                                    action.activate(None);
                                }
                                super::navigation::navigate(&app, &tab, gtk::gio::File::for_path(parent));
                                super::navigation::select_names(&tab, &[name.to_owned()], || {});
                            }
                        }
                        Ok(Err(error)) => super::toast(&app, &format!("Couldn't open {}: {error}", path.display())),
                        Err(_) => super::toast(&app, "Couldn't inspect the terminal path"),
                    }
                });
            }
        }
    }));
}

impl Drop for Pane {
    fn drop(&mut self) { self.shutdown(); }
}

struct Palette {
    background: String,
    foreground: String,
    cursor: Option<String>,
    colors: Vec<String>,
}

/// Ptyxis stores a palette name, and the actual colors live in its own data.
/// Use that profile when it is installed so this pane matches the system terminal.
fn paint_colors(term: &vte::Terminal) {
    if !apply_ptyxis(term) {
        let dark = gtk::Settings::default().is_some_and(|settings| settings.is_gtk_application_prefer_dark_theme());
        let (fg, bg) = if dark { ("#d4d4d4", "#1e1e1e") } else { ("#1e1e1e", "#fafafa") };
        if let Some(color) = rgba(fg) {
            term.set_color_foreground(&color);
        }
        if let Some(color) = rgba(bg) {
            term.set_color_background(&color);
        }
    }
}

fn apply_ptyxis(term: &vte::Terminal) -> bool {
    let Some(settings) = settings_if_installed("org.gnome.Ptyxis") else { return false };
    let uuid = settings.string("default-profile-uuid");
    if uuid.is_empty() || uuid.contains('/') { return false; }
    let path = format!("/org/gnome/Ptyxis/Profiles/{uuid}/");
    let Some(profile) = settings_at("org.gnome.Ptyxis.Profile", &path) else { return false };
    let Some(text) = palette_text(profile.string("palette").as_str()) else { return false };
    let Some(palette) = parse_palette(&text) else { return false };
    let Some(background) = rgba(&palette.background) else { return false };
    let Some(foreground) = rgba(&palette.foreground) else { return false };
    let colors: Vec<_> = palette.colors.iter().filter_map(|color| rgba(color)).collect();
    if colors.len() != 16 {
        return false;
    }
    let color_refs: Vec<_> = colors.iter().collect();
    term.set_colors(Some(&foreground), Some(&background), &color_refs);
    if let Some(cursor) = palette.cursor.as_deref().and_then(rgba) {
        term.set_color_cursor(Some(&cursor));
    }
    term.set_bold_is_bright(profile.boolean("bold-is-bright"));
    term.set_cursor_shape(match settings.string("cursor-shape").as_str() {
        "ibeam" => vte::CursorShape::Ibeam,
        "underline" => vte::CursorShape::Underline,
        _ => vte::CursorShape::Block,
    });
    term.set_cursor_blink_mode(match settings.string("cursor-blink-mode").as_str() {
        "on" => vte::CursorBlinkMode::On,
        "off" => vte::CursorBlinkMode::Off,
        _ => vte::CursorBlinkMode::System,
    });
    let font = if settings.boolean("use-system-font") {
        settings_if_installed("org.gnome.desktop.interface")
            .map(|desktop| desktop.string("monospace-font-name").to_string())
            .unwrap_or_default()
    } else {
        settings.string("font-name").to_string()
    };
    if !font.is_empty() {
        term.set_font(Some(&gtk::pango::FontDescription::from_string(&font)));
    }
    true
}

fn settings_if_installed(schema: &str) -> Option<gtk::gio::Settings> {
    gtk::gio::SettingsSchemaSource::default()?.lookup(schema, true)?;
    Some(gtk::gio::Settings::new(schema))
}

fn settings_at(schema: &str, path: &str) -> Option<gtk::gio::Settings> {
    gtk::gio::SettingsSchemaSource::default()?.lookup(schema, true)?;
    Some(gtk::gio::Settings::with_path(schema, path))
}

fn palette_text(name: &str) -> Option<String> {
    // Gio rejects the executable (invalid gvdb header). `gresource` reads the section inside it.
    if name.is_empty() || name.contains(['/', '\\', '\0']) {
        return None;
    }
    let path = format!("/org/gnome/Ptyxis/palettes/{name}.palette");
    let output = std::process::Command::new("gresource")
        .args(["extract", "/usr/bin/ptyxis", &path])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn parse_palette(text: &str) -> Option<Palette> {
    let mut values = std::collections::HashMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        if value.starts_with('#') {
            values.insert(key.trim().to_string(), value.to_string());
        }
    }
    let mut colors = Vec::new();
    for index in 0..16 {
        colors.push(values.get(&format!("Color{index}"))?.clone());
    }
    Some(Palette {
        background: values.get("Background")?.clone(),
        foreground: values.get("Foreground")?.clone(),
        cursor: values.get("Cursor").cloned(),
        colors,
    })
}

fn rgba(spec: &str) -> Option<gtk::gdk::RGBA> {
    gtk::gdk::RGBA::parse(spec).ok()
}

fn process_env() -> Vec<String> {
    std::env::vars().map(|(key, value)| {
        let value = if matches!(key.as_str(), "LANG" | "LC_CTYPE" | "LC_ALL") {
            terminal_locale(&value)
        } else { value };
        format!("{key}={value}")
    }).collect()
}

fn terminal_locale(value: &str) -> String {
    if value.is_empty() || matches!(value, "C" | "POSIX") || value.contains('.') {
        return value.into();
    }
    let (name, modifier) = value.split_once('@').unwrap_or((value, ""));
    let candidate = if modifier.is_empty() {
        format!("{name}.UTF-8")
    } else { format!("{name}.UTF-8@{modifier}") };
    let Ok(locale) = std::ffi::CString::new(candidate.as_str()) else { return value.into(); };
    // GNU tools can escape Unicode under a UTF-8 locale alias without an encoding suffix.
    // Validate the explicit name without changing the GUI process's locale.
    let handle = unsafe { libc::newlocale(libc::LC_CTYPE_MASK, locale.as_ptr(), std::ptr::null_mut()) };
    if handle.is_null() { return value.into(); }
    unsafe { libc::freelocale(handle); }
    candidate
}

// GTK's root can retain an unparented focus widget, including an inactive one.
fn release_focus(term: &vte::Terminal) {
    let Some(root) = term.root() else { return; };
    if root.focus().is_some_and(|focus| focus == *term.upcast_ref::<gtk::Widget>() || focus.is_ancestor(term)) {
        set_shell_accels(term, true);
        root.set_focus(None::<&gtk::Widget>);
    }
}

/// Window shortcuts run before the focused widget. While the shell is focused,
/// turn them off so Ctrl+C and Ctrl+D reach the command. Ctrl+backtick still closes the pane.
fn watch_focus(term: &vte::Terminal) {
    let focus = gtk::EventControllerFocus::new();
    let enter = term.downgrade();
    focus.connect_enter(move |_| { if let Some(term) = enter.upgrade() { set_shell_accels(&term, false); } });
    let leave = term.downgrade();
    focus.connect_leave(move |_| { if let Some(term) = leave.upgrade() { set_shell_accels(&term, true); } });
    term.add_controller(focus);
}

fn set_shell_accels(term: &vte::Terminal, enable: bool) {
    let Some(window) = term.root().and_then(|root| root.downcast::<gtk::Window>().ok()) else { return };
    let Some(app) = window.application() else { return };
    for (name, accels) in super::ACCELS {
        let keys: &[&str] = if enable || *name == "terminal-pane" { accels } else { &[] };
        app.set_accels_for_action(&format!("win.{name}"), keys);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "requires an isolated graphical session with SHELL=/bin/sh and temporary XDG_CONFIG_HOME"]
    fn terminal_window_can_shrink_after_growing() {
        use super::*;
        use super::super::{self as window, navigation};
        assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
        adw::init().unwrap();
        gtk::Settings::default().unwrap().set_gtk_enable_animations(false);
        let application = adw::Application::builder()
            .application_id("dev.filescope.TerminalResizeTest")
            .flags(gtk::gio::ApplicationFlags::NON_UNIQUE).build();
        application.register(gtk::gio::Cancellable::NONE).unwrap();
        gtk::glib::MainContext::default().block_on(async {
            window::open_files(&application, &[gtk::gio::File::for_path("/tmp")]);
            let app = window::app_state(&application).unwrap();
            let tab = navigation::active_tab(&app);
            app.window.set_default_size(1400, 900);
            tab.terminal.toggle(Some(Path::new("/tmp")));
            gtk::glib::timeout_future(std::time::Duration::from_millis(800)).await;
            assert!(tab.terminal.session.borrow().as_ref().unwrap().pid.get().is_some());
            assert!(app.window.measure(gtk::Orientation::Horizontal, -1).0 <= 800,
                "terminal's allocated columns must not raise the window minimum width");
            let term = tab.terminal.shell.borrow().clone().unwrap();
            let original_height = term.height();
            tab.terminal.split.set_position(180);
            // Drive allocation explicitly: Broadway may defer frames without a visible client.
            tab.terminal.split.allocate(tab.terminal.split.width(), tab.terminal.split.height(), -1, None);
            gtk::glib::timeout_future(std::time::Duration::from_millis(200)).await;
            assert!(term.height() > original_height + 100, "divider must grow the terminal");
            let large_height = term.height();
            tab.terminal.split.set_position(380);
            tab.terminal.split.allocate(tab.terminal.split.width(), tab.terminal.split.height(), -1, None);
            gtk::glib::timeout_future(std::time::Duration::from_millis(200)).await;
            assert!(term.height() < large_height - 100, "divider must shrink the terminal");
            let stable_height = term.height();
            for _ in 0..5 {
                gtk::glib::timeout_future(std::time::Duration::from_millis(40)).await;
                assert_eq!(term.height(), stable_height, "terminal allocation must settle");
            }
            assert!(app.window.measure(gtk::Orientation::Vertical, 800).0 <= 600,
                "enlarging the terminal must not raise the window minimum height");
            // Default size is applied when a window is shown; drag resizing is checked separately.
            app.window.set_visible(false);
            app.window.set_default_size(800, 600);
            app.window.present();
            gtk::glib::timeout_future(std::time::Duration::from_millis(500)).await;
            assert!(app.window.width() <= 800, "terminal prevents window shrinking: width {}", app.window.width());
            assert!(app.window.height() <= 600, "terminal prevents window shrinking: height {}", app.window.height());
            let session = tab.terminal.session.borrow().clone().unwrap();
            tab.terminal.toggle(None);
            gtk::glib::timeout_future(std::time::Duration::from_millis(100)).await;
            assert!(!tab.terminal.revealer.is_visible());
            tab.terminal.toggle(None);
            assert!(Rc::ptr_eq(&session, tab.terminal.session.borrow().as_ref().unwrap()));
            app.window.close();
        });
    }

    #[test]
    #[ignore = "requires an isolated graphical session with SHELL=/bin/sh"]
    fn terminal_shutdown_reaps_running_and_spawning_shells() {
        use super::*;
        assert_eq!(std::env::var("SHELL").as_deref(), Ok("/bin/sh"));
        gtk::init().unwrap();
        let context = gtk::glib::MainContext::default();
        context.with_thread_default(|| context.block_on(async {
            for (immediate, stubborn) in [(false, false), (true, false), (false, true)] {
                let pane = Pane::new();
                let window = gtk::Window::new();
                window.set_child(Some(&pane.widget()));
                window.present();
                pane.toggle(Some(std::path::Path::new("/tmp")));
                let session = pane.session.borrow().clone().unwrap();
                let weak = Rc::downgrade(&session);
                if !immediate {
                    let started = std::time::Instant::now();
                    while session.pid.get().is_none() {
                        assert!(started.elapsed().as_secs() < 5);
                        gtk::glib::timeout_future(std::time::Duration::from_millis(10)).await;
                    }
                }
                if stubborn {
                    let marker = std::env::temp_dir().join(format!("filescope-shell-ready-{}", std::process::id()));
                    let _ = std::fs::remove_file(&marker);
                    session.terminal.feed_child(format!("trap '' HUP; echo ready > {}\n", marker.display()).as_bytes());
                    let started = std::time::Instant::now();
                    while !marker.exists() {
                        assert!(started.elapsed().as_secs() < 5, "shell did not install its signal trap");
                        gtk::glib::timeout_future(std::time::Duration::from_millis(10)).await;
                    }
                    std::fs::remove_file(marker).unwrap();
                }
                let pid = session.pid.get();
                pane.shutdown();
                pane.shutdown(); // Closing twice is harmless.
                window.close();
                assert!(pane.shell.borrow().is_none());
                assert!(pane.session.borrow().is_none());
                drop(session);
                gtk::glib::timeout_future(std::time::Duration::from_millis(800)).await;
                assert!(weak.upgrade().is_none(), "terminal session was retained");
                if let Some(pid) = pid {
                    assert_eq!(unsafe { libc::kill(pid.0, 0) }, -1, "closed shell is still alive");
                    assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
                }
            }
        })).unwrap();
    }

    #[test]
    fn terminal_inspection_preserves_symlink_entries_and_directory_targets() {
        let root = std::env::temp_dir().join(format!("filescope-terminal-links-{}", std::process::id()));
        std::fs::create_dir_all(root.join("folder")).unwrap();
        std::fs::write(root.join("file"), "keep").unwrap();
        for (name, target) in [("broken", "missing"), ("directory-link", "folder"), ("file-link", "file")] {
            std::os::unix::fs::symlink(target, root.join(name)).unwrap();
        }
        for (name, is_dir) in [("broken", false), ("directory-link", true), ("file-link", false)] {
            assert_eq!(super::inspect_path(root.join(name)).unwrap(), (root.join(name), is_dir));
        }
        assert!(super::inspect_path(root.join("missing")).is_err());
        assert_eq!(std::fs::read_to_string(root.join("file")).unwrap(), "keep");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn terminal_locale_keeps_explicit_encodings_and_unavailable_locales() {
        for value in ["", "C", "POSIX", "en_IN.UTF-8", "de_DE.ISO-8859-1", "sr_RS.UTF-8@latin", "filescope_missing_locale@modifier"] {
            assert_eq!(super::terminal_locale(value), value);
        }
    }

    #[test]
    fn terminal_environment_prints_unicode_filenames() {
        use std::process::Command;
        const MARKER: &str = "FILESCOPE_LOCALE_TEST";
        if std::env::var_os(MARKER).is_none() {
            for key in ["LANG", "LC_CTYPE", "LC_ALL"] {
                let output = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "window::terminal::tests::terminal_environment_prints_unicode_filenames", "--nocapture"])
                    .env(MARKER, key).env("LANG", "en_IN")
                    .env_remove("LC_ALL").env_remove("LC_CTYPE").env(key, "en_IN")
                    .output().unwrap();
                assert!(output.status.success(), "{key}: {}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            }
            return;
        }
        // This locale alias must exist to reproduce coreutils' missing-encoding behavior.
        let locale = std::ffi::CString::new("en_IN.UTF-8").unwrap();
        let handle = unsafe { libc::newlocale(libc::LC_CTYPE_MASK, locale.as_ptr(), std::ptr::null_mut()) };
        if handle.is_null() { return; }
        unsafe { libc::freelocale(handle); }
        let root = std::env::temp_dir().join(format!("filescope-terminal-locale-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("हिन्दू.m4a"), "keep").unwrap();
        let env: Vec<_> = super::process_env().into_iter().map(|item| {
            let (key, value) = item.split_once('=').unwrap();
            (key.to_owned(), value.to_owned())
        }).collect();
        let key = std::env::var(MARKER).unwrap();
        assert_eq!(env.iter().find(|(name, _)| name == &key).map(|(_, value)| value.as_str()), Some("en_IN.UTF-8"));
        let output = Command::new("ls").arg("--quoting-style=shell-escape").arg(&root)
            .env_clear().envs(env).output().unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("हिन्दू.m4a"), "Unicode names were escaped");
    }

    #[test]
    fn elic_palette_keeps_its_brown_background() {
        let text = "\
Name=Elic
Background=#4A453E
Foreground=#F2F2F2
Cursor=#F2F2F2
Color0=#303030
Color1=#E1321A
Color2=#6AB017
Color3=#FFC005
Color4=#729FCF
Color5=#EC0048
Color6=#F2F2F2
Color7=#2AA7E7
Color8=#5D5D5D
Color9=#FF361E
Color10=#7BC91F
Color11=#FFD00A
Color12=#0071FF
Color13=#FF1D62
Color14=#4BB8FD
Color15=#A020F0
";
        let palette = super::parse_palette(text).unwrap();
        assert_eq!(palette.background, "#4A453E");
        assert_eq!(palette.foreground, "#F2F2F2");
        assert_eq!(palette.colors.len(), 16);
        assert_eq!(palette.colors[0], "#303030");
    }
}
