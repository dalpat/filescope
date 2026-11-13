//! One acceptance path: filesystem checks run on workers, UI stays request-owned.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Input {
    mode: Mode,
    folder: PathBuf,
    selected: Vec<PathBuf>,
    directory: Option<PathBuf>,
    name: OsString,
    search: bool,
    filter: Option<FilterSpec>,
}

pub(super) fn selected_directory(picker: &Picker) -> Option<PathBuf> {
    let selected = picker.selection.selection();
    if selected.size() != 1 {
        return None;
    }
    let info = picker
        .selection
        .item(selected.nth(0))
        .and_downcast::<gio::FileInfo>()?;
    if info.file_type() != gio::FileType::Directory {
        return None;
    }
    info.attribute_object("standard::file")
        .and_downcast::<gio::File>()?
        .path()
}

impl Input {
    pub fn capture(picker: &Picker) -> Result<Self, String> {
        let folder = picker.folder().ok_or("Choose an existing folder")?;
        Ok(Self {
            mode: picker.options.mode.clone(),
            folder,
            selected: picker.selected(),
            directory: selected_directory(picker),
            name: picker.exact_name(),
            search: picker.search.active.get(),
            filter: picker
                .options
                .filters
                .get(picker.filters.selected() as usize)
                .cloned(),
        })
    }

    fn resolve(&self, cancel: &AtomicBool) -> Result<Vec<PathBuf>, String> {
        if cancel.load(Ordering::Relaxed) { return Err("Request cancelled".into()); }
        let mut folder = self.folder.clone();
        if matches!(self.mode, Mode::SaveFiles(_))
            && let Some(directory) = &self.directory
        {
            folder = directory.clone();
        }
        if matches!(self.mode, Mode::Save)
            && self.search
            && self.selected.len() == 1
            && self.directory.is_none()
            && let Some(parent) = self.selected[0].parent()
        {
            folder = parent.to_path_buf();
        }
        let paths = match self.mode {
            Mode::Open { .. } => selection_paths(&self.mode, &folder, &self.selected),
            Mode::Save => save_paths(&Mode::SaveFiles(vec![self.name.clone()]), &folder, ""),
            _ => save_paths(&self.mode, &folder, ""),
        }?;
        Ok(paths)
    }
}

pub(super) struct State {
    pub busy: Cell<bool>,
    pub message: gtk::Label,
    generation: Cell<u64>,
    cancel: RefCell<Option<Arc<AtomicBool>>>,
    metadata_cancel: RefCell<Option<gio::Cancellable>>,
    #[cfg(test)]
    pub grant_gate: RefCell<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>,
}

impl State {
    pub fn new() -> Self {
        Self {
            busy: Cell::new(false),
            message: gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .visible(false)
                .build(),
            generation: Cell::new(0),
            cancel: RefCell::new(None),
            metadata_cancel: RefCell::new(None),
            #[cfg(test)]
            grant_gate: RefCell::new(None),
        }
    }
    pub fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.busy.set(false);
        self.message.set_visible(false);
        if let Some(cancel) = self.cancel.borrow_mut().take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(cancel) = self.metadata_cancel.borrow_mut().take() {
            cancel.cancel();
        }
    }
    fn begin(&self, message: &str) -> (u64, Arc<AtomicBool>, gio::Cancellable) {
        self.cancel();
        self.busy.set(true);
        self.message.set_label(message);
        self.message.set_visible(true);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel.replace(Some(cancel.clone()));
        let metadata = gio::Cancellable::new();
        self.metadata_cancel.replace(Some(metadata.clone()));
        (self.generation.get(), cancel, metadata)
    }
    fn current(&self, generation: u64) -> bool {
        generation == self.generation.get()
    }
    fn finish(&self) {
        self.busy.set(false);
        self.message.set_visible(false);
    }
}

fn fail(weak: &std::rc::Weak<Picker>, generation: u64, error: &str) {
    if let Some(picker) = weak.upgrade()
        && picker.acceptance.current(generation)
        && picker.complete.borrow().is_some()
    {
        picker.acceptance.finish();
        picker.error.set_label(error);
    }
}

