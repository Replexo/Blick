// Copyright (c) 2026 Blink contributors
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

//! Video editor: trim, mute, rotate and flip
//!
//! The preview plays only the kept range. Saving runs `ffmpeg`, see
//! [`crate::video_export`].

use std::cell::{Cell, OnceCell, RefCell};
use std::path::{Path, PathBuf};
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::clone;
use gst::prelude::*;

use crate::deps::*;
use crate::util::gettext::*;
use crate::util::root::ParentWindow;
use crate::video_export::{self, Quality, VideoEdit};
use crate::widgets::LpVideoTrimmer;
use crate::widgets::video_player::{format_time, video_info};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct LpVideoEditor {
        pub(super) file: OnceCell<gio::File>,
        pub(super) duration: Cell<f64>,
        pub(super) source_bitrate: Cell<Option<u64>>,
        pub(super) edit: Cell<Option<VideoEdit>>,
        /// Edits for undo and redo, and the one shown
        pub(super) history: RefCell<Vec<VideoEdit>>,
        pub(super) history_index: Cell<usize>,

        pub(super) playbin: OnceCell<gst::Element>,
        pub(super) flip_filter: OnceCell<gst::Element>,
        pub(super) bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,
        pub(super) playing: Cell<bool>,
        pub(super) tick: RefCell<Option<glib::SourceId>>,

        pub(super) picture: gtk::Picture,
        pub(super) trimmer_bin: adw::Bin,
        pub(super) trimmer: OnceCell<LpVideoTrimmer>,
        pub(super) play_button: gtk::Button,
        pub(super) mute_button: gtk::ToggleButton,
        pub(super) length_label: gtk::Label,
        pub(super) header: adw::HeaderBar,
        pub(super) save_button: gtk::MenuButton,
        pub(super) undo_button: gtk::Button,
        pub(super) redo_button: gtk::Button,
        pub(super) actions: gio::SimpleActionGroup,

        pub(super) saving: gtk::Revealer,
        pub(super) saving_title: gtk::Label,
        pub(super) progress: gtk::ProgressBar,
        pub(super) cancellable: RefCell<Option<gio::Cancellable>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpVideoEditor {
        const NAME: &'static str = "LpVideoEditor";
        type Type = super::LpVideoEditor;
        type ParentType = adw::Bin;
    }

    impl ObjectImpl for LpVideoEditor {
        fn constructed(&self) {
            self.parent_constructed();
            self.build();
        }

        fn dispose(&self) {
            if let Some(source) = self.tick.take() {
                source.remove();
            }
            if let Some(cancellable) = self.cancellable.take() {
                cancellable.cancel();
            }
            self.bus_watch.take();
            if let Some(playbin) = self.playbin.get() {
                let _ = playbin.set_state(gst::State::Null);
            }
        }
    }

    impl WidgetImpl for LpVideoEditor {}
    impl BinImpl for LpVideoEditor {}

    impl LpVideoEditor {
        fn build(&self) {
            let obj = self.obj();
            obj.add_css_class("glance-editor");

            // Header: cancel, undo and redo, save menu
            let cancel = gtk::Button::with_mnemonic(&gettext("_Cancel"));
            cancel.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| {
                    glib::spawn_future_local(async move { obj.close().await });
                }
            ));
            self.header.pack_start(&cancel);
            self.header.set_show_start_title_buttons(false);
            self.header.set_show_end_title_buttons(false);

            self.undo_button.set_icon_name("glance-undo-symbolic");
            self.undo_button.set_tooltip_text(Some(&gettext("Undo")));
            self.undo_button.set_action_name(Some("video-edit.undo"));
            self.redo_button.set_icon_name("glance-redo-symbolic");
            self.redo_button.set_tooltip_text(Some(&gettext("Redo")));
            self.redo_button.set_action_name(Some("video-edit.redo"));

            let menu = gio::Menu::new();
            let save = gio::Menu::new();
            save.append(Some(&gettext("_Save a Copy…")), Some("video-edit.save-copy"));
            save.append(Some(&gettext("_Replace Original")), Some("video-edit.replace"));
            save.append(
                Some(&gettext("Save S_maller Copy…")),
                Some("video-edit.save-smaller"),
            );
            menu.append_section(None, &save);
            let frame = gio::Menu::new();
            frame.append(
                Some(&gettext("Save _Frame as Image")),
                Some("video-edit.save-frame"),
            );
            menu.append_section(None, &frame);
            self.save_button.set_label(&gettext("_Save"));
            self.save_button.set_use_underline(true);
            self.save_button.set_menu_model(Some(&menu));
            self.save_button.add_css_class("suggested-action");
            self.header.pack_end(&self.save_button);
            self.header.pack_end(&self.redo_button);
            self.header.pack_end(&self.undo_button);
            let title = adw::WindowTitle::new(&gettext("Edit Video"), "");
            self.header.set_title_widget(Some(&title));

            // Preview
            self.picture.set_content_fit(gtk::ContentFit::Contain);
            self.picture.set_hexpand(true);
            self.picture.set_vexpand(true);
            self.picture.set_margin_top(12);
            self.picture.set_margin_bottom(12);
            self.picture.set_margin_start(12);
            self.picture.set_margin_end(12);
            let click = gtk::GestureClick::new();
            click.connect_released(clone!(
                #[weak]
                obj,
                move |_, _, _, _| obj.imp().toggle_playing()
            ));
            self.picture.add_controller(click);

            // Bottom bar: play, trimmer, mute, rotate, flip
            self.play_button.set_icon_name("glance-play-symbolic");
            self.play_button.set_tooltip_text(Some(&gettext("Play")));
            self.play_button.add_css_class("flat");
            self.play_button.add_css_class("circular");
            self.play_button.set_valign(gtk::Align::Center);
            self.play_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.imp().toggle_playing()
            ));

            self.mute_button.set_icon_name("glance-volume-high-symbolic");
            self.mute_button.set_tooltip_text(Some(&gettext("Remove Sound")));
            self.mute_button.add_css_class("flat");
            self.mute_button.add_css_class("circular");
            self.mute_button.set_valign(gtk::Align::Center);
            self.mute_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |button| {
                    let mute = button.is_active();
                    obj.imp().change(|edit| edit.mute = mute);
                }
            ));

            let rotate = gtk::Button::from_icon_name("glance-rotate-left-symbolic");
            rotate.set_tooltip_text(Some(&gettext("Rotate Left")));
            let flip = gtk::Button::from_icon_name("glance-flip-symbolic");
            flip.set_tooltip_text(Some(&gettext("Flip Horizontally")));
            for button in [&rotate, &flip] {
                button.add_css_class("flat");
                button.add_css_class("circular");
                button.set_valign(gtk::Align::Center);
            }
            rotate.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.imp().change(|edit| edit.rotation = (edit.rotation + 1) % 4)
            ));
            flip.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.imp().change(|edit| edit.flip = !edit.flip)
            ));

            self.length_label.add_css_class("numeric");
            self.length_label.add_css_class("caption");
            self.length_label.set_width_chars(5);
            self.trimmer_bin.set_hexpand(true);
            self.trimmer_bin.set_child(Some(&adw::Spinner::new()));

            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            bar.add_css_class("glance-pill");
            bar.set_margin_start(12);
            bar.set_margin_end(12);
            bar.set_margin_bottom(12);
            bar.append(&self.play_button);
            bar.append(&self.trimmer_bin);
            bar.append(&self.length_label);
            bar.append(&self.mute_button);
            bar.append(&rotate);
            bar.append(&flip);

            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.append(&self.picture);
            content.append(&bar);

            // Saving progress over everything
            self.saving_title.add_css_class("title-4");
            self.progress.set_show_text(true);
            self.progress.set_size_request(260, -1);
            let stop = gtk::Button::with_mnemonic(&gettext("_Cancel"));
            stop.add_css_class("pill");
            stop.set_halign(gtk::Align::Center);
            stop.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| {
                    if let Some(cancellable) = obj.imp().cancellable.borrow().as_ref() {
                        cancellable.cancel();
                    }
                }
            ));
            let card = gtk::Box::new(gtk::Orientation::Vertical, 18);
            card.add_css_class("card");
            card.set_halign(gtk::Align::Center);
            card.set_valign(gtk::Align::Center);
            card.set_margin_start(24);
            card.set_margin_end(24);
            let inner = gtk::Box::new(gtk::Orientation::Vertical, 18);
            inner.set_margin_top(24);
            inner.set_margin_bottom(24);
            inner.set_margin_start(24);
            inner.set_margin_end(24);
            inner.append(&self.saving_title);
            inner.append(&self.progress);
            inner.append(&stop);
            card.append(&inner);
            let dim = adw::Bin::new();
            dim.add_css_class("saving-overlay");
            dim.set_child(Some(&card));
            self.saving.set_child(Some(&dim));
            self.saving
                .set_transition_type(gtk::RevealerTransitionType::Crossfade);
            self.saving.set_can_target(false);

            let overlay = gtk::Overlay::new();
            overlay.set_child(Some(&content));
            overlay.add_overlay(&self.saving);

            let toolbar = adw::ToolbarView::new();
            toolbar.add_top_bar(&self.header);
            toolbar.set_content(Some(&overlay));
            obj.set_child(Some(&toolbar));

            self.setup_actions();
        }

        fn setup_actions(&self) {
            let obj = self.obj();
            let add = |name: &str, f: fn(&super::LpVideoEditor)| {
                let action = gio::SimpleAction::new(name, None);
                action.connect_activate(clone!(
                    #[weak]
                    obj,
                    move |_, _| f(&obj)
                ));
                self.actions.add_action(&action);
            };
            add("undo", |obj| obj.imp().move_history(-1));
            add("redo", |obj| obj.imp().move_history(1));
            add("save-copy", |obj| {
                glib::spawn_future_local(clone!(
                    #[weak]
                    obj,
                    async move { obj.save_copy().await }
                ));
            });
            add("replace", |obj| {
                glib::spawn_future_local(clone!(
                    #[weak]
                    obj,
                    async move { obj.replace_original().await }
                ));
            });
            add("save-smaller", |obj| {
                glib::spawn_future_local(clone!(
                    #[weak]
                    obj,
                    async move { obj.save_smaller().await }
                ));
            });
            add("save-frame", |obj| {
                glib::spawn_future_local(clone!(
                    #[weak]
                    obj,
                    async move { obj.save_frame().await }
                ));
            });
            obj.insert_action_group("video-edit", Some(&self.actions));

            let shortcuts = gtk::ShortcutController::new();
            shortcuts.set_scope(gtk::ShortcutScope::Global);
            for (key, modifiers, action) in [
                (gdk::Key::z, gdk::ModifierType::CONTROL_MASK, "video-edit.undo"),
                (
                    gdk::Key::z,
                    gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
                    "video-edit.redo",
                ),
                (gdk::Key::s, gdk::ModifierType::CONTROL_MASK, "video-edit.replace"),
                (
                    gdk::Key::s,
                    gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK,
                    "video-edit.save-copy",
                ),
            ] {
                shortcuts.add_shortcut(gtk::Shortcut::new(
                    Some(gtk::KeyvalTrigger::new(key, modifiers)),
                    Some(gtk::NamedAction::new(action)),
                ));
            }
            shortcuts.add_shortcut(gtk::Shortcut::new(
                Some(gtk::KeyvalTrigger::new(gdk::Key::space, gdk::ModifierType::empty())),
                Some(gtk::CallbackAction::new(clone!(
                    #[weak]
                    obj,
                    #[upgrade_or]
                    glib::Propagation::Proceed,
                    move |_, _| {
                        obj.imp().toggle_playing();
                        glib::Propagation::Stop
                    }
                ))),
            ));
            obj.add_controller(shortcuts);
            self.update_actions();
        }

        pub(super) fn edit(&self) -> VideoEdit {
            self.edit
                .get()
                .unwrap_or_else(|| VideoEdit::new(self.duration.get()))
        }

        /// Applies `f` to the edit and remembers it for undo
        pub(super) fn change(&self, f: impl FnOnce(&mut VideoEdit)) {
            let mut edit = self.edit();
            f(&mut edit);
            if Some(edit) == self.edit.get() {
                return;
            }
            let mut history = self.history.borrow_mut();
            let index = self.history_index.get();
            history.truncate(index + 1);
            history.push(edit);
            self.history_index.set(history.len() - 1);
            drop(history);
            self.show_edit(edit);
        }

        fn move_history(&self, step: isize) {
            let history = self.history.borrow();
            let index = self.history_index.get() as isize + step;
            if index < 0 || index as usize >= history.len() {
                return;
            }
            let edit = history[index as usize];
            drop(history);
            self.history_index.set(index as usize);
            if let Some(trimmer) = self.trimmer.get() {
                trimmer.set_range(edit.start, edit.end);
            }
            self.show_edit(edit);
        }

        /// Shows `edit` in the preview and the buttons
        pub(super) fn show_edit(&self, edit: VideoEdit) {
            self.edit.set(Some(edit));
            if self.mute_button.is_active() != edit.mute {
                self.mute_button.set_active(edit.mute);
            }
            self.mute_button.set_icon_name(if edit.mute {
                "glance-volume-muted-symbolic"
            } else {
                "glance-volume-high-symbolic"
            });
            if let Some(playbin) = self.playbin.get() {
                playbin.set_property("mute", edit.mute);
            }
            if let Some(filter) = self.flip_filter.get() {
                filter.set_property_from_str("video-direction", video_direction(&edit));
            }
            self.length_label
                .set_label(&format_time(gst::ClockTime::from_seconds_f64(edit.end - edit.start)));
            self.update_actions();
        }

        fn update_actions(&self) {
            let changed = self.edit().is_changed(self.duration.get());
            let index = self.history_index.get();
            let length = self.history.borrow().len();
            let enable = |name: &str, enabled: bool| {
                if let Some(action) = self
                    .actions
                    .lookup_action(name)
                    .and_downcast::<gio::SimpleAction>()
                {
                    action.set_enabled(enabled);
                }
            };
            enable("undo", index > 0);
            enable("redo", index + 1 < length);
            enable("save-copy", changed);
            enable("replace", changed);
        }

        pub(super) fn toggle_playing(&self) {
            self.set_playing(!self.playing.get());
        }

        /// Plays the kept range from the playhead, or from its start
        fn set_playing(&self, playing: bool) {
            let obj = self.obj();
            let Some(playbin) = self.playbin.get() else {
                return;
            };
            let edit = self.edit();
            if playing {
                let position = self.position();
                if position < edit.start || position >= edit.end - 0.05 {
                    self.seek(edit.start, true);
                }
            }
            self.playing.set(playing);
            let _ = playbin.set_state(if playing {
                gst::State::Playing
            } else {
                gst::State::Paused
            });
            self.play_button.set_icon_name(if playing {
                "glance-pause-symbolic"
            } else {
                "glance-play-symbolic"
            });
            self.play_button.set_tooltip_text(Some(&if playing {
                gettext("Pause")
            } else {
                gettext("Play")
            }));
            if playing && self.tick.borrow().is_none() {
                let source = glib::timeout_add_local(
                    Duration::from_millis(40),
                    clone!(
                        #[weak]
                        obj,
                        #[upgrade_or]
                        glib::ControlFlow::Break,
                        move || {
                            let imp = obj.imp();
                            let position = imp.position();
                            if let Some(trimmer) = imp.trimmer.get() {
                                trimmer.set_position(position);
                            }
                            // Only the kept range plays
                            if position >= imp.edit().end {
                                imp.set_playing(false);
                                imp.seek(imp.edit().start, true);
                            }
                            glib::ControlFlow::Continue
                        }
                    ),
                );
                self.tick.replace(Some(source));
            } else if !playing && let Some(source) = self.tick.take() {
                source.remove();
            }
        }

        pub(super) fn position(&self) -> f64 {
            self.playbin
                .get()
                .and_then(|p| p.query_position::<gst::ClockTime>())
                .map_or(0., |p| p.seconds_f64())
        }

        pub(super) fn seek(&self, seconds: f64, accurate: bool) {
            let Some(playbin) = self.playbin.get() else {
                return;
            };
            let flags = if accurate {
                gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE
            } else {
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT
            };
            let _ = playbin.seek_simple(flags, gst::ClockTime::from_seconds_f64(seconds.max(0.)));
        }

        pub(super) fn set_saving(&self, title: Option<&str>) {
            let saving = title.is_some();
            if let Some(title) = title {
                self.saving_title.set_label(title);
                self.progress.set_fraction(0.);
            }
            self.saving.set_reveal_child(saving);
            self.saving.set_can_target(saving);
            self.header.set_sensitive(!saving);
        }
    }
}

