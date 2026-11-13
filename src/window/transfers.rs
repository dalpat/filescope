//! Presentation and retry of the existing worker's exact operation outcomes.
use crate::{format, ops, undo};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn estimate(total: u64, done: u64, elapsed: Duration, stalled: Duration) -> Option<(u64, u64)> {
    if total == 0
        || done == 0
        || done > total
        || elapsed < Duration::from_millis(300)
        || stalled > Duration::from_secs(2)
    {
        return None;
    }
    let speed = done as f64 / elapsed.as_secs_f64();
    let seconds = (total - done) as f64 / speed;
    if !speed.is_finite() || speed <= 0.0 || !seconds.is_finite() {
        return None;
    }
    Some((speed.round().max(1.0) as u64, seconds.ceil() as u64))
}

struct Meter {
    bytes: u64,
    items: u64,
    done: u64,
    completed: u64,
    current: String,
    baseline: u64,
    started: Option<Instant>,
    changed: Instant,
}
impl Meter {
    fn new() -> Self {
        Self {
            bytes: 0,
            items: 0,
            done: 0,
            completed: 0,
            current: String::new(),
            baseline: 0,
            started: None,
            changed: Instant::now(),
        }
    }
    fn paint(
        &self,
        title: &gtk::Label,
        bar: &gtk::ProgressBar,
        verb: &str,
        kind: ops::Kind,
        cancelling: bool,
    ) {
        if cancelling {
            title.set_label(&format!("Cancelling {verb}…"));
            bar.pulse();
            return;
        }
        if self.started.is_none() && self.items == 0 && self.bytes == 0 {
            title.set_label(&format!("Preparing {}…", verb.to_lowercase()));
            bar.pulse();
            return;
        }
        let fraction = if self.bytes > 0 {
            self.done as f64 / self.bytes as f64
        } else if self.items > 0 {
            self.completed as f64 / self.items as f64
        } else {
            0.0
        };
        bar.set_fraction(fraction.clamp(0.0, 1.0));
        let sizes = if self.bytes > 0 {
            format!(
                " · {} / {}",
                format::human_size(self.done),
                format::human_size(self.bytes)
            )
        } else {
            String::new()
        };
        let rate = if kind == ops::Kind::Copy {
            self.started.and_then(|start| {
                estimate(
                    self.bytes.saturating_sub(self.baseline),
                    self.done.saturating_sub(self.baseline),
                    start.elapsed(),
                    self.changed.elapsed(),
                )
            })
        } else {
            None
        };
        let timing = rate
            .map(|(speed, seconds)| {
                let eta = if seconds > 86400 {
                    "more than a day".into()
                } else if seconds >= 3600 {
                    format!("{}h {}m", seconds / 3600, seconds % 3600 / 60)
                } else if seconds >= 60 {
                    format!("{}m {}s", seconds / 60, seconds % 60)
                } else {
                    format!("{seconds}s")
                };
                format!(" · {}/s · about {eta} remaining", format::human_size(speed))
            })
            .unwrap_or_default();
        title.set_label(&format!(
            "{verb} {} — {}/{}{sizes}{timing}",
            self.current, self.completed, self.items
        ));
    }
}

pub(super) fn start(
    app: &Rc<super::App>,
    kind: ops::Kind,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    record: bool,
) {
    launch(app, kind, sources, dest, record, None);
}

