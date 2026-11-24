// Copyright (c) 2024-2026 Sophie Herold
// Copyright (c) 2025 Abhinandan Thakur
// Copyright (c) 2026 Maximiliano Sandoval
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::cell::{Cell, OnceCell, RefCell};
use std::sync::Arc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glycin::Operations;

use super::LpImage;
use super::edit::LpEditCrop;
use crate::deps::*;
use crate::editing::adjust::{Adjustments, Property};
use crate::editing::geometry::{self, Geometry};

/// Slider whose changes in a row form one undo step
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slider {
    Adjust(Property),
    Geometry(geometry::Property),
}
use crate::editing::markup::Shape;
use crate::editing::render::PixelEdits;
use crate::util::ErrorType;
use crate::util::gettext::*;
use crate::util::root::ParentWindow;

/// Path of the temporary file used while replacing `path`
///
/// `n` is 0 for `.name.tmp.ext`, otherwise `.name.tmpN.ext`. The file is hidden,
/// so the viewer doesn't show it as a new image while saving.
fn tmp_path(path: &std::path::Path, n: u32) -> Option<std::path::PathBuf> {
    let mut name = std::ffi::OsString::from(".");
    name.push(path.file_stem()?);
    name.push(".tmp");
    if n > 0 {
        name.push(n.to_string());
    }
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    Some(path.with_file_name(name))
}

/// Keeps [`imp::LpEditWindow::begin_save`] marked as running while it lives
struct SaveGuard<'a>(&'a Cell<bool>);

impl Drop for SaveGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Whether `path` is inside the document portal's FUSE mount
fn is_document_portal_path(path: &std::path::Path) -> bool {
    use std::ffi::OsStr;
    use std::path::Component;

    let mut components = path.components();
    components.next() == Some(Component::RootDir)
        && components.next() == Some(Component::Normal(OsStr::new("run")))
        && components.next() == Some(Component::Normal(OsStr::new("user")))
        && matches!(components.next(), Some(Component::Normal(_)))
        && components.next() == Some(Component::Normal(OsStr::new("doc")))
}

/// File name of `path` for messages
fn display_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Error message without the file paths GIO puts into them
fn path_free_message(message: &str, paths: &[&std::path::Path]) -> String {
    let mut message = message.to_owned();
    for path in paths {
        message = message.replace(path.to_string_lossy().as_ref(), &display_name(path));
    }
    // The reason is last, after the part naming the files
    message
        .rsplit_once(": ")
        .map_or(message.as_str(), |(_, reason)| reason)
        .to_owned()
}