glib::wrapper! {
    pub struct LpVideoEditor(ObjectSubclass<imp::LpVideoEditor>)
        @extends gtk::Widget, adw::Bin,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

/// `videoflip` direction for an edit: flipped first, then rotated left
fn video_direction(edit: &VideoEdit) -> &'static str {
    match (edit.rotation.rem_euclid(4), edit.flip) {
        (0, false) => "identity",
        (1, false) => "90l",
        (2, false) => "180",
        (3, false) => "90r",
        (0, true) => "horiz",
        (1, true) => "ul-lr",
        (2, true) => "vert",
        _ => "ur-ll",
    }
}

impl LpVideoEditor {
    pub fn new(file: &gio::File) -> Self {
        let obj: Self = glib::Object::new();
        let _ = obj.imp().file.set(file.clone());
        if let Some(name) = file.basename()
            && let Some(title) = obj
                .imp()
                .header
                .title_widget()
                .and_downcast::<adw::WindowTitle>()
            {
                title.set_subtitle(&name.display().to_string());
            }
        if let Err(err) = obj.build_preview(file) {
            tracing::warn!("Video editor preview failed: {err}");
        }
        glib::spawn_future_local(clone!(
            #[weak]
            obj,
            async move { obj.load_info().await }
        ));
        obj
    }

