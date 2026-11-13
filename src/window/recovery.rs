//! Saved tab locations contain data, never live views, shells, or operation history.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::{App, Place, Tab, navigation};

const CLOSED_LIMIT: usize = 20;

pub(super) struct State {
    pub restore_session: Cell<bool>,
    closed: RefCell<VecDeque<SavedTab>>,
}

impl State {
    pub fn new(restore_session: bool) -> Self {
        Self {
            restore_session: Cell::new(restore_session),
            closed: RefCell::new(VecDeque::new()),
        }
    }

    fn remember(&self, tab: SavedTab) {
        let mut closed = self.closed.borrow_mut();
        if closed.len() == CLOSED_LIMIT {
            closed.pop_front();
        }
        closed.push_back(tab);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SavedTab {
    place: &'static str,
    uri: String,
    query: String,
    preview: bool,
    preview_width: i32,
}

impl SavedTab {
    fn capture(tab: &Tab) -> Option<Self> {
        let (place, uri, query) = match navigation::here(tab)? {
            Place::Folder(file) => ("folder", saved_uri(&file), String::new()),
            Place::Search { root, query } => ("search", saved_uri(&root), query),
            Place::Computer => ("computer", String::new(), String::new()),
            Place::Trash => ("trash", String::new(), String::new()),
        };
        Some(Self {
            place,
            uri,
            query,
            preview: tab.preview_pane.widget.is_visible(),
            preview_width: tab.preview_pane.width(),
        })
    }

    fn parse(value: &serde_json::Value) -> Option<Self> {
        let place = match value.get("place")?.as_str()? {
            "folder" => "folder",
            "search" => "search",
            "computer" => "computer",
            "trash" => "trash",
            _ => return None,
        };
        let uri = value
            .get("uri")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if matches!(place, "folder" | "search")
            && (uri.contains('\0') || glib::Uri::parse(&uri, glib::UriFlags::NONE).is_err())
        {
            return None;
        }
        Some(Self {
            place,
            uri,
            query: value
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            preview: value
                .get("preview")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            preview_width: value
                .get("preview_width")
                .and_then(|v| v.as_i64())
                .and_then(|n| i32::try_from(n).ok())
                .filter(|n| *n >= 180)
                .unwrap_or(300),
        })
    }

    fn value(&self) -> serde_json::Value {
        serde_json::json!({ "place": self.place, "uri": self.uri, "query": self.query, "preview": self.preview, "preview_width": self.preview_width })
    }

    fn restore(&self, app: &Rc<App>, select: bool) -> Rc<Tab> {
        let file = if matches!(self.place, "folder" | "search") {
            gio::File::for_uri(&self.uri)
        } else {
            gio::File::for_path(glib::home_dir())
        };
        navigation::new_tab(app, file.clone(), false);
        let tab = app.tabs.borrow().last().unwrap().clone();
        tab.preview_pane.set_width(self.preview_width);
        tab.preview_pane.set_visible(self.preview);
        let special = match self.place {
            "computer" => Some(Place::Computer),
            "trash" => Some(Place::Trash),
            "search" => Some(Place::Search {
                root: file,
                query: self.query.clone(),
            }),
            _ => None,
        };
        if let Some(place) = special {
            navigation::open_place(app, &tab, place);
        }
        if select {
            app.tab_view
                .set_selected_page(tab.page.borrow().as_ref().unwrap());
        }
        tab
    }
}

fn saved_uri(file: &gio::File) -> String { file.uri().to_string() }
fn sync_action(app: &App) {
    if let Some(action) = app
        .window
        .lookup_action("reopen-tab")
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(!app.recovery.closed.borrow().is_empty());
    }
}

pub(super) fn setup(app: &Rc<App>) {
    let reopen = gio::SimpleAction::new("reopen-tab", None);
    reopen.set_enabled(false);
    let weak = Rc::downgrade(app);
    reopen.connect_activate(move |_, _| {
        if let Some(app) = weak.upgrade() {
            reopen_closed(&app);
        }
    });
    app.window.add_action(&reopen);
    let restore = gio::SimpleAction::new_stateful(
        "restore-session",
        None,
        &app.recovery.restore_session.get().to_variant(),
    );
    let weak = Rc::downgrade(app);
    restore.connect_change_state(move |action, value| {
        let (Some(app), Some(enabled)) = (weak.upgrade(), value.and_then(|v| v.get::<bool>()))
        else {
            return;
        };
        app.recovery.restore_session.set(enabled);
        action.set_state(&enabled.to_variant());
        super::save_settings(&app);
    });
    app.window.add_action(&restore);
}

pub(super) fn remember(app: &App, tab: &Tab) {
    if let Some(saved) = SavedTab::capture(tab) {
        app.recovery.remember(saved);
    }
    sync_action(app);
}

fn reopen_closed(app: &Rc<App>) {
    let saved = app.recovery.closed.borrow_mut().pop_back();
    if let Some(saved) = saved {
        saved.restore(app, true);
    }
    sync_action(app);
}

/// Save pages in their displayed order, including locations which are offline.
pub(super) fn session(app: &App) -> String {
    if !app.recovery.restore_session.get() {
        return String::new();
    }
    let mut tabs = Vec::new();
    let mut active = 0;
    let selected = app.tab_view.selected_page();
    for index in 0..app.tab_view.n_pages() {
        let page = app.tab_view.nth_page(index);
        let Some(tab) = app
            .tabs
            .borrow()
            .iter()
            .find(|tab| tab.page.borrow().as_ref() == Some(&page))
            .cloned()
        else {
            continue;
        };
        if let Some(saved) = SavedTab::capture(&tab) {
            if selected.as_ref() == Some(&page) {
                active = tabs.len();
            }
            tabs.push(saved.value());
        }
    }
    serde_json::json!({ "tabs": tabs, "active": active }).to_string()
}

pub(super) fn restore_session(app: &Rc<App>) -> bool {
    if !app.recovery.restore_session.get() {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&crate::settings::load().session)
    else {
        return false;
    };
    let Some(records) = value.get("tabs").and_then(|v| v.as_array()) else {
        return false;
    };
    let saved: Vec<_> = records.iter().filter_map(SavedTab::parse).collect();
    if saved.is_empty() {
        return false;
    }
    let active = value
        .get("active")
        .and_then(|v| v.as_u64())
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0)
        .min(saved.len() - 1);
    let mut pages = Vec::new();
    for tab in saved {
        pages.push(tab.restore(app, false));
    }
    app.tab_view
        .set_selected_page(pages[active].page.borrow().as_ref().unwrap());
    true
}