/// Writes `data` to a new temporary file next to `original`
///
/// The file is created exclusively with private permissions, so existing files and
/// symlinks are never touched, and synced to disk. Afterwards it gets the metadata
/// (permissions, owner, …) of `original`, which has to be a resolved path and not a
/// symlink. Meant to run in a blocking thread.
fn write_tmp_file(original: &std::path::Path, data: &[u8]) -> std::io::Result<std::path::PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    for n in 0..100 {
        let tmp = tmp_path(original, n).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name")
        })?;

        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        };

        // Everything after creating is done on the handle, which stays writable even if
        // the original is read-only
        let result = file
            .write_all(data)
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                let copied = gio::File::for_path(original).copy_attributes(
                    &gio::File::for_path(&tmp),
                    gio::FileCopyFlags::ALL_METADATA,
                    gio::Cancellable::NONE,
                );
                if let Err(err) = copied {
                    tracing::warn!("Failed to copy file attributes: {err}");
                    // At least keep the permissions
                    if let Ok(metadata) = std::fs::metadata(original) {
                        file.set_permissions(std::fs::Permissions::from_mode(
                            metadata.permissions().mode() & 0o7777,
                        ))?;
                    }
                }
                // The copied modification time belongs to the old content
                file.set_modified(std::time::SystemTime::now())
            });
        drop(file);

        return match result {
            Ok(()) => Ok(tmp),
            Err(err) => {
                let _ = std::fs::remove_file(&tmp);
                Err(err)
            }
        };
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "no free temporary file name",
    ))
}

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate, glib::Properties)]
    #[properties(wrapper_type = super::LpEditWindow)]
    #[template(file = "edit_window.ui")]
    pub struct LpEditWindow {
        #[template_child]
        toolbar_view: TemplateChild<adw::ToolbarView>,
        #[template_child]
        cancel: TemplateChild<gtk::Button>,
        #[template_child]
        pub(crate) save: TemplateChild<gtk::MenuButton>,
        #[template_child]
        saving_revealer: TemplateChild<gtk::Revealer>,
        #[template_child]
        saving_status: TemplateChild<gtk::Label>,
        #[template_child]
        saving_info: TemplateChild<gtk::Revealer>,
        #[template_child]
        shortcut_controller: TemplateChild<gtk::ShortcutController>,
        #[template_child]
        header_bar: TemplateChild<adw::HeaderBar>,
        #[template_child]
        mode_switcher: TemplateChild<adw::ToggleGroup>,
        #[template_child]
        compare: TemplateChild<gtk::Button>,
        #[template_child]
        save_overwrite: TemplateChild<gtk::ListBoxRow>,

        #[property(get, construct_only)]
        original_image: OnceCell<LpImage>,
        #[property(get, construct_only)]
        pub edit_crop: OnceCell<LpEditCrop>,
        #[property(get, construct_only)]
        pub has_unsaved_changes: Cell<bool>,

        /// Whether the original file can be trashed
        pub(super) can_trash: Cell<bool>,

        pub(super) operations: RefCell<Option<Arc<Operations>>>,
        pub(super) geometry: Cell<Geometry>,
        pub(super) adjustments: Cell<Adjustments>,
        pub(super) markup: RefCell<Arc<Vec<Shape>>>,
        pub(super) resize: Cell<Option<f64>>,

        /// States before each change, for undo
        pub(super) history: RefCell<Vec<EditState>>,
        /// States undone, for redo
        pub(super) future: RefCell<Vec<EditState>>,
        /// Slider whose changes are merged into the last undo step
        pub(super) coalesce: Cell<Option<Slider>>,
        /// Redo steps dropped by the last change, for `discard_last_change`
        pub(super) dropped_future: RefCell<Vec<EditState>>,
        save_cancellable: RefCell<Option<gio::Cancellable>>,
        /// A save is running, further saves and closing are ignored
        pub(super) save_in_progress: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpEditWindow {
        const NAME: &'static str = "LpEditWindow";
        type Type = super::LpEditWindow;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            crate::widgets::edit::LpFrost::static_type();
            Self::bind_template(klass);

            klass.install_action("edit.save-copy", None, |obj, _, _| {
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    obj,
                    async move { obj.imp().save_copy().await }
                ));
            });

            klass.install_action("edit.save-overwrite", None, |obj, _, _| {
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    obj,
                    async move {
                        let _ = obj.imp().save_overwrite().await;
                    }
                ));
            });

            klass.install_action("edit.copy", None, |obj, _, _| {
                glib::spawn_future_local(glib::clone!(
                    #[weak]
                    obj,
                    async move { obj.imp().copy_edited().await }
                ));
            });

            klass.install_action("edit.undo", None, |obj, _, _| obj.undo());
            klass.install_action("edit.redo", None, |obj, _, _| obj.redo());

            klass.install_action("edit.cancel", None, move |obj, _, _| {
                obj.imp().cancel();
            });

            // Escape closes an open pill (aspect ratios, markup options or
            // colours, colour picking) before it closes the editor
            klass.install_action("edit.escape", None, move |obj, _, _| {
                let imp = obj.imp();
                if !imp
                    .edit_crop
                    .get()
                    .is_some_and(|crop| crop.close_transient())
                {
                    imp.cancel();
                }
            });
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    #[glib::derived_properties]
    impl ObjectImpl for LpEditWindow {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();

            self.mode_switcher
                .bind_property("active-name", &obj.edit_crop(), "mode")
                .bidirectional()
                .sync_create()
                .build();

            // Compare only makes sense for adjustments
            obj.edit_crop().connect_mode_notify(glib::clone!(
                #[weak]
                obj,
                move |edit_crop| {
                    obj.imp().compare.set_visible(edit_crop.mode() == "adjust");
                    // Each mode has its own pills, once they have switched
                    glib::idle_add_local_once(glib::clone!(
                        #[weak]
                        obj,
                        move || obj.imp().update_window_min_size()
                    ));
                }
            ));
            self.compare.set_visible(false);

            // Formats that would lose quality are only saved as a PNG copy
            self.save_overwrite
                .set_visible(!obj.original_image().copy_only());

            // Holding the button shows the picture without adjustments
            let hold = gtk::GestureClick::new();
            hold.connect_pressed(glib::clone!(
                #[weak]
                obj,
                move |_, _, _, _| obj.show_original(true)
            ));
            hold.connect_released(glib::clone!(
                #[weak]
                obj,
                move |_, _, _, _| obj.show_original(false)
            ));
            hold.connect_cancel(glib::clone!(
                #[weak]
                obj,
                move |_, _| obj.show_original(false)
            ));
            self.compare.add_controller(hold);

            glib::spawn_future_local(glib::clone!(
                #[weak]
                obj,
                async move {
                    let can_trash = obj.original_image().can_trash().await;
                    obj.imp().can_trash.replace(can_trash);
                    obj.imp().update_save_state(false);
                }
            ));

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::S,
                    gdk::ModifierType::CONTROL_MASK,
                )),
                Some(gtk::NamedAction::new("edit.save-overwrite")),
            ));

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::S,
                    gdk::ModifierType::CONTROL_MASK.union(gdk::ModifierType::SHIFT_MASK),
                )),
                Some(gtk::NamedAction::new("edit.save-copy")),
            ));

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::Z,
                    gdk::ModifierType::CONTROL_MASK,
                )),
                Some(gtk::NamedAction::new("edit.undo")),
            ));

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::Z,
                    gdk::ModifierType::CONTROL_MASK.union(gdk::ModifierType::SHIFT_MASK),
                )),
                Some(gtk::NamedAction::new("edit.redo")),
            ));

            obj.update_history_actions();

            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::Escape,
                    gdk::ModifierType::NO_MODIFIER_MASK,
                )),
                Some(gtk::NamedAction::new("edit.escape")),
            ));

            // Ctrl + and Ctrl − zoom, Ctrl 0 goes back to best fit
            for (keys, direction) in [
                (&[gdk::Key::plus, gdk::Key::equal, gdk::Key::KP_Add][..], 1),
                (&[gdk::Key::minus, gdk::Key::KP_Subtract][..], -1),
                (&[gdk::Key::_0, gdk::Key::KP_0][..], 0),
            ] {
                for key in keys {
                    self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                        Some(gtk::KeyvalTrigger::new(
                            *key,
                            gdk::ModifierType::CONTROL_MASK,
                        )),
                        Some(gtk::CallbackAction::new(glib::clone!(
                            #[weak]
                            obj,
                            #[upgrade_or]
                            glib::Propagation::Proceed,
                            move |_, _| {
                                obj.edit_crop().zoom_step(direction);
                                glib::Propagation::Stop
                            }
                        ))),
                    ));
                }
            }

            // Copies the edited image, unless text is being typed
            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::c,
                    gdk::ModifierType::CONTROL_MASK,
                )),
                Some(gtk::CallbackAction::new(glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    glib::Propagation::Proceed,
                    move |_, _| {
                        if obj.dialog_open() || obj.edit_crop().markup_canvas().is_typing() {
                            return glib::Propagation::Proceed;
                        }
                        let _ = obj.activate_action("edit.copy", None);
                        glib::Propagation::Stop
                    }
                ))),
            ));

            // Single keys for views and tools (A select, B brush, C crop);
            // text fields and typed text
            // take the keys first
            self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(
                    gdk::Key::c,
                    gdk::ModifierType::NO_MODIFIER_MASK,
                )),
                Some(gtk::CallbackAction::new(glib::clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    glib::Propagation::Proceed,
                    move |_, _| {
                        if obj.dialog_open() {
                            return glib::Propagation::Proceed;
                        }
                        obj.edit_crop().set_mode("crop");
                        glib::Propagation::Stop
                    }
                ))),
            ));
            // Markup tools, with the keys image editors commonly use
            let no_modifier = gdk::ModifierType::NO_MODIFIER_MASK;
            for (key, modifiers, tool) in [
                (gdk::Key::a, no_modifier, "select"),
                (gdk::Key::b, no_modifier, "brush"),
                (gdk::Key::l, no_modifier, "line"),
                (gdk::Key::L, gdk::ModifierType::SHIFT_MASK, "arrow"),
                (gdk::Key::r, no_modifier, "rectangle"),
                (gdk::Key::o, no_modifier, "ellipse"),
                (gdk::Key::t, no_modifier, "text"),
                (gdk::Key::e, no_modifier, "eraser"),
            ] {
                self.shortcut_controller.add_shortcut(gtk::Shortcut::new(
                    Some(gtk::KeyvalTrigger::new(key, modifiers)),
                    Some(gtk::CallbackAction::new(glib::clone!(
                        #[weak]
                        obj,
                        #[upgrade_or]
                        glib::Propagation::Proceed,
                        move |_, _| {
                            let edit_crop = obj.edit_crop();
                            // Letters typed onto the picture stay text
                            if obj.dialog_open() || edit_crop.markup_canvas().is_typing() {
                                return glib::Propagation::Proceed;
                            }
                            edit_crop.set_mode("markup");
                            let _ = edit_crop
                                .activate_action("edit-markup.tool", Some(&tool.to_variant()));
                            glib::Propagation::Stop
                        }
                    ))),
                ));
            }
        }
    }

    impl WidgetImpl for LpEditWindow {
        fn root(&self) {
            self.parent_root();

            let obj = self.obj();

            obj.edit_crop()
                .selection()
                .connect_cropped_notify(glib::clone!(
                    #[weak]
                    obj,
                    move |_| {
                        obj.imp().update_save_state(false);
                        obj.update_history_actions();
                    }
                ));

            self.update_save_state(false);
            glib::idle_add_local_once(glib::clone!(
                #[weak]
                obj,
                move || obj.imp().update_window_min_size()
            ));
        }
    }
    impl BinImpl for LpEditWindow {}

    impl LpEditWindow {
        /// Windows with breakpoints ignore how much room their content
        /// needs, so the window is given the editor's minimum size: all
        /// header buttons, which the header bar would cut off, and all pills
        fn update_window_min_size(&self) {
            let obj = self.obj();
            let Some(window) = obj.root().and_downcast::<gtk::Window>() else {
                return;
            };
            let (_, header_width, _, _) = self.header_bar.measure(gtk::Orientation::Horizontal, -1);
            let (_, header_height, _, _) = self.header_bar.measure(gtk::Orientation::Vertical, -1);
            let (crop_width, crop_height) = obj.edit_crop().min_size();
            let width = header_width.max(crop_width);
            let height = header_height + crop_height;
            window.set_size_request(
                width.max(crate::widgets::window::MIN_WIDTH),
                height.max(crate::widgets::window::MIN_HEIGHT),
            );
        }

        /// Close editing view
        fn cancel(&self) {
            // Closing while saving could start a second save
            if self.save_in_progress.get() {
                return;
            }

            let obj = self.obj().to_owned();

            if let Some(cancellable) = obj.imp().save_cancellable.replace(None) {
                cancellable.cancel();
            }

            glib::spawn_future_local(async move {
                if obj.ensure_saved().await == glib::Propagation::Proceed {
                    obj.window_inspect(|w| w.show_image());
                }
            });
        }

        /// Marks a save as running until the guard is dropped
        pub(super) fn begin_save(&self) -> Option<SaveGuard<'_>> {
            if self.save_in_progress.replace(true) {
                None
            } else {
                Some(SaveGuard(&self.save_in_progress))
            }
        }

        pub(super) async fn save_copy(&self) {
            let obj = self.obj();
            self.save.popdown();

            let Some(_guard) = self.begin_save() else {
                return;
            };

            if let Some(current_file) = obj.original_image().file() {
                let cancellable = gio::Cancellable::new();
                if let Some(old_cancellable) =
                    self.save_cancellable.replace(Some(cancellable.clone()))
                {
                    old_cancellable.cancel();
                }

                let file_dialog = gtk::FileDialog::new();

                let suggested_file = if let Some(path) = current_file.path() {
                    if let Some(basename) = path.file_stem() {
                        let mut suggested_file = None;
                        for i in 1..20 {
                            let mut new_path = path.clone();
                            let mut new_filename = basename.to_os_string();

                            // Translators: Filename suffix, {} is replaced by a number
                            new_filename.push(ngettext(" (Edited)", " (Edited {})", i));

                            if obj.original_image().copy_only() {
                                new_filename.push(".png");
                            } else if let Some(ext) = path.extension() {
                                new_filename.push(".");
                                new_filename.push(ext);
                            }

                            new_path.set_file_name(new_filename);

                            let file = gio::File::for_path(&new_path);
                            let file_exists = file
                                .query_info_future(
                                    gio::FILE_ATTRIBUTE_STANDARD_NAME,
                                    gio::FileQueryInfoFlags::NONE,
                                    glib::Priority::DEFAULT,
                                )
                                .await
                                .is_ok();

                            if !file_exists {
                                suggested_file = Some(file);
                                break;
                            }
                        }

                        suggested_file.unwrap_or_else(|| current_file.clone())
                    } else {
                        current_file.clone()
                    }
                } else {
                    current_file.clone()
                };

                file_dialog.set_initial_file(Some(&suggested_file));

                match file_dialog.save_future(obj.try_window().as_ref()).await {
                    Err(err) => {
                        tracing::error!("{}", err);
                    }
                    Ok(new_file) => {
                        if self
                            .save(current_file, new_file.clone(), cancellable.clone())
                            .await
                        {
                            self.has_unsaved_changes.replace(false);
                            obj.window_inspect(|w| w.show_specific_image(new_file));
                        }
                    }
                }
            }
        }

        /// Puts the edited image on the clipboard without saving a file
        pub(super) async fn copy_edited(&self) {
            let obj = self.obj();
            self.save.popdown();

            let Some(_guard) = self.begin_save() else {
                return;
            };
            let Some(current_file) = obj.original_image().file() else {
                return;
            };

            let cancellable = gio::Cancellable::new();
            if let Some(old_cancellable) = self.save_cancellable.replace(Some(cancellable.clone()))
            {
                old_cancellable.cancel();
            }

            let Some(data) = self.render_edited(current_file, cancellable).await else {
                return;
            };

            let texture = match glycin::Loader::new_vec(data).load().await {
                Ok(mut image) => image.next_frame().await.map(|frame| frame.texture()),
                Err(err) => Err(err),
            };
            self.set_saving_status(None);

            match texture {
                Ok(texture) => {
                    obj.clipboard().set_texture(&texture);
                    obj.window_show_toast(
                        // Translators: This is a toast notification
                        &gettext("Image copied."),
                        adw::ToastPriority::Normal,
                    );
                }
                Err(err) => {
                    tracing::warn!("Failed to copy edited image: {err}");
                    obj.window_show_toast(
                        &gettext("Could not copy image."),
                        adw::ToastPriority::High,
                    );
                }
            }
        }

        pub(super) async fn save_overwrite(&self) -> Result<(), ()> {
            let obj = self.obj();
            self.save.popdown();

            if obj.original_image().copy_only() {
                return Err(());
            }

            let Some(_guard) = self.begin_save() else {
                return Err(());
            };

            let Some(opened_file) = obj.original_image().file() else {
                self.show_save_error("The image has no file that could be replaced.");
                return Err(());
            };
            let Some(opened_path) = opened_file.path() else {
                self.show_save_error("Only local files can be replaced.");
                return Err(());
            };
            // Follow symlinks, the real file is replaced and not the link
            let current_path = match std::fs::canonicalize(&opened_path) {
                Ok(path) => path,
                Err(err) => {
                    self.show_save_error(&format!("Failed to resolve the image file: {err}"));
                    return Err(());
                }
            };
            let original_file = gio::File::for_path(&current_path);
            let name = super::display_name(&current_path);

            let cancellable = gio::Cancellable::new();
            if let Some(old_cancellable) = self.save_cancellable.replace(Some(cancellable.clone()))
            {
                old_cancellable.cancel();
            }

            tracing::debug!("Saving changes …");
            let Some(data) = self
                .render_edited(opened_file.clone(), cancellable.clone())
                .await
            else {
                // Nothing was written, so the edits are still only here
                return Err(());
            };

            // The edited bytes are written, synced and given the permissions of the original
            // before the original is touched
            self.set_saving_status(Some(gettext("Saving Image")));
            let write_path = current_path.clone();
            let tmp_file = match gio::spawn_blocking(move || {
                super::write_tmp_file(&write_path, &data)
            })
            .await
            {
                Ok(Ok(tmp_path)) => gio::File::for_path(tmp_path),
                Ok(Err(err)) => {
                    self.show_save_error(&format!(
                        "Failed to write edited image for {name}: {err}"
                    ));
                    return Err(());
                }
                Err(_) => {
                    self.show_save_error("Writing the edited image panicked.");
                    return Err(());
                }
            };
            tracing::debug!("Image written to '{}'", tmp_file.uri());

            if cancellable.is_cancelled() {
                // Canceled before the original was touched
                tracing::debug!("Saving canceled");
                self.set_saving_status(None);
                Self::remove_tmp_file(&tmp_file).await;
                return Err(());
            }

            // The trash portal can't be used for files shared via the document portal
            let trash_failed = if super::is_document_portal_path(&current_path) {
                tracing::debug!("Not trashing document portal file {current_path:?}");
                true
            } else {
                tracing::debug!("Moving image to trash '{}'", original_file.uri());
                self.set_saving_status(Some(gettext("Moving Original to Trash")));
                // The result is awaited without a cancellable since a trash operation
                // that has been started can't be told apart from a finished one otherwise
                match original_file.trash_future(glib::Priority::DEFAULT).await {
                    Ok(()) => false,
                    Err(err) => {
                        tracing::warn!("Failed to move {current_path:?} to trash: {err}");
                        true
                    }
                }
            };

            // Flags for moving the temporary file into place
            let mut move_flags = gio::FileCopyFlags::NONE;

            if trash_failed {
                // The user asked to replace the original, so it is overwritten without a copy
                // in the trash
                if cancellable.is_cancelled() {
                    self.set_saving_status(None);
                    Self::remove_tmp_file(&tmp_file).await;
                    return Err(());
                }
                tracing::debug!("Replacing original without trashing it");
                // Same directory, so this is an atomic rename
                move_flags = gio::FileCopyFlags::OVERWRITE;
            } else if cancellable.is_cancelled() {
                tracing::debug!("Canceled too late, original is already in the trash");
            }

            tracing::debug!("Moving '{}' to '{}'", tmp_file.uri(), original_file.uri());
            self.set_saving_status(Some(gettext("Finalizing Save Operation")));
            // Also awaited to the end, the original is gone at this point if it was trashed
            let move_result = tmp_file
                .move_future(&original_file, move_flags, glib::Priority::DEFAULT)
                .0
                .await;

            if let Err(err) = move_result {
                // Without the original the temporary file holds the only edited copy
                let keep_tmp = !trash_failed && Self::is_not_found(&original_file).await;
                let tmp_path = tmp_file.path().unwrap_or_default();
                let tmp_name = super::display_name(&tmp_path);
                let reason = super::path_free_message(
                    err.message(),
                    &[current_path.as_path(), tmp_path.as_path()],
                );
                if keep_tmp {
                    tracing::warn!("Original is gone, keeping '{}'", tmp_file.uri());
                    self.show_save_error(&format!(
                        "Failed to move the edited image to {name}: {reason}\n\nThe original is in the trash. The edited image was kept as {tmp_name} in the same folder."
                    ));
                } else {
                    Self::remove_tmp_file(&tmp_file).await;
                    self.show_save_error(&format!(
                        "Failed to move the edited image to {name}: {reason}"
                    ));
                }
                return Err(());
            }

            self.has_unsaved_changes.replace(false);
            obj.window_inspect(|w| w.show_specific_image(opened_file));
            Ok(())
        }

        /// Shows an error for failed saving
        fn show_save_error(&self, details: &str) {
            self.obj().window_show_error(
                &gettext("Failed to save image."),
                details,
                ErrorType::General,
            );
            self.set_saving_status(None);
        }

        /// Returns `true` only if `file` is known to not exist
        async fn is_not_found(file: &gio::File) -> bool {
            file.query_info_future(
                gio::FILE_ATTRIBUTE_STANDARD_NAME,
                gio::FileQueryInfoFlags::NONE,
                glib::Priority::DEFAULT,
            )
            .await
            .is_err_and(|err| err.matches(gio::IOErrorEnum::NotFound))
        }

        /// Deletes a temporary file, errors are only logged
        async fn remove_tmp_file(file: &gio::File) {
            match file.delete_future(glib::Priority::DEFAULT).await {
                Ok(()) => tracing::debug!("Removed temporary file '{}'", file.uri()),
                Err(err) if err.matches(gio::IOErrorEnum::NotFound) => {}
                Err(err) => tracing::warn!("Failed to remove '{}': {err}", file.uri()),
            }
        }

        #[must_use]
        /// Saves image with operations applies
        ///
        /// Returns `true` if editing and saving was successful
        async fn save(
            &self,
            current_file: gio::File,
            new_file: gio::File,
            cancellable: gio::Cancellable,
        ) -> bool {
            match self.render_edited(current_file, cancellable.clone()).await {
                Some(data) => self.write(new_file, data, cancellable).await,
                None => false,
            }
        }

        /// Applies the edits and returns the encoded image
        ///
        /// Shows errors itself, `None` is returned if there is nothing to save
        async fn render_edited(
            &self,
            current_file: gio::File,
            cancellable: gio::Cancellable,
        ) -> Option<Vec<u8>> {
            let obj = self.obj();

            obj.edit_crop().apply_crop();
            obj.edit_crop().markup_canvas().finish_text();

            self.set_saving_status(Some(gettext("Editing Image")));

            let pixel_edits = obj.pixel_edits();
            let image = obj.original_image();
            // Only JPEG and PNG can be cropped and rotated without decoding,
            // everything else goes through the encoder
            if !pixel_edits.is_empty() || !image.lossless_edit() {
                tracing::debug!("Rendering pixel edits {pixel_edits:?}");
                let operations = obj.operations().map(|x| (*x).clone());
                let target_mime_type = image
                    .copy_only()
                    .then(|| glycin::MimeType::new("image/png".to_string()));
                let result = crate::editing::render::render(
                    current_file,
                    operations,
                    pixel_edits.clone(),
                    target_mime_type,
                    &cancellable,
                )
                .await;

                return match result {
                    Err(err) if cancellable.is_cancelled() => {
                        tracing::debug!("Rendering edited image canceled: {err}");
                        self.set_saving_status(None);
                        None
                    }
                    Err(err) => {
                        tracing::warn!("Failed to render image: {err}");
                        obj.window_show_error(
                            &gettext("Failed to edit image."),
                            &format!("Failed to edit image:\n\n{err}\n\n{pixel_edits:#?}"),
                            ErrorType::Loader,
                        );
                        self.set_saving_status(None);
                        None
                    }
                    Ok(data) => Some(data),
                };
            }

            let mut editor = glycin::Editor::new(current_file);
            editor.cancellable(cancellable.clone());

            if let Some(operations) = obj.operations() {
                tracing::debug!("Computing edited image.");
                let result = match editor.edit().await {
                    Ok(editable_image) => editable_image.apply_complete(&operations).await,
                    Err(err) => Err(err),
                };
                match result {
                    Err(err) if err.is_cancelled() => {
                        tracing::debug!("Computing edited image canceled");
                    }
                    Err(err) => {
                        tracing::warn!("Failed to edit image: {err}");
                        obj.window_show_error(
                            &gettext("Failed to edit image."),
                            &format!("Failed to edit image:\n\n{err}\n\n{operations:#?}"),
                            ErrorType::Loader,
                        )
                    }
                    Ok(edit) => {
                        // TODO: Clone might be avoidable
                        let data = edit.data().to_vec();
                        return Some(data);
                    }
                }
            }

            self.set_saving_status(None);
            None
        }

        /// Writes encoded image data to `new_file`
        ///
        /// Returns `true` if writing was successful
        async fn write(
            &self,
            new_file: gio::File,
            data: Vec<u8>,
            cancellable: gio::Cancellable,
        ) -> bool {
            let obj = self.obj();

            tracing::debug!("Saving edited image to '{}'", new_file.uri());
            self.set_saving_status(Some(gettext("Saving Image")));

            let save_result = gio::CancellableFuture::new(
                new_file.replace_contents_future(data, None, true, gio::FileCreateFlags::NONE),
                cancellable,
            )
            .await;

            if save_result.is_err() {
                tracing::debug!("Saving image canceled");
            } else if let Ok(Err(err)) = save_result {
                obj.window_show_error(
                    &gettext("Failed to Save Image"),
                    &format!("Failed to write file:\n\n{err:?}"),
                    ErrorType::General,
                );
            } else {
                tracing::debug!("Image saved");
                return true;
            }

            self.set_saving_status(None);
            false
        }

        fn set_saving_status(&self, message: Option<String>) {
            let is_saving = message.is_some();
            self.saving_status.set_label(&message.unwrap_or_default());
            self.saving_revealer.set_reveal_child(is_saving);
            self.update_save_state(is_saving);

            if is_saving {
                // Show text and spinner delayed
                glib::timeout_add_local_once(
                    std::time::Duration::from_millis(
                        self.saving_revealer.transition_duration().into(),
                    ),
                    glib::clone!(
                        #[weak(rename_to=imp)]
                        self,
                        move || {
                            imp.saving_info.set_reveal_child(true);
                        }
                    ),
                );
            } else {
                self.saving_info.set_reveal_child(false);
            }
        }

        pub(super) fn is_save_sensitive(&self) -> bool {
            let obj = self.obj();

            self.operations
                .borrow()
                .as_ref()
                .is_some_and(|x| !x.operations().is_empty())
                || !self.geometry.get().is_identity()
                || obj.edit_crop().selection().cropped()
                || !self.adjustments.get().is_identity()
                || !self.markup.borrow().is_empty()
                || obj.edit_crop().markup_canvas().has_typed_text()
                || self.resize.get().is_some()
        }

        pub fn update_save_state(&self, force_disabled: bool) {
            let obj = self.obj();

            if !force_disabled {
                self.has_unsaved_changes.replace(self.is_save_sensitive());
            }

            let enabled = self.is_save_sensitive() && !force_disabled;

            self.save.set_sensitive(enabled);
            obj.action_set_enabled(
                "edit.save-overwrite",
                enabled && self.can_trash.get() && !obj.original_image().copy_only(),
            );
            obj.action_set_enabled("edit.save-copy", enabled);
            obj.action_set_enabled("edit.copy", enabled);
        }
    }
}

