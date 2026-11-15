// Copyright (c) 2026 Replexo
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

//! Several images opened together: thumbnail strip and actions for the set

use super::*;
use crate::util::ErrorType;

pub fn init_actions(klass: &mut <imp::LpImageWindow as ObjectSubclass>::Class) {
    klass.install_action_async("win.move-set", None, |win, _, _| async move {
        win.transfer_set(true).await;
    });
    klass.install_action_async("win.copy-set", None, |win, _, _| async move {
        win.transfer_set(false).await;
    });
    klass.install_action_async("win.archive-set", None, |win, _, _| async move {
        win.archive_set();
    });
}

impl LpImageWindow {
    pub(super) fn setup_set(&self) {
        let imp = self.imp();

        if let Some(popover) = imp.set_menu_button.popover() {
            popover.set_has_arrow(false);
            popover.set_offset(0, -8);
        }

        imp.thumbnail_strip.connect_activate_file(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move |_, file| obj.imp().image_view.show_file(file)
        ));

        imp.thumbnail_strip.connect_remove_file(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move |_, file| {
                obj.imp().image_view.remove_from_set(file);
                obj.update_set();
            }
        ));

        imp.image_view.connect_multiple_notify(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            move |_| obj.update_set()
        ));

        imp.image_view.connect_current_page_notify(glib::clone!(
            #[weak(rename_to = obj)]
            self,
            // The file list is only complete once the first image shows
            move |_| obj.update_set()
        ));
    }

    /// Updates the thumbnails and the previous/next buttons
    pub(super) fn update_set(&self) {
        let imp = self.imp();
        let multiple = imp.image_view.multiple();

        if multiple {
            imp.thumbnail_strip.set_files(&imp.image_view.files());
            imp.thumbnail_strip
                .set_current(imp.image_view.current_file().as_ref());
        } else {
            imp.thumbnail_strip.set_files(&[]);
        }
    }

    /// Moves or copies all images of the set into a folder the user picks
    async fn transfer_set(&self, move_files: bool) {
        let imp = self.imp();
        let files = imp.image_view.files();

        let dialog = gtk::FileDialog::new();
        dialog.set_title(&if move_files {
            gettext("Move Images To")
        } else {
            gettext("Copy Images To")
        });
        let folder = match dialog
            .select_folder_future(self.try_window().as_ref())
            .await
        {
            Ok(folder) => folder,
            Err(err) => {
                if !err.matches(gtk::DialogError::Dismissed) {
                    tracing::error!("Failed to select folder: {err}");
                }
                return;
            }
        };

        let current = imp.image_view.current_file();
        let mut new_files = Vec::new();
        let mut new_current = None;
        let mut errors = Vec::new();

        for file in files {
            let Some(name) = file.basename() else {
                continue;
            };
            let target = folder.child(name);

            let result = if move_files {
                file.move_future(&target, gio::FileCopyFlags::NONE, glib::Priority::DEFAULT)
                    .0
                    .await
            } else {
                file.copy_future(&target, gio::FileCopyFlags::NONE, glib::Priority::DEFAULT)
                    .0
                    .await
            };

            match result {
                Ok(()) if move_files => {
                    if current.as_ref().is_some_and(|x| x.equal(&file)) {
                        new_current = Some(target.clone());
                    }
                    new_files.push(target);
                }
                Ok(()) => new_files.push(file),
                Err(err) => {
                    errors.push(format!("{}: {err}", file.uri()));
                    new_files.push(file);
                }
            }
        }

        if move_files {
            imp.image_view
                .set_images_as_set(new_files, new_current.or(current));
        }

        if !errors.is_empty() {
            self.window_show_error(
                &if move_files {
                    gettext("Could not move all images")
                } else {
                    gettext("Could not copy all images")
                },
                &errors.join("\n"),
                ErrorType::General,
            );
        }
    }

    /// Asks the archive manager to pack all images of the set
    fn archive_set(&self) {
        let paths: Vec<std::ffi::OsString> = self
            .imp()
            .image_view
            .files()
            .iter()
            .filter_map(|x| x.path())
            .map(|x| x.into_os_string())
            .collect();

        let mut args: Vec<&std::ffi::OsStr> = vec!["file-roller".as_ref(), "--add".as_ref()];
        args.extend(paths.iter().map(|x| x.as_os_str()));

        if let Err(err) = gio::Subprocess::newv(&args, gio::SubprocessFlags::NONE) {
            self.window_show_error(
                &gettext("Could not create archive"),
                &format!("Failed to start the archive manager: {err}"),
                ErrorType::General,
            );
        }
    }
}