pub(super) fn start(picker: &Rc<Picker>) {
    let input = match picker.destinations() {
        Ok(input) => input,
        Err(error) => {
            picker.error.set_label(&error);
            return;
        }
    };
    let (generation, cancel, metadata_cancel) = picker.acceptance.begin("Checking the selection…");
    picker.error.set_label("");
    let checked_input = input.clone();
    let weak = Rc::downgrade(picker);
    glib::spawn_future_local(async move {
        let worker_cancel = cancel.clone();
        let checked = gio::spawn_blocking(move || {
            let paths = checked_input.resolve(&worker_cancel)?;
            let snapshot = if matches!(checked_input.mode, Mode::Open { .. }) {
                None
            } else {
                Some(save::Snapshot::read(&paths)?)
            };
            Ok::<_, String>((paths, snapshot))
        })
        .await;
        let (paths, snapshot) = match checked {
            Ok(Ok(checked)) => checked,
            Ok(Err(error)) => {
                fail(&weak, generation, &error);
                return;
            }
            Err(_) => {
                fail(
                    &weak,
                    generation,
                    "The selection worker stopped. No destination was returned.",
                );
                return;
            }
        };
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        if matches!(
            input.mode,
            Mode::Open {
                directory: false,
                ..
            }
        ) && let Some(spec) = &input.filter
        {
            let filter = file_filter(spec);
            for path in &paths {
                let file = gio::File::for_path(path);
                match gio::CancellableFuture::new(
                    file.query_info_future(
                        super::super::ATTRS,
                        gio::FileQueryInfoFlags::NONE,
                        glib::Priority::DEFAULT,
                    ),
                    metadata_cancel.clone(),
                )
                .await
                {
                    Ok(Ok(info)) if filter.match_(&info) => {}
                    Ok(Ok(_)) => {
                        fail(
                            &weak,
                            generation,
                            "Select files matching the chosen file type",
                        );
                        return;
                    }
                    Ok(Err(error)) => {
                        fail(&weak, generation, &error.to_string());
                        return;
                    }
                    Err(_) => return,
                }
            }
        }
        let Some(picker) = weak.upgrade() else {
            return;
        };
        if !picker.acceptance.current(generation) || picker.complete.borrow().is_none() {
            return;
        }
        picker.acceptance.finish();
        if picker.destinations().as_ref() != Ok(&input) {
            picker
                .error
                .set_label("The destination changed. Review it and choose Save again.");
            return;
        }
        if snapshot
            .as_ref()
            .is_none_or(|snapshot| !snapshot.has_existing())
        {
            grant(&picker, paths, snapshot, Some(input), None);
            return;
        }
        let snapshot = snapshot.unwrap();
        let Some(window) = picker.window.upgrade() else {
            return;
        };
        let dialog = adw::AlertDialog::new(
            Some("Replace existing files?"),
            Some(&snapshot.existing_names().join("\n")),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("replace", "Replace")]);
        dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        picker.overwrite.set(true);
        let weak = Rc::downgrade(&picker);
        dialog.choose(&window, gio::Cancellable::NONE, move |response| {
            let Some(picker) = weak.upgrade() else {
                return;
            };
            picker.overwrite.set(false);
            if response != "replace" || picker.complete.borrow().is_none() {
                return;
            }
            if picker.destinations().as_ref() != Ok(&input) {
                picker
                    .error
                    .set_label("The destination changed. Review it and choose Save again.");
                return;
            }
            grant(&picker, paths, Some(snapshot), Some(input), None);
        });
    });
}

fn confirm_identities(paths: &[std::path::PathBuf], identities: &[(u64, u64)]) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if identities.len() != paths.len() {
        return Err("The selection changed. Remove it and select it again.".into());
    }
    for (path, (device, inode)) in paths.iter().zip(identities) {
        // Follow the link, as the basket's GIO query does. The returned path stays
        // the selected symlink; only a changed target inode is a different file.
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("selected file");
                return Err(format!(
                    "{name}: changed since it was selected; remove it and select it again"
                ));
            }
        };
        let same = meta.is_file()
            && meta.ino() == *inode
            && (meta.dev() == *device || meta.dev() as u32 as u64 == *device);
        if !same {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("selected file");
            return Err(format!(
                "{name}: changed since it was selected; remove it and select it again"
            ));
        }
    }
    Ok(())
}

pub(super) fn grant(
    picker: &Rc<Picker>,
    paths: Vec<PathBuf>,
    snapshot: Option<save::Snapshot>,
    input: Option<Input>,
    identities: Option<Vec<(u64, u64)>>,
) {
    if picker.acceptance.busy.get() || picker.complete.borrow().is_none() {
        return;
    }
    let mode = picker.options.mode.clone();
    let (generation, cancel, _) = picker
        .acceptance
        .begin("Checking file access…");
    picker.error.set_label("");
    let weak = Rc::downgrade(picker);
    #[cfg(test)]
    let gate = picker.acceptance.grant_gate.borrow_mut().take();
    glib::spawn_future_local(async move {
        let worker_cancel = cancel.clone();
        let result = gio::spawn_blocking(move || {
            #[cfg(test)]
            if let Some((reached, release)) = gate {
                let _ = reached.send(());
                release
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .map_err(|_| "The test access gate stopped".to_owned())?;
            }
            location::check_grant(&paths, &mode, &worker_cancel)?;
            if let Some(identities) = identities {
                confirm_identities(&paths, &identities)?;
            }
            if snapshot.is_some_and(|snapshot| !snapshot.unchanged()) {
                return Err("The destination changed. Review it and choose Save again.".to_owned());
            }
            Ok(paths)
        })
        .await;
        let Some(picker) = weak.upgrade() else {
            return;
        };
        if !picker.acceptance.current(generation)
            || cancel.load(Ordering::Relaxed)
            || picker.complete.borrow().is_none()
        {
            return;
        }
        picker.acceptance.finish();
        if input.is_some_and(|input| picker.destinations().as_ref() != Ok(&input)) {
            picker
                .error
                .set_label("The destination changed. Review it and choose Save again.");
            return;
        }
        match result {
            Ok(Ok(paths)) => picker.complete_accepted(&paths),
            Ok(Err(error)) => picker.error.set_label(&error),
            Err(_) => picker.error.set_label(
                "The access worker stopped. No destination was returned; no files were changed.",
            ),
        }
    });
}
