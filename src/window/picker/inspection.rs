//! Adapt the shared preview pane without importing the main window's actions.
use super::*;

pub(super) fn update(picker: &Picker) {
    let selected = picker.selection.selection();
    let target = (selected.size() == 1)
        .then(|| picker.selection.item(selected.nth(0)))
        .flatten()
        .and_downcast::<gio::FileInfo>()
        .and_then(|info| {
            info.attribute_object("standard::file")
                .and_downcast::<gio::File>()
                .map(|file| (file, info))
        });
    picker.preview.set_target(target);
}

pub(super) fn connect(picker: &Rc<Picker>) {
    let weak = Rc::downgrade(picker);
    picker.preview_button.connect_toggled(move |button| {
        if let Some(picker) = weak.upgrade() {
            picker.preview.set_visible(button.is_active());
            update(&picker);
        }
    });
    let action = gio::SimpleAction::new("preview-pane", None);
    let weak = Rc::downgrade(picker);
    action.connect_activate(move |_, _| {
        if let Some(picker) = weak.upgrade() {
            picker
                .preview_button
                .set_active(!picker.preview_button.is_active());
        }
    });
    let actions = gio::SimpleActionGroup::new();
    actions.add_action(&action);
    if let Some(window) = picker.window.upgrade() {
        window.insert_action_group("win", Some(&actions));
    }
}