fn launch(
    app: &Rc<super::App>,
    kind: ops::Kind,
    sources: Vec<PathBuf>,
    dest: PathBuf,
    record: bool,
    retries: Option<Vec<ops::Failure>>,
) {
    if sources.is_empty() {
        return;
    }
    let (verb, past) = match kind {
        ops::Kind::Copy => ("Copying", "Copied"),
        ops::Kind::Move => ("Moving", "Moved"),
        ops::Kind::Delete => ("Deleting", "Deleted"),
    };
    let mut removal = if matches!(kind, ops::Kind::Delete | ops::Kind::Move) {
        super::navigation::RemovalSelection::capture(app, &sources)
    } else {
        None
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, events) = async_channel::unbounded();
    let (decision_tx, decision_rx) = std::sync::mpsc::channel();
    let title = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    title.add_css_class("caption");
    title.set_label(&format!("Preparing {}…", verb.to_lowercase()));
    let bar = gtk::ProgressBar::builder()
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let stop = crate::icon::flat_button("process-stop-symbolic", "Cancel");
    let meter = Rc::new(RefCell::new(Meter::new()));
    {
        let cancel = cancel.clone();
        let decision = decision_tx.clone();
        let title = title.downgrade();
        stop.connect_clicked(move |stop| {
            if !cancel.swap(true, Ordering::Relaxed) {
                stop.set_sensitive(false);
                stop.set_tooltip_text(Some("Cancellation requested"));
                if let Some(title) = title.upgrade() {
                    title.set_label("Cancelling…");
                }
                let _ = decision.send(ops::Decision::Cancel);
            }
        });
    }
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    line.append(&bar);
    line.append(&stop);
    let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
    row.append(&title);
    row.append(&line);
    app.progress_box.append(&row);
    app.progress_revealer.set_reveal_child(true);
    let running = Rc::new(Cell::new(true));
    {
        let (running, meter, cancel) = (running.clone(), meter.clone(), cancel.clone());
        let (title, bar) = (title.downgrade(), bar.downgrade());
        glib::timeout_add_local(Duration::from_millis(250), move || {
            if !running.get() {
                return glib::ControlFlow::Break;
            }
            if let (Some(title), Some(bar)) = (title.upgrade(), bar.upgrade()) {
                meter
                    .borrow()
                    .paint(&title, &bar, verb, kind, cancel.load(Ordering::Relaxed));
            }
            glib::ControlFlow::Continue
        });
    }
    let work = super::BackgroundWork::new(app);
    {
        let (cancel, dest) = (cancel.clone(), dest.clone());
        std::thread::spawn(move || {
            if let Some(failures) = retries {
                ops::retry(kind, failures, dest, cancel, tx, decision_rx);
            } else {
                ops::run(kind, sources, dest, cancel, tx, decision_rx);
            }
        });
    }
    let app = app.clone();
    glib::spawn_future_local(async move {
        let _work = work;
        let mut conflict: Option<(glib::WeakRef<adw::AlertDialog>, Rc<Cell<bool>>)> = None;
        while let Ok(event) = events.recv().await {
            match event {
                ops::Event::Total {
                    bytes,
                    items,
                } => {
                    let mut state = meter.borrow_mut();
                    state.bytes = bytes;
                    state.items = items;
                    state.started = Some(Instant::now());
                }
                ops::Event::Advance {
                    bytes_done,
                    items_done,
                    current,
                } => {
                    let mut state = meter.borrow_mut();
                    if state.started.is_none() {
                        state.started = Some(Instant::now());
                        state.baseline = bytes_done;
                    }
                    if bytes_done != state.done {
                        state.changed = Instant::now();
                    }
                    state.done = bytes_done;
                    state.completed = items_done;
                    state.current = current;
                }
                ops::Event::Conflict { name } => {
                    meter.borrow_mut().started = None;
                    if app.window.application().is_none() || cancel.load(Ordering::Relaxed) {
                        let _ = decision_tx.send(ops::Decision::Cancel);
                        continue;
                    }
                    let dialog = adw::AlertDialog::new(
                        Some(&format!("“{name}” already exists")),
                        Some("An item with the same name is already in the destination."),
                    );
                    let all = gtk::CheckButton::with_label("Apply to all conflicts");
                    dialog.set_extra_child(Some(&all));
                    dialog.add_responses(&[
                        ("cancel", "Cancel"),
                        ("skip", "Skip"),
                        ("rename", "Keep Both"),
                        ("replace", "Replace"),
                    ]);
                    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
                    dialog.set_default_response(Some("rename"));
                    dialog.set_close_response("cancel");
                    let pending = Rc::new(Cell::new(true));
                    conflict = Some((dialog.downgrade(), pending.clone()));
                    let closed = pending.clone();
                    dialog.connect_closed(move |_| closed.set(false));
                    let decision = decision_tx.clone();
                    dialog.choose(&app.window, gio::Cancellable::NONE, move |response| {
                        pending.set(false);
                        let answer = match response.as_str() {
                            "replace" => {
                                if all.is_active() {
                                    ops::Decision::ReplaceAll
                                } else {
                                    ops::Decision::Replace
                                }
                            }
                            "skip" => {
                                if all.is_active() {
                                    ops::Decision::SkipAll
                                } else {
                                    ops::Decision::Skip
                                }
                            }
                            "rename" => ops::Decision::Rename,
                            _ => ops::Decision::Cancel,
                        };
                        let _ = decision.send(answer);
                    });
                }
                ops::Event::Finished {
                    ok,
                    failed,
                    pairs,
                    failures,
                    cancelled,
                } => {
                    running.set(false);
                    stop.set_sensitive(false);
                    if let Some((dialog, pending)) = conflict.take() {
                        if pending.replace(false) {
                            if let Some(dialog) = dialog.upgrade() {
                                dialog.close();
                            }
                        }
                    }
                    app.progress_box.remove(&row);
                    if app.progress_box.first_child().is_none() {
                        app.progress_revealer.set_reveal_child(false);
                    }
                    if record && !pairs.is_empty() {
                        let action = match kind {
                            ops::Kind::Copy => Some(undo::Action::Copy { pairs }),
                            ops::Kind::Move => Some(undo::Action::Move { pairs }),
                            ops::Kind::Delete => None,
                        };
                        if let Some(action) = action {
                            super::commands::push_undo(&app, action);
                        }
                    }
                    for tab in app.tabs.borrow().iter() {
                        super::navigation::refresh(tab);
                    }
                    if let Some(removal) = removal.take() {
                        removal.finish(&app);
                    }
                    super::update_chrome(&app);
                    let (message, failures) = present(verb, past, ok, failed, cancelled, failures);
                    super::toast(&app, &message);
                    if !failures.is_empty() && app.window.application().is_some() {
                        show_failures(&app, &message, kind, dest.clone(), record, failures);
                    }
                    return;
                }
            }
            meter
                .borrow()
                .paint(&title, &bar, verb, kind, cancel.load(Ordering::Relaxed));
        }
        running.set(false);
        app.progress_box.remove(&row);
        if app.progress_box.first_child().is_none() {
            app.progress_revealer.set_reveal_child(false);
        }
        super::toast(
            &app,
            "The file operation ended unexpectedly; check its source and destination",
        );
    });
}

