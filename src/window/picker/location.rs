//! Request-owned local folder loading and access checks.
use super::*;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Slot {
    generation: Cell<u64>,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
}
impl Slot {
    fn new() -> Self {
        Self {
            generation: Cell::new(0),
            cancel: RefCell::new(None),
        }
    }
    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(cancel) = self.cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    fn begin(&self) -> (u64, Arc<AtomicBool>) {
        self.cancel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel.replace(Some(cancel.clone()));
        (self.generation.get(), cancel)
    }
    fn current(&self, generation: u64) -> bool {
        self.generation.get() == generation
    }
}

pub(super) struct State {
    pub notice: gtk::Label,
    pub favorites: gtk::Box,
    pub recents: gtk::Box,
    pub current: RefCell<Option<PathBuf>>,
    pub unresolved: Cell<bool>,
    navigation: Slot,
    listing: Slot,
}
impl State {
    pub fn new() -> Self {
        Self {
            notice: gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .margin_start(12)
                .margin_end(12)
                .visible(false)
                .build(),
            favorites: gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .build(),
            recents: gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(6)
                .build(),
            current: RefCell::new(None),
            unresolved: Cell::new(false),
            navigation: Slot::new(),
            listing: Slot::new(),
        }
    }
    pub fn close(&self) {
        self.navigation.cancel();
        self.listing.cancel();
    }
}

pub(super) enum Intent {
    Initial,
    Visit(Option<PathBuf>),
    History { back: bool, current: PathBuf },
}

pub(super) fn display_path(path: &Path) -> String {
    path.to_str().map(str::to_owned).unwrap_or_else(|| gio::File::for_path(path).uri().to_string())
}
fn cancelled(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Relaxed) {
        Err("Request cancelled; no files were changed".into())
    } else {
        Ok(())
    }
}

pub(super) fn check_grant(
    paths: &[PathBuf],
    mode: &Mode,
    cancel: &AtomicBool,
) -> Result<(), String> {
    for path in paths {
        cancelled(cancel)?;
        match mode {
            Mode::Open {
                directory: false, ..
            } => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(path)
                    .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
                if !file
                    .metadata()
                    .map_err(|error| error.to_string())?
                    .is_file()
                {
                    return Err(format!("{} is no longer a regular file", path.display()));
                }
            }
            Mode::Open {
                directory: true, ..
            } => {
                std::fs::read_dir(path)
                    .map_err(|error| format!("Cannot read folder {}: {error}", path.display()))?;
            }
            _ => {
                let parent = path
                    .parent()
                    .ok_or("Choose an existing destination folder")?;
                let writable = |path: &Path, mode| -> Result<(), String> {
                    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
                        .map_err(|_| "Invalid destination path")?;
                    if unsafe { libc::access(path_c.as_ptr(), mode) } == 0 {
                        Ok(())
                    } else {
                        Err(format!(
                            "Cannot write {}: {}",
                            path.display(),
                            std::io::Error::last_os_error()
                        ))
                    }
                };
                writable(parent, libc::W_OK | libc::X_OK)?;
                if path.symlink_metadata().is_ok() {
                    writable(path, libc::W_OK)?;
                }
            }
        }
    }
    Ok(())
}

struct Folder {
    path: PathBuf,
    note: Option<String>,
}

fn folder(
    mut paths: Vec<PathBuf>,
    initial: bool,
    app_id: &str,
    cancel: &AtomicBool,
) -> Result<Folder, String> {
    if initial {
        for path in memory::recent(app_id)
            .into_iter()
            .chain(std::iter::once(glib::home_dir()))
        {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    let mut error = "Choose an existing folder".to_owned();
    for (index, path) in paths.into_iter().enumerate() {
        cancelled(cancel)?;
        let checked = std::fs::read_dir(&path)
            .map(|_| ())
            .map_err(|error| format!("Folder unavailable: {}: {error}", path.display()));
        match checked {
            Ok(()) => {
                let note = (initial && index > 0).then(|| {
                    format!(
                        "The requested or last-used folder is unavailable. Using {}.",
                        path.display()
                    )
                });
                return Ok(Folder {
                    path,
                    note,
                });
            }
            Err(failure) => {
                error = failure;
                if !initial {
                    return Err(error);
                }
            }
        }
    }
    Err(error)
}

pub(super) fn open(picker: &Rc<Picker>, paths: Vec<PathBuf>, intent: Intent) {
    picker.acceptance.cancel();
    picker.basket.cancel();
    search::stop(picker, false);
    picker.selection.unselect_all();
    picker.source.set_file(gio::File::NONE);
    picker.preview.set_target(None);
    picker.location.unresolved.set(true);
    picker.search.entry.set_sensitive(false);
    if let Some(path) = paths.first() {
        picker.path.set_text(&display_path(path));
    }
    picker
        .location
        .notice
        .set_label("Opening the requested location…");
    picker.location.notice.set_visible(true);
    picker.error.set_label("");
    let (generation, cancel) = picker.location.navigation.begin();
    let weak = Rc::downgrade(picker);
    let app_id = picker.navigation.app_id.clone();
    glib::spawn_future_local(async move {
        let worker_cancel = cancel.clone();
        let initial = matches!(intent, Intent::Initial);
        let result =
            gio::spawn_blocking(move || folder(paths, initial, &app_id, &worker_cancel)).await;
        let Some(picker) = weak.upgrade() else {
            return;
        };
        if !picker.location.navigation.current(generation)
            || cancel.load(Ordering::Relaxed)
            || picker.complete.borrow().is_none()
        {
            return;
        }
        match result {
            Ok(Ok(folder)) => {
                picker.location.current.replace(Some(folder.path.clone()));
                picker.location.unresolved.set(false);
                picker.set_folder(&folder.path);
                match intent {
                    Intent::Visit(previous) => {
                        if previous.as_ref() != Some(&folder.path) {
                            picker.navigation.visit(previous);
                        }
                    }
                    Intent::History { back, current } => {
                        picker.navigation.complete_history(back, current)
                    }
                    Intent::Initial => {}
                }
                picker.location.notice.set_visible(false);
                picker.search.entry.set_sensitive(true);
                if let Some(note) = folder.note {
                    picker.error.set_label(&note);
                }
            }
            Ok(Err(error)) => {
                picker.location.notice.set_visible(false);
                picker.error.set_label(&error);
            }
            Err(_) => {
                picker.location.notice.set_visible(false);
                picker.error.set_label("The location worker stopped. Choose another location or retry; no files were changed.");
            }
        }
    });
}

pub(super) fn connect(picker: &Rc<Picker>) {
    let (generation, cancel) = picker.location.listing.begin();
    let weak = Rc::downgrade(picker);
    let app_id = picker.navigation.app_id.clone();
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || (crate::bookmarks::load(), memory::recent(&app_id))).await;
        let Some(picker) = weak.upgrade() else { return; };
        if !picker.location.listing.current(generation) || cancel.load(Ordering::Relaxed)
            || picker.complete.borrow().is_none() { return; }
        match result {
            Ok((favorites, recent)) => {
                if let Some(popover) = picker.navigation.places.popover() {
                    navigation::section(&picker, &picker.location.favorites, "Favorites", favorites, &popover);
                    navigation::section(&picker, &picker.location.recents, "Recent folders", recent, &popover);
                }
            }
            Err(_) => picker.error.set_label("Could not load favorite and recent folders"),
        }
    });
}
