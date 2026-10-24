//! GTK loads folders; GIO updates changed entries without retaining closed tabs.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use gtk::{gio, glib, prelude::*};

type Update = (glib::GString, gio::Cancellable, Result<gio::FileInfo, glib::Error>);
const UPDATE_BATCH: usize = 32;

pub(super) struct Contents {
    pub items: gio::ListStore,
    source: gtk::DirectoryList,
    monitor: RefCell<Option<gio::FileMonitor>>,
    requests: RefCell<HashMap<glib::GString, gio::Cancellable>>,
    waiting: RefCell<HashMap<glib::GString, gio::File>>,
    positions: RefCell<HashMap<glib::GString, u32>>,
    ready: RefCell<VecDeque<Update>>,
    flush: RefCell<Option<glib::SourceId>>,
    generation: Cell<u64>,
    stopped: Cell<bool>,
}

impl Contents {
    pub fn new(source: &gtk::DirectoryList) -> Rc<Self> {
        // GTK 4.22's monitor callbacks use freed QueuedEvents after list disposal.
        // Keep its streaming loader, but never enable its built-in monitor.
        assert!(!source.is_monitored());
        let this = Rc::new(Self {
            items: gio::ListStore::new::<gio::FileInfo>(),
            source: source.clone(),
            monitor: RefCell::new(None),
            requests: RefCell::new(HashMap::new()),
            waiting: RefCell::new(HashMap::new()),
            positions: RefCell::new(HashMap::new()),
            ready: RefCell::new(VecDeque::new()),
            flush: RefCell::new(None),
            generation: Cell::new(0),
            stopped: Cell::new(false),
        });
        let weak = Rc::downgrade(&this);
        source.connect_items_changed(move |source, position, removed, added| {
            let Some(this) = weak.upgrade().filter(|this| !this.stopped.get()) else { return; };
            if source.n_items() == 0 {
                // Live changes may have altered the count since GTK's initial listing.
                this.positions.borrow_mut().clear();
                this.items.remove_all();
            } else {
                let additions: Vec<_> = (position..position + added)
                    .filter_map(|i| source.item(i).and_downcast::<gio::FileInfo>()).collect();
                for (offset, info) in additions.iter().enumerate() {
                    if let Some(file) = info.attribute_object("standard::file").and_downcast::<gio::File>() {
                        this.positions.borrow_mut().insert(file.uri(), position + offset as u32);
                    }
                }
                this.items.splice(position, removed, &additions);
            }
        });
        let weak = Rc::downgrade(&this);
        source.connect_file_notify(move |_| {
            if let Some(this) = weak.upgrade() { this.watch(); }
        });
        let weak = Rc::downgrade(&this);
        source.connect_loading_notify(move |source| {
            if source.is_loading() { return; }
            let Some(this) = weak.upgrade() else { return; };
            let waiting = std::mem::take(&mut *this.waiting.borrow_mut());
            for file in waiting.into_values() { this.query(&file); }
        });
        this
    }

    fn cancel(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(flush) = self.flush.borrow_mut().take() { flush.remove(); }
        self.ready.borrow_mut().clear();
        if let Some(monitor) = self.monitor.borrow_mut().take() { monitor.cancel(); }
        for (_, request) in self.requests.borrow_mut().drain() { request.cancel(); }
        self.waiting.borrow_mut().clear();
    }

    pub fn stop(&self) {
        if self.stopped.replace(true) { return; }
        self.cancel();
        self.source.set_file(gio::File::NONE);
    }