pub(super) fn show_unavailable(app: &Rc<App>, tab: &Rc<Tab>) {
    let Some(error) = tab.dir_list.error() else {
        return;
    };
    let page = tab
        .view_stack
        .child_by_name("unavailable")
        .and_downcast::<adw::StatusPage>()
        .unwrap_or_else(|| {
            let page = adw::StatusPage::builder()
                .title("Location Unavailable")
                .build();
            crate::icon::show_status(&page, "folder-missing-symbolic", 128);
            let buttons = gtk::Box::builder()
                .spacing(12)
                .halign(gtk::Align::Center)
                .build();
            for (label, retry) in [("Retry", true), ("Go Home", false)] {
                let button = gtk::Button::with_label(label);
                let app = Rc::downgrade(app);
                let tab = Rc::downgrade(tab);
                button.connect_clicked(move |_| {
                    let (Some(app), Some(tab)) = (app.upgrade(), tab.upgrade()) else {
                        return;
                    };
                    if !app.tabs.borrow().iter().any(|open| Rc::ptr_eq(open, &tab)) {
                        return;
                    }
                    if retry {
                        navigation::refresh(&tab);
                    } else {
                        navigation::navigate(&app, &tab, gio::File::for_path(glib::home_dir()));
                    }
                });
                buttons.append(&button);
            }
            page.set_child(Some(&buttons));
            tab.view_stack.add_named(&page, Some("unavailable"));
            page
        });
    let uri = tab
        .current_dir()
        .map(|file| file.uri().to_string())
        .unwrap_or_default();
    page.set_description(Some(&format!(
        "{uri}\n{error}\nRetry, or choose another location from the sidebar."
    )));
    tab.view_stack.set_visible_child_name("unavailable");
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::{SavedTab, State};
    use std::time::{Duration, Instant};

    async fn settle() {
        glib::timeout_future(Duration::from_millis(80)).await;
        for _ in 0..3 {
            for widget in gtk::Window::list_toplevels() {
                if widget.is_visible() && widget.width() > 0 {
                    widget.queue_allocate();
                    widget.allocate(widget.width(), widget.height(), -1, None);
                }
            }
            glib::timeout_future(Duration::from_millis(10)).await;
        }
    }

    async fn until(condition: impl Fn() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(
                start.elapsed() < Duration::from_secs(8),
                "tab recovery did not settle"
            );
            settle().await;
        }
        settle().await;
    }

    fn fixture() -> (adw::Application, Rc<App>, PathBuf) {
        let config =
            PathBuf::from(std::env::var_os("XDG_CONFIG_HOME").expect("isolated configuration"));
        assert!(config.starts_with(std::env::temp_dir()));
        std::fs::create_dir_all(config.join("filescope")).unwrap();
        std::fs::write(
            config.join("filescope/settings"),
            "automatic_updates=false\n",
        )
        .unwrap();
        adw::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let application = adw::Application::builder()
            .application_id("dev.filescope.TabRecoveryTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        application.register(gio::Cancellable::NONE).unwrap();
        let root =
            std::env::temp_dir().join(format!("filescope-recovery-{}", glib::uuid_string_random()));
        for name in ["a", "b", "c", "dest"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(root.join(name).join("notes.txt"), name).unwrap();
        }
        open_files(&application, &[gio::File::for_path(root.join("a"))]);
        let app = app_state(&application).unwrap();
        (application, app, root)
    }

    fn action(app: &App, name: &str) -> gio::Action {
        app.window
            .lookup_action(name)
            .unwrap_or_else(|| panic!("missing {name} action"))
    }

    fn close(app: &Rc<App>, tab: Rc<Tab>) -> std::rc::Weak<Tab> {
        let weak = Rc::downgrade(&tab);
        let page = tab.page.borrow().clone().unwrap();
        drop(tab);
        app.tab_view.close_page(&page);
        weak
    }

    #[test]
    fn closed_tab_retention_is_twenty_locations_in_recent_order() {
        let state = State::new(false);
        for index in 0..25 {
            state.remember(SavedTab {
                place: "folder",
                uri: format!("file:///tmp/{index}"),
                query: String::new(),
                preview: false,
                preview_width: 300,
            });
        }
        let closed = state.closed.borrow();
        assert_eq!(closed.len(), 20);
        assert_eq!(closed.back().unwrap().uri, "file:///tmp/24");
        assert_eq!(closed.front().unwrap().uri, "file:///tmp/5");
    }

    #[test]
    fn saved_locations_preserve_offline_uris_and_search_text() {
        let saved = SavedTab {
            place: "search",
            uri: "smb://offline/share/reports%20old".into(),
            query: "report\n=résumé".into(),
            preview: true,
            preview_width: 346,
        };
        let serialized = saved.value().to_string();
        assert!(
            !serialized.contains('\n'),
            "settings records must remain a single line"
        );
        assert_eq!(
            SavedTab::parse(&serde_json::from_str::<serde_json::Value>(&serialized).unwrap()),
            Some(saved)
        );
        assert!(
            SavedTab::parse(&serde_json::json!({"place":"folder","uri":"not a URI"})).is_none()
        );
        assert!(SavedTab::parse(&serde_json::json!({"place":"unknown"})).is_none());
    }



    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn hidden_preview_width_survives_recovery_without_opening_it() {
        let (_application, app, root) = fixture();
        glib::MainContext::default().block_on(async {
            navigation::new_tab(&app, gio::File::for_path(root.join("b")), true);
            let tab = navigation::active_tab(&app);
            tab.preview_pane.set_visible(true);
            tab.preview_pane.set_width(360);
            settle().await;
            let width = tab.preview_pane.width();
            assert!((width - 360).abs() < 20);
            tab.preview_pane.set_visible(false);
            assert!(close(&app, tab).upgrade().is_none());
            action(&app, "reopen-tab").activate(None);
            let recovered = navigation::active_tab(&app);
            assert!(!recovered.preview_pane.widget.is_visible());
            assert_eq!(recovered.preview_pane.width(), width);
            recovered.preview_pane.set_visible(true);
            settle().await;
            assert!((recovered.preview_pane.widget.width() - width).abs() < 20);
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn reopen_closed_tab_restores_own_location() {
        let (_application, app, root) = fixture();
        glib::MainContext::default().block_on(async {
            let original = navigation::active_tab(&app);
            navigation::new_tab(&app, gio::File::for_path(root.join("b")), true);
            let b = navigation::active_tab(&app);
            b.preview_pane.set_visible(true);
            settle().await;
            let split = &b.preview_pane.split;
            split.set_position(split.width() - 340);
            settle().await;
            let preview_width = b.preview_pane.widget.width();
            assert!(preview_width > 250);
            navigation::new_tab(&app, gio::File::for_path(root.join("c")), true);
            let c = navigation::active_tab(&app);
            c.preview_pane.set_visible(false);
            assert!(close(&app, c).upgrade().is_none());
            app.tab_view
                .set_selected_page(b.page.borrow().as_ref().unwrap());
            assert!(close(&app, b).upgrade().is_none());
            let reopen = action(&app, "reopen-tab");
            assert!(reopen.is_enabled());
            reopen.activate(None);
            let recovered_b = navigation::active_tab(&app);
            until(|| !recovered_b.dir_list.is_loading()).await;
            assert_eq!(
                recovered_b.current_dir().unwrap().path(),
                Some(root.join("b"))
            );
            assert!(recovered_b.preview_pane.widget.is_visible());
            assert!((recovered_b.preview_pane.widget.width() - preview_width).abs() < 20);
            assert_eq!(original.current_dir().unwrap().path(), Some(root.join("a")));
            reopen.activate(None);
            let recovered_c = navigation::active_tab(&app);
            assert_eq!(
                recovered_c.current_dir().unwrap().path(),
                Some(root.join("c"))
            );
            assert!(!recovered_c.preview_pane.widget.is_visible());
            assert!(!reopen.is_enabled());
            assert!(recovered_b.back.borrow().is_empty() && recovered_c.fwd.borrow().is_empty());
            assert!(
                ACCELS
                    .iter()
                    .any(|(name, keys)| *name == "reopen-tab" && keys.contains(&"<ctrl><shift>t"))
            );
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn session_restore_respects_opt_in() {
        let (application, app, root) = fixture();
        glib::MainContext::default().block_on(async {
            let restore = action(&app, "restore-session");
            assert_eq!(restore.state().unwrap().get::<bool>(), Some(false));
            restore.change_state(&true.to_variant());
            for name in ["b", "c"] {
                navigation::new_tab(&app, gio::File::for_path(root.join(name)), true);
            }
            let second = app.tabs.borrow()[1].clone();
            second.preview_pane.set_visible(true);
            app.tab_view
                .reorder_page(second.page.borrow().as_ref().unwrap(), 0);
            app.tab_view
                .set_selected_page(second.page.borrow().as_ref().unwrap());
            set_view_mode(&app, ViewMode::List);
            settle().await;
            app.window.close();
            open_files(&application, &[]);
            let restored = app_state(&application).unwrap();
            assert_eq!(restored.tabs.borrow().len(), 3);
            let paths: Vec<_> = restored
                .tabs
                .borrow()
                .iter()
                .map(|tab| tab.current_dir().unwrap().path().unwrap())
                .collect();
            assert_eq!(paths, ["b", "a", "c"].map(|name| root.join(name)));
            assert_eq!(
                navigation::active_tab(&restored)
                    .current_dir()
                    .unwrap()
                    .path(),
                Some(root.join("b"))
            );
            assert!(
                navigation::active_tab(&restored)
                    .preview_pane
                    .widget
                    .is_visible()
            );
            assert_eq!(restored.view_mode.get(), ViewMode::List);
            restored.window.close();
            open_files(&application, &[gio::File::for_path(root.join("c"))]);
            let explicit = app_state(&application).unwrap();
            assert_eq!(
                explicit.tabs.borrow().len(),
                1,
                "explicit launches must not add the old session"
            );
            assert_eq!(
                navigation::active_tab(&explicit)
                    .current_dir()
                    .unwrap()
                    .path(),
                Some(root.join("c"))
            );
            action(&explicit, "restore-session").change_state(&false.to_variant());
            explicit.window.close();
            open_files(&application, &[]);
            let clean = app_state(&application).unwrap();
            assert_eq!(clean.tabs.borrow().len(), 1);
            assert_eq!(
                navigation::active_tab(&clean)
                    .view_stack
                    .visible_child_name()
                    .as_deref(),
                Some("computer")
            );
            clean.window.close();
            let mut malformed = settings::load();
            malformed.restore_session = true;
            malformed.session = "{malformed".into();
            settings::save(&malformed);
            open_files(&application, &[]);
            let fallback = app_state(&application).unwrap();
            assert_eq!(fallback.tabs.borrow().len(), 1);
            assert_eq!(
                navigation::active_tab(&fallback)
                    .view_stack
                    .visible_child_name()
                    .as_deref(),
                Some("computer")
            );
            fallback.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn recovery_restores_special_places_and_search_scope() {
        let (_application, app, root) = fixture();
        glib::MainContext::default().block_on(async {
            for place in ["computer", "trash", "search"] {
                navigation::new_tab(&app, gio::File::for_path(root.join("b")), true);
                let tab = navigation::active_tab(&app);
                match place {
                    "computer" => places::show_computer(&app, &tab),
                    "trash" => navigation::go_trash(&app, &tab),
                    _ => navigation::open_place(
                        &app,
                        &tab,
                        Place::Search {
                            root: gio::File::for_path(root.join("b")),
                            query: "notes".into(),
                        },
                    ),
                }
                assert!(close(&app, tab).upgrade().is_none());
                action(&app, "reopen-tab").activate(None);
                let recovered = navigation::active_tab(&app);
                assert_eq!(
                    recovered.view_stack.visible_child_name().as_deref(),
                    Some(place)
                );
                if place == "search" {
                    assert!(recovered.search.active.get());
                    assert_eq!(*recovered.search.query.borrow(), "notes");
                    assert_eq!(
                        recovered.dir_list.file().unwrap().path(),
                        Some(root.join("b"))
                    );
                }
            }
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn recovery_unavailable_location_keeps_navigation_usable() {
        let (_application, app, root) = fixture();
        glib::MainContext::default().block_on(async {
            let missing = root.join("temporarily-offline");
            navigation::new_tab(&app, gio::File::for_path(&missing), true);
            close(&app, navigation::active_tab(&app));
            action(&app, "reopen-tab").activate(None);
            let recovered = navigation::active_tab(&app);
            until(|| recovered.dir_list.error().is_some()).await;
            assert_eq!(
                recovered.current_dir().unwrap().path(),
                Some(missing.clone())
            );
            assert_eq!(
                recovered.view_stack.visible_child_name().as_deref(),
                Some("unavailable")
            );
            let page = recovered
                .view_stack
                .child_by_name("unavailable")
                .and_downcast::<adw::StatusPage>()
                .unwrap();
            assert!(page.description().unwrap().contains("temporarily-offline"));
            assert!(app.window.lookup_action("up").unwrap().is_enabled());
            std::fs::create_dir(&missing).unwrap();
            std::fs::write(missing.join("back-online.txt"), "available").unwrap();
            action(&app, "refresh").activate(None);
            until(|| recovered.selection.n_items() == 1 && recovered.dir_list.error().is_none())
                .await;
            assert!(matches!(
                recovered.view_stack.visible_child_name().as_deref(),
                Some("grid" | "list")
            ));
            navigation::go_up(&app, &recovered);
            assert_eq!(recovered.current_dir().unwrap().path(), Some(root.clone()));
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn tab_recovery_preserves_background_lifecycle() {
        let (_application, app, root) = fixture();
        glib::MainContext::default().block_on(async {
            navigation::new_tab(&app, gio::File::for_path(root.join("b")), true);
            let closing = navigation::active_tab(&app);
            closing.terminal.toggle(Some(&root));
            until(|| closing.terminal.has_live_session()).await;
            let old_grid = closing.grid_view.clone();
            commands::start_job(
                &app,
                crate::ops::Kind::Copy,
                vec![root.join("a/notes.txt")],
                root.join("dest"),
                true,
            );
            until(|| app.window.visible_dialog().is_some()).await;
            let conflict = app
                .window
                .visible_dialog()
                .unwrap()
                .downcast::<adw::AlertDialog>()
                .unwrap();
            assert!(BackgroundWork::active());
            let weak = close(&app, closing);
            assert!(
                weak.upgrade().is_none(),
                "closed view callbacks retained the tab"
            );
            assert!(app.progress_box.first_child().is_some());
            action(&app, "reopen-tab").activate(None);
            let reopened = navigation::active_tab(&app);
            assert!(
                !reopened.terminal.has_live_session(),
                "recovery restarted the shell"
            );
            assert!(
                app.progress_box.first_child().is_some() && app.progress_revealer.reveals_child()
            );
            assert!(BackgroundWork::active());
            old_grid.emit_by_name::<()>("activate", &[&0_u32]);
            assert_eq!(reopened.current_dir().unwrap().path(), Some(root.join("b")));
            conflict.emit_by_name::<()>("response", &[&"rename"]);
            conflict.close();
            until(|| !BackgroundWork::active()).await;
            assert_eq!(
                std::fs::read_to_string(root.join("dest/notes (copy).txt")).unwrap(),
                "a"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("dest/notes.txt")).unwrap(),
                "dest"
            );
            assert!(app.progress_box.first_child().is_none());
            app.window.close();
        });
        std::fs::remove_dir_all(root).unwrap();
    }
}