    fn file(&self) -> gio::File {
        self.imp().file.get().cloned().unwrap()
    }

    fn build_preview(&self, file: &gio::File) -> anyhow::Result<()> {
        let imp = self.imp();
        let sink = gst::ElementFactory::make("gtk4paintablesink").build()?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        imp.picture.set_paintable(Some(&paintable));
        let video_sink = if paintable
            .property::<Option<gdk::GLContext>>("gl-context")
            .is_some()
        {
            gst::ElementFactory::make("glsinkbin")
                .property("sink", &sink)
                .build()
                .unwrap_or(sink.clone())
        } else {
            sink
        };
        let flip = gst::ElementFactory::make("videoflip").build()?;
        flip.set_property_from_str("video-direction", "auto");
        let playbin = gst::ElementFactory::make("playbin3")
            .property("uri", file.uri().as_str())
            .property("video-sink", &video_sink)
            .property("video-filter", &flip)
            .build()?;
        let bus = playbin.bus().ok_or_else(|| anyhow::anyhow!("No bus"))?;
        let guard = bus.add_watch_local(clone!(
            #[weak(rename_to = obj)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move |_, message| {
                if let gst::MessageView::Eos(_) = message.view() {
                    let imp = obj.imp();
                    imp.toggle_playing();
                    imp.seek(imp.edit().start, true);
                }
                glib::ControlFlow::Continue
            }
        ))?;
        imp.bus_watch.replace(Some(guard));
        playbin.set_state(gst::State::Paused)?;
        let _ = imp.flip_filter.set(flip);
        let _ = imp.playbin.set(playbin);
        Ok(())
    }

    /// Duration and frame rate, then the trimmer
    async fn load_info(&self) {
        let uri = self.file().uri().to_string();
        let info = gio::spawn_blocking(move || video_info(&uri))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let imp = self.imp();
        let duration = info.duration.map_or(0., |d| d.seconds_f64());
        imp.duration.set(duration);
        imp.source_bitrate.set(None);
        let edit = VideoEdit::new(duration);
        imp.history.replace(vec![edit]);
        imp.history_index.set(0);

        let frame = info.frame_rate.map_or(1. / 30., |fps| 1. / fps.max(1.));
        let trimmer = LpVideoTrimmer::new(&self.file(), duration, frame);
        trimmer.connect_preview(clone!(
            #[weak(rename_to = obj)]
            self,
            move |_, time| {
                let imp = obj.imp();
                if imp.playing.get() {
                    imp.set_playing_false();
                }
                imp.seek(time, true);
            }
        ));
        trimmer.connect_seek(clone!(
            #[weak(rename_to = obj)]
            self,
            move |_, time| obj.imp().seek(time, true)
        ));
        trimmer.connect_range_changed(clone!(
            #[weak(rename_to = obj)]
            self,
            move |_, start, end| {
                obj.imp().change(|edit| {
                    edit.start = start;
                    edit.end = end;
                })
            }
        ));
        imp.trimmer_bin.set_child(Some(&trimmer));
        let _ = imp.trimmer.set(trimmer);
        imp.show_edit(edit);
    }

    /// Back to the viewer, after asking when there are changes
    async fn close(&self) {
        let imp = self.imp();
        if imp.edit().is_changed(imp.duration.get()) {
            let dialog = adw::AlertDialog::new(
                Some(&gettext("Discard Changes?")),
                Some(&gettext("The changes to this video will be lost.")),
            );
            dialog.add_responses(&[("cancel", &gettext("_Cancel")), ("discard", &gettext("_Discard"))]);
            dialog.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");
            if dialog.choose_future(Some(self)).await != "discard" {
                return;
            }
        }
        self.window_inspect(|w| w.show_image());
    }

    /// Runs `export` with the progress shown, and reports errors
    async fn run(
        &self,
        title: &str,
        export: impl AsyncFnOnce(gio::Cancellable, Box<dyn Fn(f64)>) -> Result<(), video_export::Error>,
    ) -> bool {
        let imp = self.imp();
        imp.set_playing_false();
        let cancellable = gio::Cancellable::new();
        imp.cancellable.replace(Some(cancellable.clone()));
        imp.set_saving(Some(title));
        let progress = imp.progress.clone();
        let result = export(
            cancellable,
            Box::new(move |fraction| progress.set_fraction(fraction)),
        )
        .await;
        imp.cancellable.take();
        imp.set_saving(None);
        match result {
            Ok(()) => true,
            Err(video_export::Error::Cancelled) => false,
            Err(err) => {
                let dialog = adw::AlertDialog::new(
                    Some(&gettext("Could Not Save Video")),
                    Some(&err.to_string()),
                );
                dialog.add_response("close", &gettext("_Close"));
                dialog.present(Some(self));
                false
            }
        }
    }

    fn input_path(&self) -> Option<PathBuf> {
        let path = self.file().path();
        if path.is_none() {
            self.show_toast(&gettext("Only videos in local folders can be edited"));
        }
        path
    }

    fn show_toast(&self, text: &str) {
        self.window_inspect(|w| w.show_toast(text, adw::ToastPriority::High));
    }

    /// A free name next to the video, like `clip (Edited).mp4`
    fn suggested_name(&self, suffix: &str, extension: Option<&str>) -> gio::File {
        let file = self.file();
        let Some(path) = file.path() else {
            return file;
        };
        let stem = path
            .file_stem()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_default();
        let extension = extension
            .map(|x| x.to_string())
            .or_else(|| path.extension().map(|x| x.to_string_lossy().to_string()))
            .unwrap_or_else(|| "mp4".to_string());
        for n in 1..100 {
            let name = if n == 1 {
                format!("{stem} ({suffix}).{extension}")
            } else {
                format!("{stem} ({suffix} {n}).{extension}")
            };
            let candidate = path.with_file_name(name);
            if !candidate.exists() {
                return gio::File::for_path(candidate);
            }
        }
        file
    }

    async fn choose_file(&self, suggested: &gio::File) -> Option<PathBuf> {
        let dialog = gtk::FileDialog::new();
        dialog.set_initial_file(Some(suggested));
        let window = self.root().and_downcast::<gtk::Window>();
        dialog.save_future(window.as_ref()).await.ok()?.path()
    }

    async fn save_copy(&self) {
        let Some(input) = self.input_path() else {
            return;
        };
        // Translators: File name suffix of edited copies
        let suggested = self.suggested_name(&gettext("Edited"), None);
        let Some(output) = self.choose_file(&suggested).await else {
            return;
        };
        let edit = self.imp().edit();
        let saved = self
            .run(&gettext("Saving Video"), async |cancellable, progress| {
                video_export::save(&input, &output, &edit, progress, &cancellable).await
            })
            .await;
        if saved {
            self.window_inspect(|w| w.show_specific_image(gio::File::for_path(&output)));
        }
    }

    /// Writes next to the original, moves the original to the trash or
    /// overwrites it when that is not possible
    async fn replace_original(&self) {
        let Some(input) = self.input_path() else {
            return;
        };
        let temp = hidden_temp_path(&input);
        let edit = self.imp().edit();
        let saved = self
            .run(&gettext("Saving Video"), async |cancellable, progress| {
                video_export::save(&input, &temp, &edit, progress, &cancellable).await
            })
            .await;
        if !saved {
            let _ = std::fs::remove_file(&temp);
            return;
        }
        let original = gio::File::for_path(&input);
        if let Err(err) = original.trash_future(glib::Priority::DEFAULT).await {
            tracing::info!("Could not move the original to the trash, overwriting: {err}");
        }
        if let Err(err) = std::fs::rename(&temp, &input) {
            let _ = std::fs::remove_file(&temp);
            self.show_toast(&gettext_f("Could not replace the original: {}", [err.to_string()]));
            return;
        }
        // The folder reports the trashed original a moment later; showing the
        // video before that would move the viewer to its neighbour
        glib::timeout_future(Duration::from_millis(600)).await;
        self.window_inspect(|w| w.show_specific_image(original));
    }

    async fn save_smaller(&self) {
        let Some(input) = self.input_path() else {
            return;
        };
        let imp = self.imp();
        let edit = imp.edit();
        let seconds = edit.end - edit.start;
        let bitrate = video_export::probe(&input, &gio::Cancellable::new())
            .await
            .ok()
            .and_then(|info| info.bitrate);

        // Quality choice with estimated sizes
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let mut group: Option<gtk::CheckButton> = None;
        let mut buttons = Vec::new();
        for (quality, name) in [
            (Quality::Small, gettext("Small")),
            (Quality::Medium, gettext("Medium")),
            (Quality::Original, gettext("Original Quality")),
        ] {
            let size = glib::format_size(video_export::estimate_size(quality, seconds, bitrate, edit.mute));
            // Translators: A quality choice, {} are its name and estimated file size like “Small, about 12 MB”
            let button = gtk::CheckButton::with_label(&gettext_f("{}, about {}", [name, size.to_string()]));
            if let Some(group) = &group {
                button.set_group(Some(group));
            } else {
                group = Some(button.clone());
            }
            choices.append(&button);
            buttons.push((quality, button));
        }
        buttons[1].1.set_active(true);

        let dialog = adw::AlertDialog::new(
            Some(&gettext("Save Smaller Copy")),
            Some(&gettext("The copy is encoded again and keeps the original as it is.")),
        );
        dialog.set_extra_child(Some(&choices));
        dialog.add_responses(&[("cancel", &gettext("_Cancel")), ("save", &gettext("_Save…"))]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        if dialog.choose_future(Some(self)).await != "save" {
            return;
        }
        let quality = buttons
            .iter()
            .find(|(_, b)| b.is_active())
            .map_or(Quality::Medium, |(q, _)| *q);

        let extension = video_export::smaller_extension().await;
        // Translators: File name suffix of smaller copies
        let suggested = self.suggested_name(&gettext("Smaller"), Some(extension));
        let Some(output) = self.choose_file(&suggested).await else {
            return;
        };
        let saved = self
            .run(&gettext("Saving Smaller Copy"), async |cancellable, progress| {
                video_export::save_smaller(&input, &output, &edit, quality, progress, &cancellable).await
            })
            .await;
        if saved {
            self.window_inspect(|w| w.show_specific_image(gio::File::for_path(&output)));
        }
    }

    /// Saves the shown frame next to the video and opens it in the image
    /// editor
    async fn save_frame(&self) {
        let imp = self.imp();
        let Some(path) = self.file().path() else {
            return;
        };
        let Some(texture) = frame_texture(&imp.picture) else {
            self.show_toast(&gettext("Could not take the frame"));
            return;
        };
        let time = format_time(gst::ClockTime::from_seconds_f64(imp.position())).replace(':', "-");
        // Translators: File name suffix of a saved video frame, {} is its time like 1-05
        let suggested = self.suggested_name(&gettext_f("Frame {}", [time]), Some("png"));
        let Some(target) = suggested.path() else {
            return;
        };
        if let Err(err) = texture.save_to_png(&target) {
            self.show_toast(&gettext_f("Could not save the frame: {}", [err.to_string()]));
            return;
        }
        let _ = path;
        self.window_inspect(|w| w.open_in_editor(gio::File::for_path(&target)));
    }
}