    fn watch(self: &Rc<Self>) {
        self.cancel();
        if self.stopped.get() { return; }
        self.positions.borrow_mut().clear();
        self.items.remove_all();
        let Some(file) = self.source.file() else { return; };
        let Ok(monitor) = file.monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE) else { return; };
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, file, other, event| {
            let Some(this) = weak.upgrade() else { return; };
            match event {
                gio::FileMonitorEvent::Created | gio::FileMonitorEvent::Deleted
                | gio::FileMonitorEvent::MovedIn | gio::FileMonitorEvent::MovedOut
                | gio::FileMonitorEvent::Renamed | gio::FileMonitorEvent::AttributeChanged
                | gio::FileMonitorEvent::ChangesDoneHint => {
                    this.query(file);
                    if let Some(other) = other { this.query(other); }
                }
                _ => {}
            }
        });
        *self.monitor.borrow_mut() = Some(monitor);
    }

    fn query(self: &Rc<Self>, file: &gio::File) {
        if self.stopped.get() { return; }
        if !self.source.file().is_some_and(|dir| file.parent().is_some_and(|parent| dir.equal(&parent))) { return; }
        let uri = file.uri();
        if self.source.is_loading() {
            self.waiting.borrow_mut().insert(uri, file.clone());
            return;
        }
        let request = gio::Cancellable::new();
        if let Some(previous) = self.requests.borrow_mut().insert(uri.clone(), request.clone()) {
            previous.cancel();
        }
        let weak = Rc::downgrade(self);
        let file_copy = file.clone();
        let current = request.clone();
        file.query_info_async(super::ATTRS, gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT,
            Some(&request), move |result| {
                let Some(this) = weak.upgrade() else { return; };
                if this.stopped.get() || this.requests.borrow().get(&uri) != Some(&current) { return; }
                if let Ok(info) = &result { info.set_attribute_object("standard::file", &file_copy); }
                this.ready.borrow_mut().push_back((uri, current, result));
                this.schedule();
            });
    }

    fn schedule(self: &Rc<Self>) {
        if self.flush.borrow().is_some() { return; }
        let weak = Rc::downgrade(self);
        *self.flush.borrow_mut() = Some(glib::timeout_add_local_once(std::time::Duration::from_millis(1), move || {
            let Some(this) = weak.upgrade() else { return; };
            this.flush.borrow_mut().take();
            this.apply_ready();
            if !this.ready.borrow().is_empty() { this.schedule(); }
        }));
    }

    fn apply_ready(&self) {
        let generation = self.generation.get();
        let mut deleted = Vec::new();
        for _ in 0..UPDATE_BATCH {
            let Some((uri, request, result)) = self.ready.borrow_mut().pop_front() else { break; };
            if self.requests.borrow().get(&uri) != Some(&request) { continue; }
            self.requests.borrow_mut().remove(&uri);
            let position = self.positions.borrow().get(&uri).copied();
            match result {
                Ok(info) => {
                    if let Some(position) = position {
                        let existing = self.items.item(position).and_downcast::<gio::FileInfo>().unwrap();
                        // GTK preserves selection across reordering when the object stays the same.
                        info.copy_into(&existing);
                        self.items.splice(position, 1, &[existing]);
                    } else {
                        self.positions.borrow_mut().insert(uri, self.items.n_items());
                        self.items.append(&info);
                    }
                }
                Err(error) if error.matches(gio::IOErrorEnum::NotFound) => {
                    if let Some(position) = position { deleted.push(position); }
                }
                _ => {} // Failed metadata reads must not remove an existing entry.
            }
            // Model-change handlers can navigate or close the tab synchronously.
            if self.generation.get() != generation { return; }
        }
        if deleted.is_empty() { return; }
        deleted.sort_unstable();
        // Rebase once per batch, not once per deleted file.
        self.positions.borrow_mut().retain(|_, position| {
            let before = deleted.partition_point(|removed| removed < position);
            if deleted.get(before) == Some(position) { return false; }
            *position -= before as u32;
            true
        });
        for position in deleted.into_iter().rev() {
            self.items.remove(position);
            if self.generation.get() != generation { return; }
        }
    }
}

impl Drop for Contents {
    fn drop(&mut self) { self.stop(); }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for(condition: impl Fn() -> bool) {
        let start = std::time::Instant::now();
        while !condition() {
            assert!(start.elapsed().as_secs() < 5, "folder update timed out");
            glib::timeout_future(std::time::Duration::from_millis(10)).await;
        }
    }

    fn info(contents: &Contents, name: &str) -> Option<gio::FileInfo> {
        (0..contents.items.n_items())
            .filter_map(|i| contents.items.item(i).and_downcast::<gio::FileInfo>())
            .find(|info| info.name() == std::path::Path::new(name))
    }

