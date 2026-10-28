//! Allocation and scrolling checks need a real GTK display, not helper-only tests.
use super::*;
use std::time::Duration;

async fn settle() {
    glib::timeout_future(Duration::from_millis(200)).await;
    // Broadway can defer frames without a visible client; still exercise native GTK allocation.
    let context = glib::MainContext::default();
    for _ in 0..3 {
        for widget in gtk::Window::list_toplevels() {
            if widget.is_visible() && widget.width() > 0 {
                widget.queue_allocate();
                widget.allocate(widget.width(), widget.height(), -1, None);
            }
        }
        for _ in 0..25 {
            if !context.pending() {
                break;
            }
            context.iteration(false);
        }
    }
}

fn widgets(root: &gtk::Widget) -> Vec<gtk::Widget> {
    let mut result = vec![root.clone()];
    let mut child = root.first_child();
    while let Some(widget) = child {
        result.extend(widgets(&widget));
        child = widget.next_sibling();
    }
    result
}

fn fixture() -> (adw::Application, Rc<App>, Rc<Tab>, PathBuf) {
    assert!(std::env::var_os("XDG_CONFIG_HOME").is_some());
    adw::init().unwrap();
    gtk::Settings::default()
        .unwrap()
        .set_gtk_enable_animations(false);
    let application = adw::Application::builder()
        .application_id("dev.filescope.PreviewResizeTest")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    application.register(gio::Cancellable::NONE).unwrap();
    let root =
        std::env::temp_dir().join(format!("filescope-preview-resize-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("log.txt"),
        format!("{}\n", "long original line ".repeat(100)),
    )
    .unwrap();
    for (name, color) in [
        ("wide.png", 0xff0000ff),
        ("other.png", 0x00ff00ff),
        ("portrait.png", 0x0000ffff),
    ] {
        let (width, height) = if name == "portrait.png" {
            (800, 1600)
        } else {
            (1600, 800)
        };
        let image =
            gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, width, height)
                .unwrap();
        image.fill(color);
        image.savev(root.join(name), "png", &[]).unwrap();
    }
    open_files(&application, &[gio::File::for_path(&root)]);
    let app = app_state(&application).unwrap();
    app.window.set_default_size(1120, 740);
    let tab = navigation::active_tab(&app);
    tab.preview_pane.set_visible(true);
    (application, app, tab, root)
}

fn splitter(tab: &Tab) -> gtk::Paned {
    tab.preview_pane
        .widget
        .parent()
        .and_downcast::<gtk::Paned>()
        .expect("preview must have a draggable divider")
}

async fn target(tab: &Rc<Tab>, path: &std::path::Path) {
    while tab.dir_list.is_loading() {
        settle().await;
    }
    navigation::select_names(
        tab,
        &[path.file_name().unwrap().to_string_lossy().into_owned()],
        || {},
    );
    for _ in 0..100 {
        settle().await;
        if widgets(tab.preview_pane.widget.upcast_ref())
            .iter()
            .any(|w| w.is::<gtk::TextView>() || w.is::<gtk::Picture>())
        {
            return;
        }
    }
    panic!("preview failed to load");
}

fn resize(tab: &Tab, width: i32) {
    let pane = splitter(tab);
    pane.set_position(pane.width() - width);
    pane.allocate(pane.width(), pane.height(), -1, None);
}

fn text_view(tab: &Tab) -> gtk::TextView {
    widgets(tab.preview_pane.widget.upcast_ref())
        .into_iter()
        .find_map(|w| w.downcast().ok())
        .unwrap()
}

fn picture(tab: &Tab) -> gtk::Picture {
    widgets(tab.preview_pane.widget.upcast_ref())
        .into_iter()
        .find_map(|w| w.downcast().ok())
        .unwrap()
}

fn cleanup(app: &App, root: PathBuf) {
    app.window.close();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
fn preview_divider_resizes_without_locking_window() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        target(&tab, &root.join("log.txt")).await;
        let original = tab.preview_pane.widget.width();
        resize(&tab, 480);
        settle().await;
        assert!(
            tab.preview_pane.widget.width() > original + 100,
            "original={original}, resized={}, split={} position={}",
            tab.preview_pane.widget.width(),
            splitter(&tab).width(),
            splitter(&tab).position()
        );
        resize(&tab, 220);
        settle().await;
        assert!(tab.preview_pane.widget.width() < 250);
        tab.terminal.toggle(Some(&root));
        settle().await;
        resize(&tab, 480);
        settle().await;
        assert!(app.window.measure(gtk::Orientation::Horizontal, -1).0 <= 800);
        app.window.set_visible(false);
        app.window.set_default_size(800, 600);
        app.window.present();
        settle().await;
        assert!(
            app.window.width() <= 800,
            "expanded preview must not lock window width"
        );
        assert!(tab.view_stack.width() >= 160);
        assert!(tab.preview_pane.widget.width() >= 180);
        cleanup(&app, root);
    });
}

