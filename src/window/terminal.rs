//! A shell inside the window. "Open Terminal Here" stays a separate window
//! and lives in commands.rs.

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use vte::prelude::*;

const MIN_HEIGHT: i32 = 120;
const MAX_HEIGHT: i32 = 640;

/// One tab's bottom pane. The shell is started once and kept when the pane hides.
pub(super) struct Pane {
    revealer: gtk::Revealer,
    body: gtk::Box,
    note: gtk::Label,
    shell: Rc<RefCell<Option<vte::Terminal>>>,
    height: Rc<Cell<i32>>,
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
        body.set_vexpand(false);
        body.append(&note);

        let title = gtk::Label::builder().label("Terminal").xalign(0.0).hexpand(true).build();
        let close = gtk::Button::new();
        close.set_icon_name("window-close-symbolic");
        close.add_css_class("flat");
        close.set_tooltip_text(Some("Hide terminal"));
        let bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        bar.add_css_class("fs-terminal-bar");
        bar.append(&title);
        bar.append(&close);

        let handle = gtk::Box::new(gtk::Orientation::Vertical, 0);
        handle.add_css_class("fs-terminal-handle");
        handle.set_cursor(gtk::gdk::Cursor::from_name("ns-resize", None).as_ref());

        let height = Rc::new(Cell::new(200));
        let drag_origin = Rc::new(Cell::new(200));
        let shell: Rc<RefCell<Option<vte::Terminal>>> = Rc::new(RefCell::new(None));
        let drag = gtk::GestureDrag::new();
        {
            let height = height.clone();
            let drag_origin = drag_origin.clone();
            drag.connect_drag_begin(move |_, _, _| drag_origin.set(height.get()));
        }
        {
            let height = height.clone();
            let drag_origin = drag_origin.clone();
            let body = body.clone();
            let shell = shell.clone();
            drag.connect_drag_update(move |_, _, dy| {
                let next = dragged_height(drag_origin.get(), dy);
                height.set(next);
                body.set_size_request(-1, next);
                if let Some(term) = shell.borrow().as_ref() {
                    term.set_size_request(-1, next);
                }
            });
        }
        handle.add_controller(drag);

        let wrap = gtk::Box::new(gtk::Orientation::Vertical, 0);
        wrap.add_css_class("fs-terminal");
        wrap.set_vexpand(false);
        wrap.append(&handle);
        wrap.append(&bar);
        wrap.append(&body);

        let revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .child(&wrap)
            .reveal_child(false)
            .vexpand(false)
            .build();

        let pane = Self {
            revealer: revealer.clone(),
            body,
            note,
            shell,
            height,
        };
        let hide = pane.revealer.clone();
        let shell = pane.shell.clone();
        close.connect_clicked(move |_| {
            hide.set_reveal_child(false);
            if let Some(term) = shell.borrow().as_ref() {
                set_shell_accels(term, true);
            }
        });
        pane
    }

    pub(super) fn widget(&self) -> gtk::Revealer {
        self.revealer.clone()
    }

    /// Show or hide the pane. A shell starts only the first time `cwd` is a folder.
    pub(super) fn toggle(&self, cwd: Option<&Path>) {
        if self.revealer.reveals_child() {
            self.revealer.set_reveal_child(false);
            if let Some(term) = self.shell.borrow().as_ref() {
                set_shell_accels(term, true);
            }
            return;
        }
        self.revealer.set_reveal_child(true);
        if let Some(term) = self.shell.borrow().as_ref() {
            let next = self.height.get();
            self.body.set_size_request(-1, next);
            term.set_size_request(-1, next);
            term.grab_focus();
            set_shell_accels(term, false);
            return;
        }
        let Some(dir) = cwd.and_then(|path| path.to_str()) else {
            self.note.set_label("No folder to open a terminal in");
            self.note.set_visible(true);
            self.body.set_size_request(-1, -1);
            return;
        };
        self.note.set_visible(false);
        let term = vte::Terminal::new();
        term.set_vexpand(true);
        term.set_hexpand(true);
        term.set_size_request(-1, self.height.get());
        self.body.set_size_request(-1, self.height.get());
        paint_colors(&term);
        watch_focus(&term);
        self.body.append(&term);
        *self.shell.borrow_mut() = Some(term.clone());

        let body = self.body.clone();
        let note = self.note.clone();
        let shell = self.shell.clone();
        term.connect_child_exited(move |term, _status| {
            // Ctrl+D (or `exit`) ended the shell. Drop it so the next open starts fresh.
            body.remove(term);
            body.set_size_request(-1, -1);
            note.set_label("Shell exited. Close and reopen the pane to start a new session.");
            note.set_visible(true);
            *shell.borrow_mut() = None;
            set_shell_accels(term, true);
        });

        let shell_bin = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let env = process_env();
        let env_refs: Vec<&str> = env.iter().map(String::as_str).collect();
        let host = self.body.clone();
        let note = self.note.clone();
        let shell = self.shell.clone();
        let failed = term.clone();
        term.spawn_async(
            vte::PtyFlags::DEFAULT,
            Some(dir),
            &[&shell_bin],
            &env_refs,
            gtk::glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            None::<&gtk::gio::Cancellable>,
            move |result| match result {
                Ok(pid) => failed.watch_child(pid),
                Err(_) => {
                    host.remove(&failed);
                    note.set_label("Couldn't start the shell");
                    note.set_visible(true);
                    *shell.borrow_mut() = None;
                    set_shell_accels(&failed, true);
                }
            },
        );
        term.grab_focus();
        set_shell_accels(&term, false);
    }
}

/// Dragging the handle up grows the pane. `dy` is negative when the pointer moves up.
fn dragged_height(start: i32, dy: f64) -> i32 {
    ((start as f64 - dy).round() as i32).clamp(MIN_HEIGHT, MAX_HEIGHT)
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
    std::env::vars().map(|(key, value)| format!("{key}={value}")).collect()
}

/// Window shortcuts run before the focused widget. While the shell is focused,
/// turn them off so Ctrl+C and Ctrl+D reach the command. Ctrl+backtick still closes the pane.
fn watch_focus(term: &vte::Terminal) {
    let focus = gtk::EventControllerFocus::new();
    let enter = term.clone();
    focus.connect_enter(move |_| set_shell_accels(&enter, false));
    let leave = term.clone();
    focus.connect_leave(move |_| set_shell_accels(&leave, true));
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
    use super::dragged_height;

    #[test]
    fn dragging_up_grows_the_pane_and_down_shrinks_it() {
        assert_eq!(dragged_height(200, -40.0), 240);
        assert_eq!(dragged_height(200, 50.0), 150);
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