    #[test]
    #[ignore = "requires an isolated graphical session"]
    fn folder_monitor_updates_entries_and_discards_old_directory_results() {
        gtk::init().unwrap();
        let root = std::env::temp_dir().join(format!("filescope-monitor-updates-{}", std::process::id()));
        for dir in ["one", "two"] { std::fs::create_dir_all(root.join(dir)).unwrap(); }
        std::fs::write(root.join("one/keep"), "keep").unwrap();
        let context = glib::MainContext::default();
        context.with_thread_default(|| context.block_on(async {
            let source = gtk::DirectoryList::builder().attributes(super::super::ATTRS).monitored(false).build();
            let contents = Contents::new(&source);
            source.set_file(Some(&gio::File::for_path(root.join("one"))));
            std::fs::write(root.join("one/during-load"), "new").unwrap();
            contents.query(&gio::File::for_path(root.join("one/during-load")));
            assert_eq!(contents.waiting.borrow().len(), 1);
            wait_for(|| !source.is_loading() && contents.items.n_items() == 2).await;
            std::fs::remove_file(root.join("one/during-load")).unwrap();
            wait_for(|| !source.is_loading() && contents.items.n_items() == 1).await;
            let filtered = gtk::FilterListModel::new(Some(contents.items.clone()), None::<gtk::Filter>);
            let sorted = gtk::SortListModel::new(Some(filtered), None::<gtk::Sorter>);
            let selection = gtk::MultiSelection::new(Some(sorted));
            selection.select_item(0, true);

            std::fs::write(root.join("one/keep"), "changed while selected").unwrap();
            wait_for(|| info(&contents, "keep").is_some_and(|i| i.size() == 22)).await;
            assert_eq!(selection.selection().size(), 1, "saving a selected file must keep it selected");

            std::fs::write(root.join("one/added"), "new").unwrap();
            wait_for(|| info(&contents, "added").is_some()).await;
            assert_eq!(contents.items.n_items(), 2);
            assert_eq!(selection.selection().size(), 1);
            assert_eq!(selection.item(selection.selection().minimum()).and_downcast::<gio::FileInfo>().unwrap().name(), std::path::Path::new("keep"));
            std::fs::rename(root.join("one/added"), root.join("one/renamed")).unwrap();
            wait_for(|| info(&contents, "renamed").is_some() && info(&contents, "added").is_none()).await;
            std::fs::write(root.join("one/renamed"), "longer contents").unwrap();
            wait_for(|| info(&contents, "renamed").is_some_and(|i| i.size() == 15)).await;
            std::fs::create_dir(root.join("one/folder")).unwrap();
            wait_for(|| info(&contents, "folder").is_some_and(|i| i.file_type() == gio::FileType::Directory)).await;
            std::fs::remove_file(root.join("one/renamed")).unwrap();
            wait_for(|| info(&contents, "renamed").is_none()).await;

            // A queued completion from the old folder must not populate the new one.
            contents.query(&gio::File::for_path(root.join("one/keep")));
            let first = contents.requests.borrow().values().next().unwrap().clone();
            contents.query(&gio::File::for_path(root.join("one/keep")));
            assert!(first.is_cancelled());
            assert_eq!(contents.requests.borrow().len(), 1, "separate GFile objects share one URI request");
            source.set_file(Some(&gio::File::for_path(root.join("two"))));
            wait_for(|| !source.is_loading()).await;
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            assert_eq!(contents.items.n_items(), 0);
            std::fs::write(root.join("two/live"), "live").unwrap();
            wait_for(|| info(&contents, "live").is_some()).await;
            // GTK's original empty listing has no rows to clear after live additions.
            source.set_file(gio::File::NONE);
            assert_eq!(contents.items.n_items(), 0);
            source.set_file(Some(&gio::File::for_path(root.join("one"))));
            wait_for(|| !source.is_loading() && contents.items.n_items() == 2).await;
            assert!(info(&contents, "live").is_none());
            contents.stop();
        })).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session"]
    fn folder_bursts_preserve_selection_across_sorting_and_deletion() {
        gtk::init().unwrap();
        let root = std::env::temp_dir().join(format!("filescope-monitor-burst-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..128 { std::fs::write(root.join(format!("file-{i:03}")), vec![b'x'; i + 1]).unwrap(); }
        let context = glib::MainContext::default();
        context.with_thread_default(|| context.block_on(async {
            let source = gtk::DirectoryList::builder().attributes(super::super::ATTRS).monitored(false).build();
            let contents = Contents::new(&source);
            source.set_file(Some(&gio::File::for_path(&root)));
            wait_for(|| !source.is_loading() && contents.items.n_items() == 128).await;
            let sorter = gtk::CustomSorter::new(|a, b| {
                a.downcast_ref::<gio::FileInfo>().unwrap().size()
                    .cmp(&b.downcast_ref::<gio::FileInfo>().unwrap().size()).into()
            });
            let filtered = gtk::FilterListModel::new(Some(contents.items.clone()), None::<gtk::Filter>);
            let sorted = gtk::SortListModel::new(Some(filtered), Some(sorter));
            let selection = gtk::MultiSelection::new(Some(sorted.clone()));
            selection.select_item(0, false);
            selection.select_item(127, false);
            let selected_names = || {
                let selected = selection.selection();
                let mut names: Vec<_> = (0..selected.size()).map(|i| {
                    selection.item(selected.nth(i as u32)).and_downcast::<gio::FileInfo>().unwrap().name()
                }).collect();
                names.sort();
                names
            };
            let expected = vec![std::path::PathBuf::from("file-000"), std::path::PathBuf::from("file-127")];
            std::fs::write(root.join("file-000"), vec![b'x'; 256]).unwrap();
            wait_for(|| sorted.item(127).and_downcast::<gio::FileInfo>().is_some_and(|i| i.name() == std::path::Path::new("file-000"))).await;
            assert_eq!(selected_names(), expected, "size sorting must retain the selected files");

            // Atomic editor saves replace the inode while retaining the same displayed URI.
            let staging = root.with_extension("staging");
            std::fs::write(&staging, vec![b'x'; 300]).unwrap();
            std::fs::rename(&staging, root.join("file-000")).unwrap();
            wait_for(|| info(&contents, "file-000").is_some_and(|i| i.size() == 300)).await;
            assert_eq!(selected_names(), expected);

            for i in 1..97 { std::fs::remove_file(root.join(format!("file-{i:03}"))).unwrap(); }
            wait_for(|| contents.items.n_items() == 32).await;
            for i in 0..96 { std::fs::write(root.join(format!("new-{i:03}")), "new").unwrap(); }
            wait_for(|| contents.items.n_items() == 128).await;
            std::fs::write(root.join("file-127"), vec![b'x'; 400]).unwrap();
            wait_for(|| info(&contents, "file-127").is_some_and(|i| i.size() == 400)).await;
            assert_eq!(selected_names(), expected, "deletion batches must rebase remaining rows correctly");

            std::fs::remove_file(root.join("file-000")).unwrap();
            wait_for(|| info(&contents, "file-000").is_none()).await;
            std::fs::write(root.join("file-000"), "replacement").unwrap();
            wait_for(|| info(&contents, "file-000").is_some()).await;
            assert_eq!(selected_names(), vec![std::path::PathBuf::from("file-127")], "a newly created file must not inherit a deleted file's selection");

            // A synchronous model callback may navigate while a completion batch is being applied.
            let list = source.downgrade();
            let handler = contents.items.connect_items_changed(move |_, _, _, added| {
                if added > 0 { if let Some(list) = list.upgrade() { list.set_file(gio::File::NONE); } }
            });
            for i in 0..64 { std::fs::write(root.join(format!("leave-{i:03}")), "new").unwrap(); }
            wait_for(|| source.file().is_none()).await;
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            assert_eq!(contents.items.n_items(), 0, "the rest of an obsolete batch must not repopulate the folder");
            contents.items.disconnect(handler);
            contents.stop();
        })).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires an isolated graphical session"]
    fn closing_directory_cancels_pending_metadata_without_retaining_models() {
        gtk::init().unwrap();
        let root = std::env::temp_dir().join(format!("filescope-monitor-close-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..64 { std::fs::write(root.join(i.to_string()), "keep").unwrap(); }
        let context = glib::MainContext::default();
        context.with_thread_default(|| context.block_on(async {
            for _ in 0..30 {
                let source = gtk::DirectoryList::builder().attributes(super::super::ATTRS).monitored(false).build();
                let contents = Contents::new(&source);
                source.set_file(Some(&gio::File::for_path(&root)));
                wait_for(|| !source.is_loading()).await;
                for i in 0..64 { contents.query(&gio::File::for_path(root.join(i.to_string()))); }
                let requests: Vec<_> = contents.requests.borrow().values().cloned().collect();
                assert_eq!(requests.len(), 64);
                let weak = Rc::downgrade(&contents);
                let weak_source = source.downgrade();
                let weak_items = contents.items.downgrade();
                drop(contents);
                drop(source);
                assert!(requests.iter().all(|request| request.is_cancelled()));
                assert!(weak.upgrade().is_none());
                assert!(weak_source.upgrade().is_none());
                assert!(weak_items.upgrade().is_none());
                // Process completions after every referenced model has been released.
                glib::timeout_future(std::time::Duration::from_millis(30)).await;

                let source = gtk::DirectoryList::builder().attributes(super::super::ATTRS).monitored(false).build();
                let contents = Contents::new(&source);
                source.set_file(Some(&gio::File::for_path(&root)));
                assert!(source.is_loading());
                let weak_source = source.downgrade();
                drop(contents);
                drop(source);
                assert!(weak_source.upgrade().is_none());
                glib::timeout_future(std::time::Duration::from_millis(30)).await;
            }
        })).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