#[test]
#[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
fn preview_text_wrap_is_opt_in() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        target(&tab, &root.join("log.txt")).await;
        let view = text_view(&tab);
        let buffer = view.buffer();
        let original = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string();
        assert_eq!(view.wrap_mode(), gtk::WrapMode::None);
        let wrap = widgets(tab.preview_pane.widget.upcast_ref())
            .into_iter()
            .find_map(|w| w.downcast::<gtk::ToggleButton>().ok())
            .expect("explicit wrap control");
        let scroll = view.parent().and_downcast::<gtk::ScrolledWindow>().unwrap();
        assert_eq!(scroll.hscrollbar_policy(), gtk::PolicyType::Automatic);
        assert!(scroll.hadjustment().upper() > scroll.hadjustment().page_size());
        resize(&tab, 220);
        settle().await;
        assert_eq!(
            view.wrap_mode(),
            gtk::WrapMode::None,
            "narrowing must not enable wrapping"
        );
        wrap.set_active(true);
        settle().await;
        assert_eq!(view.wrap_mode(), gtk::WrapMode::WordChar);
        resize(&tab, 220);
        settle().await;
        assert!(scroll.hadjustment().upper() <= scroll.hadjustment().page_size() + 1.0);
        wrap.set_active(false);
        settle().await;
        assert_eq!(view.wrap_mode(), gtk::WrapMode::None);
        assert!(scroll.hadjustment().upper() > scroll.hadjustment().page_size());
        assert_eq!(
            buffer.text(&buffer.start_iter(), &buffer.end_iter(), true),
            original
        );
        assert_eq!(
            std::fs::read_to_string(root.join("log.txt")).unwrap(),
            original
        );
        cleanup(&app, root);
    });
}

#[test]
#[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
fn preview_resize_preserves_tab_width() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        target(&tab, &root.join("log.txt")).await;
        resize(&tab, 420);
        settle().await;
        let width = tab.preview_pane.widget.width();
        tab.preview_pane.set_visible(false);
        settle().await;
        tab.preview_pane.set_visible(true);
        settle().await;
        assert!(
            (tab.preview_pane.widget.width() - width).abs() <= 2,
            "wanted={width}, actual={}",
            tab.preview_pane.widget.width()
        );
        navigation::new_tab(&app, gio::File::for_path(&root), true);
        let other = navigation::active_tab(&app);
        other.preview_pane.set_visible(true);
        settle().await;
        resize(&other, 230);
        settle().await;
        app.tab_view
            .set_selected_page(tab.page.borrow().as_ref().unwrap());
        settle().await;
        assert!(
            (tab.preview_pane.widget.width() - width).abs() <= 2,
            "wanted={width}, actual={}",
            tab.preview_pane.widget.width()
        );
        for mode in [ViewMode::List, ViewMode::Columns, ViewMode::Grid] {
            set_view_mode(&app, mode);
            settle().await;
            assert!((tab.preview_pane.widget.width() - width).abs() <= 2);
            assert!(tab.preview_pane.widget.is_visible());
        }
        tab.search_bar.set_search_mode(true);
        tab.search_entry.set_text("log");
        search::changed(&app, &tab, false);
        settle().await;
        assert!((tab.preview_pane.widget.width() - width).abs() <= 2);
        tab.search_bar.set_search_mode(false);
        settle().await;
        app.tab_view
            .set_selected_page(other.page.borrow().as_ref().unwrap());
        settle().await;
        assert!(
            other.preview_pane.widget.width() < 260,
            "second tab width={}",
            other.preview_pane.widget.width()
        );
        cleanup(&app, root);
    });
}

#[test]
#[ignore = "requires an isolated graphical session and temporary XDG_CONFIG_HOME"]
fn preview_image_resize_rejects_stale_results() {
    let (_application, app, tab, root) = fixture();
    glib::MainContext::default().block_on(async {
        target(&tab, &root.join("wide.png")).await;
        let initial = picture(&tab).paintable().unwrap().intrinsic_width();
        resize(&tab, 600);
        settle().await;
        for _ in 0..50 {
            if picture(&tab).paintable().unwrap().intrinsic_width() > initial + 100 {
                break;
            }
            settle().await;
        }
        let large = picture(&tab);
        assert!(
            large.paintable().unwrap().intrinsic_width() > initial + 100,
            "initial={initial}, current={}, pane={}",
            large.paintable().unwrap().intrinsic_width(),
            tab.preview_pane.widget.width()
        );
        assert!(
            (large.width() - 2 * large.height()).abs() <= 3,
            "image must preserve aspect ratio: {}x{}",
            large.width(),
            large.height()
        );
        for width in [200, 550, 240, 520] {
            resize(&tab, width);
        }
        target(&tab, &root.join("other.png")).await;
        for _ in 0..10 {
            settle().await;
        }
        let image = picture(&tab)
            .paintable()
            .unwrap()
            .downcast::<gtk::gdk::Texture>()
            .unwrap();
        let stride = image.width() as usize * 4;
        let mut pixels = vec![0; stride * image.height() as usize];
        image.download(&mut pixels, stride);
        assert!(
            pixels[1] > 200 && pixels[0] < 20 && pixels[2] < 20,
            "obsolete red image replaced the green target"
        );
        target(&tab, &root.join("portrait.png")).await;
        let portrait = picture(&tab);
        assert!((portrait.height() - 2 * portrait.width()).abs() <= 3);
        assert!(portrait.paintable().unwrap().intrinsic_width() >= portrait.width() - 2);
        resize(&tab, 220);
        settle().await;
        assert!((picture(&tab).height() - 2 * picture(&tab).width()).abs() <= 3);
        resize(&tab, 600);
        tab.preview_pane.set_visible(false);
        settle().await;
        assert!(!tab.preview_pane.widget.is_mapped());
        tab.preview_pane.set_visible(true);
        settle().await;
        target(&tab, &root.join("log.txt")).await;
        for _ in 0..5 {
            settle().await;
        }
        assert!(
            widgets(tab.preview_pane.widget.upcast_ref())
                .iter()
                .all(|w| !w.is::<gtk::Picture>())
        );
        cleanup(&app, root);
    });
}