impl imp::LpVideoEditor {
    fn set_playing_false(&self) {
        if self.playing.get() {
            self.toggle_playing();
        }
    }
}

/// The frame a picture shows, as a texture
fn frame_texture(picture: &gtk::Picture) -> Option<gdk::Texture> {
    let paintable = picture.paintable()?.current_image();
    let (width, height) = (paintable.intrinsic_width(), paintable.intrinsic_height());
    if width <= 0 || height <= 0 {
        return None;
    }
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);
    let node = snapshot.to_node()?;
    let renderer = picture.native()?.renderer()?;
    Some(renderer.render_texture(
        node,
        Some(&graphene::Rect::new(0., 0., width as f32, height as f32)),
    ))
}

/// `.name.tmp.ext` next to `path`, hidden so the viewer does not list it
fn hidden_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_stem()
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map(|x| format!(".{}", x.to_string_lossy()))
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp{extension}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_name_is_hidden() {
        assert_eq!(
            hidden_temp_path(Path::new("/a/clip.mp4")),
            Path::new("/a/.clip.tmp.mp4")
        );
    }

    #[test]
    fn directions() {
        let mut edit = VideoEdit::new(1.);
        assert_eq!(video_direction(&edit), "identity");
        edit.rotation = 1;
        assert_eq!(video_direction(&edit), "90l");
        edit.flip = true;
        assert_eq!(video_direction(&edit), "ul-lr");
    }
}