fn present(
    verb: &str,
    past: &str,
    ok: u64,
    failed: u64,
    cancelled: bool,
    failures: Vec<ops::Failure>,
) -> (String, Vec<ops::Failure>) {
    let cache = cancelled
        && failures.iter().any(|failure| {
            failure.stopped() && failure.message.contains("pending cached data was kept")
        });
    let failures = if cancelled {
        failures
            .into_iter()
            .filter(|failure| !failure.stopped())
            .collect()
    } else {
        failures
    };
    let mut message = if cancelled && !failures.is_empty() {
        format!("{verb} cancelled — {ok} done; {} failed", failures.len())
    } else if cancelled {
        format!("{verb} cancelled — {ok} done")
    } else if failed > 0 {
        format!("{past} {ok}; {failed} failed")
    } else {
        format!("{past} {ok} item{}", super::plural(ok as usize))
    };
    if cache {
        message.push_str(". Pending cached data was kept");
    }
    (message, failures)
}

fn show_failures(
    app: &Rc<super::App>,
    message: &str,
    kind: ops::Kind,
    dest: PathBuf,
    record: bool,
    failures: Vec<ops::Failure>,
) {
    let details = failures
        .iter()
        .take(100)
        .map(|failure| {
            let source = failure
                .source
                .to_str()
                .map(str::to_owned)
                .unwrap_or_else(|| gio::File::for_path(&failure.source).uri().into());
            format!(
                "{source}: {}",
                failure.message.chars().take(600).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    text.buffer().set_text(&details);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&text)
        .min_content_height(100)
        .max_content_height(260)
        .propagate_natural_height(true)
        .build();
    let retry: Vec<_> = if record && kind != ops::Kind::Delete {
        failures
            .iter()
            .filter(|failure| failure.can_retry())
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let body = format!(
        "{} failed selection{}. Completed selections will not be repeated. Partially copied folders are checked for conflicts again.{}",
        failures.len(),
        super::plural(failures.len()),
        if failures.len() > 100 {
            " Showing the first 100 errors."
        } else {
            ""
        }
    );
    let dialog = adw::AlertDialog::new(Some(message), Some(&body));
    dialog.set_extra_child(Some(&scroll));
    dialog.add_response("close", "Close");
    if !retry.is_empty() {
        dialog.add_response("retry", "Retry Failed");
        dialog.set_response_appearance("retry", adw::ResponseAppearance::Suggested);
    }
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    let parent = app.window.clone();
    let app = Rc::downgrade(app);
    dialog.choose(&parent, gio::Cancellable::NONE, move |response| {
        if response == "retry" {
            if let Some(app) = app
                .upgrade()
                .filter(|app| app.window.application().is_some())
            {
                let sources = retry.iter().map(|failure| failure.source.clone()).collect();
                launch(&app, kind, sources, dest, record, Some(retry));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops;
    use gtk::{gio, glib};
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::time::Instant;

    async fn until(condition: impl Fn() -> bool) {
        settle("transfer UI did not settle", condition).await;
    }

    async fn settle(message: &str, condition: impl Fn() -> bool) {
        let start = Instant::now();
        while !condition() {
            assert!(start.elapsed() < Duration::from_secs(6), "{message}");
            glib::timeout_future(Duration::from_millis(20)).await;
        }
    }

    fn press(root: &gtk::Widget, label: &str) -> bool {
        if let Some(button) = root.downcast_ref::<gtk::Button>() {
            if button.label().as_deref() == Some(label) {
                button.emit_clicked();
                return true;
            }
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if press(&widget, label) {
                return true;
            }
            child = widget.next_sibling();
        }
        false
    }

    fn fixture() -> (adw::Application, Rc<super::super::App>, PathBuf) {
        adw::init().unwrap();
        gtk::Settings::default()
            .unwrap()
            .set_gtk_enable_animations(false);
        let root = PathBuf::from(std::env::var_os("TMPDIR").unwrap()).join("transfers");
        std::fs::create_dir_all(root.join("destination")).unwrap();
        let settings = glib::user_config_dir().join("filescope");
        std::fs::create_dir_all(&settings).unwrap();
        std::fs::write(settings.join("settings"), "automatic_updates=false\n").unwrap();
        let app = adw::Application::builder()
            .application_id("dev.filescope.TransferUiTest")
            .flags(gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(gio::Cancellable::NONE).unwrap();
        super::super::open_files(&app, &[gio::File::for_path(&root)]);
        let state = super::super::app_state(&app).unwrap();
        (app, state, root)
    }

    fn alert(app: &super::super::App) -> Option<adw::AlertDialog> {
        app.window
            .visible_dialog()
            .and_downcast::<adw::AlertDialog>()
    }


    #[test]
    fn transfer_estimates_handle_unknown_and_stalled_totals() {
        assert_eq!(
            estimate(1000, 500, Duration::from_secs(2), Duration::ZERO),
            Some((250, 2))
        );
        assert!(estimate(0, 0, Duration::from_secs(2), Duration::ZERO).is_none());
        assert!(estimate(100, 0, Duration::ZERO, Duration::ZERO).is_none());
        assert!(estimate(100, 200, Duration::from_secs(1), Duration::ZERO).is_none());
        assert!(estimate(1000, 500, Duration::from_secs(5), Duration::from_secs(3)).is_none());
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn transfer_retry_keeps_completed_results() {
        let (_app, state, root) = fixture();
        let first = root.join("first");
        let second = root.join("second");
        let destination = root.join("destination");
        std::fs::write(&first, "first source").unwrap();
        std::fs::write(&second, "second source").unwrap();
        std::fs::write(destination.join("second"), "old destination").unwrap();
        glib::MainContext::default().block_on(async {
            super::super::commands::start_job(
                &state,
                ops::Kind::Copy,
                vec![first, second.clone()],
                destination.clone(),
                true,
            );
            until(|| {
                alert(&state).is_some_and(|dialog| {
                    dialog
                        .heading()
                        .is_some_and(|heading| heading.contains("already exists"))
                })
            })
            .await;
            std::fs::write(destination.join("second"), "newer destination").unwrap();
            alert(&state)
                .unwrap()
                .emit_by_name::<()>("response", &[&"replace"]);
            until(|| {
                alert(&state).is_some_and(|dialog| {
                    dialog
                        .heading()
                        .is_some_and(|heading| heading.contains("failed"))
                })
            })
            .await;
            until(|| !super::super::BackgroundWork::active()).await;
            assert!(
                state.progress_box.first_child().is_none(),
                "failed details are not an active transfer"
            );
            assert_eq!(
                std::fs::read_to_string(destination.join("second")).unwrap(),
                "newer destination"
            );
            std::fs::write(destination.join("first"), "edited after completion").unwrap();
            alert(&state)
                .unwrap()
                .emit_by_name::<()>("response", &[&"retry"]);
            until(|| {
                alert(&state).is_some_and(|dialog| {
                    dialog
                        .heading()
                        .is_some_and(|heading| heading.contains("second"))
                })
            })
            .await;
            alert(&state)
                .unwrap()
                .emit_by_name::<()>("response", &[&"rename"]);
            until(|| {
                state.progress_box.first_child().is_none()
                    && !super::super::BackgroundWork::active()
            })
            .await;
            assert_eq!(
                std::fs::read_to_string(destination.join("first")).unwrap(),
                "edited after completion"
            );
            assert_eq!(
                std::fs::read_to_string(destination.join("second")).unwrap(),
                "newer destination"
            );
            assert_eq!(
                std::fs::read_to_string(destination.join("second (copy)")).unwrap(),
                "second source"
            );
            assert_eq!(std::fs::read_to_string(second).unwrap(), "second source");
            state.window.close();
        });
    }

    fn find_stop(root: &gtk::Widget) -> Option<gtk::Button> {
        if let Some(button) = root.downcast_ref::<gtk::Button>() {
            if button.tooltip_text().as_deref() == Some("Cancel") {
                return Some(button.clone());
            }
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(button) = find_stop(&widget) {
                return Some(button);
            }
            child = widget.next_sibling();
        }
        None
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn cancel_feedback_matches_worker_completion() {
        let (_app, state, root) = fixture();
        let source = root.join("source");
        let destination = root.join("destination");
        std::fs::write(&source, "source stays").unwrap();
        std::fs::write(destination.join("source"), "destination stays").unwrap();
        glib::MainContext::default().block_on(async {
            super::super::commands::start_job(
                &state,
                ops::Kind::Copy,
                vec![source.clone()],
                destination.clone(),
                true,
            );
            until(|| alert(&state).is_some()).await;
            let stop = find_stop(state.progress_box.upcast_ref()).unwrap();
            stop.emit_clicked();
            assert!(
                !stop.is_sensitive(),
                "cancel requested must be visible until worker finishes"
            );
            until(|| {
                state.progress_box.first_child().is_none()
                    && !super::super::BackgroundWork::active()
            })
            .await;
            assert!(state.window.visible_dialog().is_none());
            assert_eq!(std::fs::read_to_string(source).unwrap(), "source stays");
            assert_eq!(
                std::fs::read_to_string(destination.join("source")).unwrap(),
                "destination stays"
            );
            state.window.close();
        });
    }

    fn texts(root: &gtk::Widget) -> Vec<String> {
        let mut found = Vec::new();
        if let Some(label) = root.downcast_ref::<gtk::Label>() {
            found.push(label.text().to_string());
        }
        if let Some(button) = root.downcast_ref::<gtk::Button>() {
            if let Some(label) = button.label() {
                found.push(label.to_string());
            }
        }
        if let Some(view) = root.downcast_ref::<gtk::TextView>() {
            let buffer = view.buffer();
            found.push(
                buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .to_string(),
            );
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            found.extend(texts(&widget));
            child = widget.next_sibling();
        }
        found
    }

    #[test]
    #[ignore = "requires isolated GTK and temporary XDG directories"]
    fn cancelled_transfer_keeps_prior_failure_details() {
        let (_app, state, root) = fixture();
        let fifo = root.join("unsupported.fifo");
        let file = root.join("later.txt");
        let destination = root.join("destination");
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        std::fs::write(&file, "later source").unwrap();
        glib::MainContext::default().block_on(async {
            let (release, ready) = ops::pause_next_copy();
            super::super::commands::start_job(
                &state,
                ops::Kind::Copy,
                vec![fifo.clone(), file.clone()],
                destination.clone(),
                true,
            );
            settle("copy did not reach the pause", || ready.try_recv().is_ok()).await;
            find_stop(state.progress_box.upcast_ref())
                .unwrap()
                .emit_clicked();
            release.send(()).unwrap();
            settle("cancelled copy hid the earlier failure", || {
                alert(&state).is_some_and(|dialog| {
                    dialog.heading().is_some_and(|heading| {
                        heading.contains("cancelled") && heading.contains("failed")
                    })
                })
            })
            .await;
            settle("failure details kept the transfer active", || {
                !super::super::BackgroundWork::active()
            })
            .await;
            assert!(state.progress_box.first_child().is_none());
            let dialog = alert(&state).unwrap();
            let shown = texts(dialog.upcast_ref()).join("\n");
            assert!(
                shown.contains("unsupported.fifo"),
                "earlier failure was hidden after cancel: {shown}"
            );
            assert!(
                shown.contains("cannot be copied safely"),
                "failure detail was not the file-type error: {shown}"
            );
            assert!(
                !shown.contains("interrupted"),
                "cancellation was shown as an item failure: {shown}"
            );
            assert!(
                texts(dialog.upcast_ref()).iter().any(|text| text == "Retry Failed"),
                "eligible earlier failure lost Retry Failed"
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "later source");
            assert!(
                texts(state.toasts.upcast_ref())
                    .iter()
                    .any(|text| text == "Copying cancelled — 0 done; 1 failed"),
                "toast did not keep both cancellation and the earlier failure"
            );
            assert!(press(dialog.upcast_ref(), "Close"));
            settle("failure dialog stayed open", || alert(&state).is_none()).await;

            let plain = root.join("plain.txt");
            std::fs::write(&plain, "plain source").unwrap();
            let (release, ready) = ops::pause_next_copy();
            super::super::commands::start_job(
                &state,
                ops::Kind::Copy,
                vec![plain.clone()],
                destination.clone(),
                true,
            );
            settle("plain copy did not reach the pause", || {
                ready.try_recv().is_ok()
            })
            .await;
            find_stop(state.progress_box.upcast_ref())
                .unwrap()
                .emit_clicked();
            release.send(()).unwrap();
            settle("plain cancellation did not finish", || {
                state.progress_box.first_child().is_none()
                    && !super::super::BackgroundWork::active()
            })
            .await;
            assert!(alert(&state).is_none(), "cancel alone opened a failure dialog");
            assert_eq!(std::fs::read_to_string(&plain).unwrap(), "plain source");
            state.window.close();
        });
    }
}