/// Everything that undo and redo bring back
#[derive(Debug, Clone)]
pub struct EditState {
    operations: Option<Arc<Operations>>,
    geometry: Geometry,
    adjustments: Adjustments,
    markup: Arc<Vec<Shape>>,
    resize: Option<f64>,
}

glib::wrapper! {
    pub struct LpEditWindow(ObjectSubclass<imp::LpEditWindow>)
    @extends gtk::Widget, adw::Bin,
    @implements gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

impl LpEditWindow {
    pub const REQUIRED_OPERATIONS: &[glycin::OperationId] = &[
        glycin::OperationId::Clip,
        glycin::OperationId::MirrorHorizontally,
        glycin::OperationId::Rotate,
    ];

    pub fn new(image: LpImage) -> Self {
        glib::Object::builder()
            .property("original_image", image.clone())
            .property("edit_crop", LpEditCrop::new(image))
            .build()
    }

    pub fn operations(&self) -> Option<Arc<glycin::Operations>> {
        self.imp().operations.borrow().clone()
    }

    /// Sets the crop, rotate and flip operations as one undo step
    pub fn set_operations(&self, operations: Option<Arc<glycin::Operations>>) {
        if self.operations() != operations {
            self.record(None);
        }
        self.replace_operations(operations);
    }

    fn replace_operations(&self, operations: Option<Arc<glycin::Operations>>) {
        let imp = self.imp();
        imp.operations.replace(operations);
        // Markup, the resized size and the geometry sliders follow the
        // operations
        self.edit_crop().markup_changed();
        self.edit_crop().geometry_changed();
        self.edit_crop().update_size_label();
        imp.update_save_state(false);
    }

    pub fn add_operation(&self, operation: glycin::Operation) {
        let mut operations = self
            .operations()
            .map(|x| x.operations().to_vec())
            .unwrap_or_default();

        operations.push(operation);

        self.set_operations(Some(Arc::new(Operations::new(operations))));
    }

    fn show_original(&self, original: bool) {
        let adjustments = if original {
            Adjustments::default()
        } else {
            self.adjustments()
        };
        self.edit_crop().image().set_adjustments(adjustments);
    }

    pub fn adjustments(&self) -> Adjustments {
        self.imp().adjustments.get()
    }

    /// Changes the colour adjustments
    ///
    /// Changes to the same `property` in a row form one undo step.
    pub fn change_adjustments(&self, adjustments: Adjustments, property: Option<Property>) {
        if adjustments == self.adjustments() {
            return;
        }
        self.record(property.map(Slider::Adjust));
        self.replace_adjustments(adjustments);
    }

    /// Straightening and perspective, for the original orientation
    pub fn geometry(&self) -> Geometry {
        self.imp().geometry.get()
    }

    /// Changes the straightening and perspective
    ///
    /// Changes to the same `property` in a row form one undo step.
    pub fn change_geometry(&self, geometry: Geometry, property: Option<geometry::Property>) {
        if geometry == self.geometry() {
            return;
        }

        // Markup moves with the picture. While a slider moves, it is moved
        // from where the slider started, so small errors don't add up.
        let coalesce = property.map(Slider::Geometry);
        let continuing = coalesce.is_some() && self.imp().coalesce.get() == coalesce;
        let (start_geometry, start_markup) = self
            .imp()
            .history
            .borrow()
            .last()
            .filter(|_| continuing)
            .map(|x| (x.geometry, x.markup.clone()))
            .unwrap_or_else(|| (self.geometry(), self.markup()));

        self.record(coalesce);
        self.replace_geometry(geometry);
        self.move_markup_with_geometry(&start_markup, start_geometry, geometry);
    }

    /// Puts `markup`, drawn on the picture warped by `from`, on the same
    /// spots of the picture warped by `to`
    fn move_markup_with_geometry(&self, markup: &[Shape], from: Geometry, to: Geometry) {
        if markup.is_empty() && self.markup().is_empty() {
            return;
        }
        let (width, height) = self.original_image().image_size();
        let moved = geometry::follow(markup, from, to, (width as f64, height as f64));
        self.set_markup_list(Arc::new(moved));
    }

    fn replace_geometry(&self, geometry: Geometry) {
        let imp = self.imp();
        imp.geometry.set(geometry);
        self.edit_crop().image().set_geometry(geometry);
        self.edit_crop().geometry_changed();
        imp.update_save_state(false);
    }

    fn replace_adjustments(&self, adjustments: Adjustments) {
        let imp = self.imp();
        imp.adjustments.set(adjustments);
        self.edit_crop().image().set_adjustments(adjustments);
        imp.update_save_state(false);
    }

    /// Whether a dialog like Resize covers the editor
    fn dialog_open(&self) -> bool {
        self.root()
            .and_downcast::<adw::ApplicationWindow>()
            .is_some_and(|window| window.visible_dialog().is_some())
    }

    pub fn markup(&self) -> Arc<Vec<Shape>> {
        self.imp().markup.borrow().clone()
    }

    pub fn add_markup(&self, shape: Shape) {
        self.record(None);
        let mut markup = (*self.markup()).clone();
        markup.push(shape);
        self.set_markup_list(Arc::new(markup));
    }

    /// Replaces the shape at `index`, as a new undo step if `record` is set
    pub fn replace_markup(&self, index: usize, shape: Shape, record: bool) {
        if record {
            self.record(None);
        }
        let mut markup = (*self.markup()).clone();
        if let Some(old) = markup.get_mut(index) {
            *old = shape;
            self.set_markup_list(Arc::new(markup));
        }
    }

    pub fn remove_markup(&self, index: usize) {
        let mut markup = (*self.markup()).clone();
        if index < markup.len() {
            self.record(None);
            markup.remove(index);
            self.edit_crop().markup_canvas().clear_selection();
            self.set_markup_list(Arc::new(markup));
        }
    }

    fn set_markup_list(&self, markup: Arc<Vec<Shape>>) {
        let imp = self.imp();
        imp.markup.replace(markup);
        self.edit_crop().markup_changed();
        imp.update_save_state(false);
    }

    /// Edits that are applied to the decoded pixels on save
    pub fn pixel_edits(&self) -> PixelEdits {
        PixelEdits {
            geometry: self.geometry(),
            adjustments: self.adjustments(),
            markup: self.markup(),
            resize: self.resize(),
        }
    }

    /// Factor the image is scaled down by on save
    pub fn resize(&self) -> Option<f64> {
        self.imp().resize.get()
    }

    pub fn set_resize(&self, resize: Option<f64>) {
        // Only a resize that changes the size counts
        let (width, height) = self.edit_crop().image().image_size();
        let size = (width.max(1) as u32, height.max(1) as u32);
        let resize = resize.filter(|x| crate::editing::render::scaled_size(size, *x) != size);
        if resize != self.resize() {
            self.record(None);
            self.replace_resize(resize);
        }
    }

    fn replace_resize(&self, resize: Option<f64>) {
        let imp = self.imp();
        imp.resize.set(resize);
        self.edit_crop().update_size_label();
        imp.update_save_state(false);
    }

    /// Undoes crop, rotate, flip, straightening and resize as one step
    pub fn reset_crop_view(&self) {
        if self.operations().is_some() || self.resize().is_some() || !self.geometry().is_identity()
        {
            self.record(None);
        }
        self.replace_operations(None);
        let (geometry, markup) = (self.geometry(), self.markup());
        self.replace_geometry(Geometry::default());
        self.move_markup_with_geometry(&markup, geometry, Geometry::default());
        self.replace_resize(None);
    }

    fn state(&self) -> EditState {
        EditState {
            operations: self.operations(),
            geometry: self.geometry(),
            adjustments: self.adjustments(),
            markup: self.markup(),
            resize: self.resize(),
        }
    }

    /// Remembers the current state before a change
    fn record(&self, coalesce: Option<Slider>) {
        let imp = self.imp();

        if coalesce.is_some() && imp.coalesce.get() == coalesce {
            return;
        }
        imp.coalesce.set(coalesce);

        imp.history.borrow_mut().push(self.state());
        let future = imp.future.take();
        imp.dropped_future.replace(future);
        self.update_history_actions();
    }

    /// Starts a new undo step for the next slider change
    pub fn end_coalescing(&self) {
        self.imp().coalesce.set(None);
    }

    fn restore(&self, state: EditState) {
        let imp = self.imp();
        imp.coalesce.set(None);

        let edit_crop = self.edit_crop();
        edit_crop.selection().reset();

        if let Err(err) = edit_crop.image().set_operations(state.operations.clone()) {
            tracing::error!("Failed to restore operations: {err}");
        }
        self.replace_operations(state.operations);
        self.replace_geometry(state.geometry);
        self.replace_adjustments(state.adjustments);
        self.set_markup_list(state.markup);
        self.replace_resize(state.resize);

        edit_crop.state_restored();
        edit_crop.markup_canvas().clear_selection();
        self.update_history_actions();
    }

    /// Updates Save after a change that is not an undo step yet
    pub fn refresh_save_state(&self) {
        self.imp().update_save_state(false);
    }

    pub fn undo(&self) {
        // Typed text becomes a shape, and undo stops there, since text
        // that stayed the same adds no step to undo
        let markup_canvas = self.edit_crop().markup_canvas();
        if markup_canvas.is_typing() {
            markup_canvas.finish_text();
            return;
        }

        // A pending crop selection is undone first
        let selection = self.edit_crop().selection();
        if self.edit_crop().mode() == "crop" && selection.cropped() {
            selection.reset();
            return;
        }

        let previous = self.imp().history.borrow_mut().pop();
        if let Some(previous) = previous {
            self.imp().future.borrow_mut().push(self.state());
            self.restore(previous);
        }
    }

    pub fn redo(&self) {
        // Adding the typed text starts a new branch, with nothing to redo
        let markup_canvas = self.edit_crop().markup_canvas();
        if markup_canvas.is_typing() {
            markup_canvas.finish_text();
            return;
        }
        let next = self.imp().future.borrow_mut().pop();
        if let Some(next) = next {
            self.imp().history.borrow_mut().push(self.state());
            self.restore(next);
        }
    }

    /// Drops the last change without keeping it for redo
    pub fn discard_last_change(&self) {
        let imp = self.imp();
        let previous = imp.history.borrow_mut().pop();
        if let Some(previous) = previous {
            self.restore(previous);
            let future = imp.dropped_future.take();
            imp.future.replace(future);
            self.update_history_actions();
        }
    }

    fn update_history_actions(&self) {
        let imp = self.imp();
        let pending_crop = imp
            .edit_crop
            .get()
            .is_some_and(|x| x.mode() == "crop" && x.selection().cropped());
        self.action_set_enabled(
            "edit.undo",
            pending_crop || !imp.history.borrow().is_empty(),
        );
        self.action_set_enabled("edit.redo", !imp.future.borrow().is_empty());
    }

    /// Returns if the window should proceed with closing
    pub async fn ensure_saved(&self) -> glib::Propagation {
        // Closing while saving could start a second save
        if self.imp().save_in_progress.get() {
            return glib::Propagation::Stop;
        }
        self.edit_crop().markup_canvas().finish_text();
        if self.has_unsaved_changes() {
            let dialog = adw::AlertDialog::new(
                Some(&gettext("Save Changes?")),
                Some(&gettext(
                    "There are unsaved image editing changes. Closing without saving will permanently lose these modifications.",
                )),
            );

            dialog.add_response("cancel", &gettext("Cancel"));
            dialog.add_response("discard", &gettext("Discard"));
            dialog.add_response("save", &gettext("Save"));

            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);

            let res = dialog.choose_future(Some(self)).await;

            match res.as_str() {
                "cancel" => glib::Propagation::Stop,
                "close" => glib::Propagation::Stop,
                "discard" => {
                    self.imp().has_unsaved_changes.replace(false);
                    glib::Propagation::Proceed
                }
                // Replacing needs the original in the trash
                "save" if !self.imp().can_trash.get() => {
                    self.imp().save_copy().await;
                    if self.has_unsaved_changes() {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                "save" => {
                    if self.imp().save_overwrite().await.is_ok() {
                        glib::Propagation::Proceed
                    } else {
                        glib::Propagation::Stop
                    }
                }
                res => {
                    tracing::error!("Invalid dialog result: {res}");
                    glib::Propagation::Stop
                }
            }
        } else {
            glib::Propagation::Proceed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        display_name, is_document_portal_path, path_free_message, tmp_path, write_tmp_file,
    };

    #[test]
    fn tmp_path_names() {
        let p = Path::new("/a/b/photo.png");
        assert_eq!(tmp_path(p, 0).unwrap(), Path::new("/a/b/.photo.tmp.png"));
        assert_eq!(tmp_path(p, 2).unwrap(), Path::new("/a/b/.photo.tmp2.png"));
        assert_eq!(
            tmp_path(Path::new("/a/noext"), 0).unwrap(),
            Path::new("/a/.noext.tmp")
        );
        assert!(tmp_path(Path::new("/"), 0).is_none());
    }

    #[test]
    fn document_portal_paths() {
        assert!(is_document_portal_path(Path::new(
            "/run/user/1000/doc/abc/x.png"
        )));
        assert!(!is_document_portal_path(Path::new(
            "/run/user/1000/other/x.png"
        )));
        assert!(!is_document_portal_path(Path::new("/home/u/doc/x.png")));
        assert!(!is_document_portal_path(Path::new(
            "run/user/1000/doc/x.png"
        )));
    }

    #[test]
    fn display_names() {
        assert_eq!(display_name(Path::new("/a/b/photo.png")), "photo.png");
        assert_eq!(display_name(Path::new("/")), "");
    }

    #[test]
    fn write_tmp_file_is_exclusive_and_keeps_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("blick-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("photo.png");
        std::fs::write(&original, b"old").unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o640)).unwrap();
        // Occupied names, one of them a dangling symlink
        std::fs::write(dir.join(".photo.tmp.png"), b"user data").unwrap();
        std::os::unix::fs::symlink(dir.join("missing"), dir.join(".photo.tmp1.png")).unwrap();

        let tmp = write_tmp_file(&original, b"new").unwrap();

        assert_eq!(tmp, dir.join(".photo.tmp2.png"));
        assert_eq!(std::fs::read(&tmp).unwrap(), b"new");
        assert_eq!(
            std::fs::read(dir.join(".photo.tmp.png")).unwrap(),
            b"user data"
        );
        assert!(!dir.join("missing").exists());
        assert_eq!(
            std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(std::fs::read(&original).unwrap(), b"old");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn path_free_messages() {
        let orig = Path::new("/home/u/pics/photo.png");
        let tmp = Path::new("/home/u/pics/.photo.tmp.png");
        assert_eq!(
            path_free_message(
                "Error renaming file /home/u/pics/.photo.tmp.png: Permission denied",
                &[orig, tmp]
            ),
            "Permission denied"
        );
        assert_eq!(
            path_free_message("Error moving /home/u/pics/photo.png", &[orig, tmp]),
            "Error moving photo.png"
        );
    }

    #[test]
    fn write_tmp_file_read_only_original() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("blick-test-ro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("photo.png");
        std::fs::write(&original, b"old").unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o444)).unwrap();

        let tmp = write_tmp_file(&original, b"new").unwrap();

        assert_eq!(std::fs::read(&tmp).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777,
            0o444
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
