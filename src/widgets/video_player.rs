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

//! Plays a video in an image page
//!
//! GStreamer's `playbin3` decodes into the GTK 4 paintable sink, shown by a
//! picture over the page's image. While paused, the frame goes into the
//! page's [`LpImage`], so zoom, pan, live text and copying work as for
//! images.

use std::cell::{Cell, OnceCell, RefCell};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::clone;
use glib::subclass::Signal;
use gst::prelude::*;

use crate::deps::*;
use crate::util::gettext::*;
use crate::widgets::LpImage;

/// How far the arrow keys and double taps skip
const SKIP: gst::ClockTime = gst::ClockTime::from_seconds(10);
/// The controls hide after this long without moving the pointer
const HIDE_DELAY: Duration = Duration::from_secs(3);
/// Playback speeds offered in the menu
const SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];
/// Positions closer than this to the start or end are not remembered
const RESUME_MARGIN: f64 = 5.;

thread_local! {
    /// Volume and mute, shared by all videos
    static VOLUME: Cell<f64> = const { Cell::new(1.) };
    static MUTED: Cell<bool> = const { Cell::new(false) };
}

mod imp {
    use super::*;

    #[derive(Debug, Default)]
    pub struct LpVideoPlayer {
        pub(super) file: RefCell<Option<gio::File>>,
        pub(super) image: RefCell<Option<LpImage>>,
        pub(super) playbin: OnceCell<gst::Element>,
        pub(super) video_sink: OnceCell<gst::Element>,
        pub(super) bus_watch: RefCell<Option<gst::bus::BusWatchGuard>>,

        pub(super) picture: gtk::Picture,
        pub(super) controls: gtk::Revealer,
        pub(super) play_button: gtk::Button,
        pub(super) position_label: gtk::Label,
        pub(super) duration_label: gtk::Label,
        pub(super) seek: gtk::Scale,
        pub(super) volume_button: gtk::MenuButton,
        pub(super) volume_scale: gtk::Scale,
        pub(super) mute_button: gtk::ToggleButton,
        pub(super) actions: gio::SimpleActionGroup,

        pub(super) duration: Cell<Option<gst::ClockTime>>,
        pub(super) playing: Cell<bool>,
        pub(super) active: Cell<bool>,
        pub(super) started: Cell<bool>,
        pub(super) prerolled: Cell<bool>,
        pub(super) looping: Cell<bool>,
        pub(super) rate: Cell<f64>,
        /// The scale is moved by the code, not the user
        pub(super) updating: Cell<bool>,
        pub(super) tick: RefCell<Option<glib::SourceId>>,
        pub(super) hide_timeout: RefCell<Option<glib::SourceId>>,
        pub(super) click_timeout: RefCell<Option<glib::SourceId>>,
        pub(super) inhibit_cookie: Cell<u32>,
        /// The frame shown in the image belongs to this position
        pub(super) frame_position: Cell<Option<gst::ClockTime>>,
        pub(super) missing_codec: RefCell<Option<String>>,

        /// Frame preview over the seek bar
        pub(super) preview: gtk::Popover,
        pub(super) preview_picture: gtk::Picture,
        pub(super) preview_label: gtk::Label,
        /// Where the pointer is on the seek bar, and if a frame is loading
        pub(super) preview_wanted: Cell<Option<gst::ClockTime>>,
        pub(super) preview_loading: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpVideoPlayer {
        const NAME: &'static str = "LpVideoPlayer";
        type Type = super::LpVideoPlayer;
    }

    impl ObjectImpl for LpVideoPlayer {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // Description and details of why the video can't play
                    Signal::builder("error")
                        .param_types([String::static_type(), String::static_type()])
                        .build(),
                    // The video played to its end
                    Signal::builder("ended").build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.rate.set(1.);
            self.build_controls();
        }

        fn dispose(&self) {
            self.save_position();
            self.preview.unparent();
            let _ = std::fs::remove_file(frame_path(self.file.borrow().as_ref()));
            self.uninhibit();
            if let Some(source) = self.tick.take() {
                source.remove();
            }
            if let Some(source) = self.hide_timeout.take() {
                source.remove();
            }
            if let Some(source) = self.click_timeout.take() {
                source.remove();
            }
            self.bus_watch.take();
            if let Some(playbin) = self.playbin.get() {
                let _ = playbin.set_state(gst::State::Null);
            }
        }
    }

    impl LpVideoPlayer {
        fn build_controls(&self) {
            let obj = self.obj();

            self.picture.set_content_fit(gtk::ContentFit::Contain);
            self.picture.set_hexpand(true);
            self.picture.set_vexpand(true);
            self.picture.add_css_class("video");

            self.play_button.set_icon_name("media-playback-start-symbolic");
            self.play_button.set_tooltip_text(Some(&gettext("Play")));
            self.play_button.add_css_class("flat");
            self.play_button.add_css_class("circular");
            self.play_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.toggle_playing()
            ));

            for label in [&self.position_label, &self.duration_label] {
                label.add_css_class("numeric");
                label.add_css_class("caption");
                label.set_label("0:00");
            }

            self.seek.set_hexpand(true);
            self.seek.set_draw_value(false);
            self.seek.set_range(0., 1.);
            self.seek.update_property(&[gtk::accessible::Property::Label(&gettext(
                "Position",
            ))]);
            self.seek.connect_change_value(clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, _, value| {
                    let imp = obj.imp();
                    if !imp.updating.get() {
                        obj.seek_to(gst::ClockTime::from_mseconds((value * 1000.) as u64));
                    }
                    glib::Propagation::Proceed
                }
            ));

            self.setup_preview();

            self.volume_scale.set_orientation(gtk::Orientation::Vertical);
            self.volume_scale.set_inverted(true);
            self.volume_scale.set_range(0., 1.);
            self.volume_scale.set_increments(0.05, 0.2);
            self.volume_scale.set_size_request(-1, 140);
            self.volume_scale.set_value(VOLUME.get());
            self.volume_scale
                .update_property(&[gtk::accessible::Property::Label(&gettext("Volume"))]);
            self.volume_scale.connect_value_changed(clone!(
                #[weak]
                obj,
                move |scale| {
                    VOLUME.set(scale.value());
                    obj.imp().apply_volume();
                }
            ));

            self.mute_button.set_icon_name("audio-volume-muted-symbolic");
            self.mute_button.set_tooltip_text(Some(&gettext("Mute")));
            self.mute_button.add_css_class("flat");
            self.mute_button.add_css_class("circular");
            self.mute_button.set_active(MUTED.get());
            self.mute_button.connect_toggled(clone!(
                #[weak]
                obj,
                move |button| {
                    MUTED.set(button.is_active());
                    obj.imp().apply_volume();
                }
            ));

            let volume_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
            volume_box.append(&self.volume_scale);
            volume_box.append(&self.mute_button);
            let volume_popover = gtk::Popover::new();
            volume_popover.set_child(Some(&volume_box));
            self.volume_button.set_popover(Some(&volume_popover));
            self.volume_button.set_tooltip_text(Some(&gettext("Volume")));
            self.volume_button.set_direction(gtk::ArrowType::Up);
            self.volume_button.add_css_class("flat");
            self.volume_button.add_css_class("circular");

            // Speed and looping
            let speed = gio::SimpleAction::new_stateful(
                "speed",
                Some(glib::VariantTy::STRING),
                &"1".to_variant(),
            );
            speed.connect_activate(clone!(
                #[weak]
                obj,
                move |action, target| {
                    if let Some(rate) = target
                        .and_then(|x| x.get::<String>())
                        .and_then(|x| x.parse::<f64>().ok())
                    {
                        action.set_state(&rate.to_string().to_variant());
                        obj.set_rate(rate);
                    }
                }
            ));
            self.actions.add_action(&speed);
            let looping = gio::SimpleAction::new_stateful("loop", None, &false.to_variant());
            looping.connect_activate(clone!(
                #[weak]
                obj,
                move |action, _| {
                    let looping = !obj.imp().looping.get();
                    obj.imp().looping.set(looping);
                    action.set_state(&looping.to_variant());
                }
            ));
            self.actions.add_action(&looping);

            let speed_menu = gio::Menu::new();
            for rate in SPEEDS {
                // Translators: Playback speed, {} is a number like 1.5
                let label = gettext_f("{}×", [rate.to_string()]);
                speed_menu.append(
                    Some(&label),
                    Some(&format!("video.speed::{}", rate)),
                );
            }
            let menu = gio::Menu::new();
            menu.append_section(Some(&gettext("Speed")), &speed_menu);
            let loop_section = gio::Menu::new();
            loop_section.append(Some(&gettext("_Loop")), Some("video.loop"));
            menu.append_section(None, &loop_section);
            let options = gtk::MenuButton::new();
            options.set_icon_name("view-more-symbolic");
            options.set_menu_model(Some(&menu));
            options.set_tooltip_text(Some(&gettext("Playback Options")));
            options.set_direction(gtk::ArrowType::Up);
            options.add_css_class("flat");
            options.add_css_class("circular");

            let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            bar.add_css_class("osd");
            bar.add_css_class("toolbar");
            bar.add_css_class("video-controls");
            bar.set_margin_start(12);
            bar.set_margin_end(12);
            bar.set_margin_bottom(12);
            bar.append(&self.play_button);
            bar.append(&self.position_label);
            bar.append(&self.seek);
            bar.append(&self.duration_label);
            bar.append(&self.volume_button);
            bar.append(&options);
            bar.insert_action_group("video", Some(&self.actions));

            let clamp = adw::Clamp::new();
            clamp.set_maximum_size(720);
            clamp.set_child(Some(&bar));

            self.controls.set_child(Some(&clamp));
            self.controls
                .set_transition_type(gtk::RevealerTransitionType::Crossfade);
            self.controls.set_valign(gtk::Align::End);
            self.controls.set_reveal_child(true);
            self.controls.set_can_focus(false);

            // Pointer movement shows the controls; they hide while playing
            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(clone!(
                #[weak]
                obj,
                move |_, _, _| obj.imp().show_controls()
            ));
            self.picture.add_controller(motion);
            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(clone!(
                #[weak]
                obj,
                move |_, _, _| obj.imp().show_controls()
            ));
            self.controls.add_controller(motion);

            // Click plays and pauses, a double click toggles fullscreen and
            // a double tap on the left or right third skips
            let click = gtk::GestureClick::new();
            click.connect_pressed(clone!(
                #[weak]
                obj,
                move |gesture, n_press, x, _| obj.imp().clicked(gesture, n_press, x)
            ));
            self.picture.add_controller(click);

            self.setup_shortcuts();
        }

        /// Hovering the seek bar shows the frame at that point
        fn setup_preview(&self) {
            let obj = self.obj();

            self.preview_picture.set_size_request(160, 90);
            self.preview_picture
                .set_content_fit(gtk::ContentFit::Contain);
            self.preview_label.add_css_class("numeric");
            self.preview_label.add_css_class("caption");
            let preview_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
            preview_box.append(&self.preview_picture);
            preview_box.append(&self.preview_label);
            self.preview.set_child(Some(&preview_box));
            self.preview.set_autohide(false);
            self.preview.set_can_target(false);
            self.preview.set_position(gtk::PositionType::Top);
            self.preview.set_parent(&self.seek);

            let motion = gtk::EventControllerMotion::new();
            motion.connect_motion(clone!(
                #[weak]
                obj,
                move |_, x, _| obj.imp().hover_seek(x)
            ));
            motion.connect_leave(clone!(
                #[weak]
                obj,
                move |_| {
                    let imp = obj.imp();
                    imp.preview_wanted.set(None);
                    imp.preview.popdown();
                }
            ));
            self.seek.add_controller(motion);
        }

        fn hover_seek(&self, x: f64) {
            let Some(duration) = self.duration.get() else {
                return;
            };
            let width = self.seek.width().max(1) as f64;
            let fraction = (x / width).clamp(0., 1.);
            let position =
                gst::ClockTime::from_nseconds((duration.nseconds() as f64 * fraction) as u64);

            self.preview_label.set_label(&format_time(position));
            self.preview
                .set_pointing_to(Some(&gdk::Rectangle::new(x as i32, 0, 1, 1)));
            if !self.preview.is_visible() {
                self.preview.popup();
            }

            self.preview_wanted.set(Some(position));
            self.load_preview();
        }

        /// Loads the wanted frame, one at a time so fast movements skip
        /// frames
        fn load_preview(&self) {
            if self.preview_loading.get() {
                return;
            }
            let (Some(position), Some(file)) =
                (self.preview_wanted.get(), self.file.borrow().clone())
            else {
                return;
            };
            self.preview_loading.set(true);
            let obj = self.obj();
            glib::spawn_future_local(clone!(
                #[weak]
                obj,
                async move {
                    let texture = video_frame(&file, Some(position), 320).await;
                    let imp = obj.imp();
                    imp.preview_loading.set(false);
                    if let Ok(texture) = texture {
                        imp.preview_picture.set_paintable(Some(&texture));
                    }
                    if imp.preview_wanted.get().is_some_and(|wanted| wanted != position) {
                        imp.load_preview();
                    }
                }
            ));
        }

        fn setup_shortcuts(&self) {
            let obj = self.obj();
            let shortcuts = gtk::ShortcutController::new();
            shortcuts.set_scope(gtk::ShortcutScope::Global);

            let add = |key: gdk::Key, callback: fn(&super::LpVideoPlayer)| {
                shortcuts.add_shortcut(gtk::Shortcut::new(
                    Some(gtk::KeyvalTrigger::new(key, gdk::ModifierType::empty())),
                    Some(gtk::CallbackAction::new(clone!(
                        #[weak]
                        obj,
                        #[upgrade_or]
                        glib::Propagation::Proceed,
                        move |_, _| {
                            if !obj.is_active() {
                                return glib::Propagation::Proceed;
                            }
                            callback(&obj);
                            glib::Propagation::Stop
                        }
                    ))),
                ));
            };
            add(gdk::Key::space, |obj| obj.toggle_playing());
            add(gdk::Key::m, |obj| obj.toggle_mute());
            add(gdk::Key::f, |obj| {
                let _ = obj
                    .imp()
                    .picture
                    .activate_action("win.toggle-fullscreen", None);
            });
            add(gdk::Key::comma, |obj| obj.step_frame(false));
            add(gdk::Key::period, |obj| obj.step_frame(true));

            self.controls.add_controller(shortcuts);
        }

        fn clicked(&self, gesture: &gtk::GestureClick, n_press: i32, x: f64) {
            let obj = self.obj();
            let touch = gesture
                .current_event_device()
                .is_some_and(|device| {
                    gdk::prelude::DeviceExt::source(&device) == gdk::InputSource::Touchscreen
                });

            if let Some(source) = self.click_timeout.take() {
                source.remove();
            }

            if n_press == 2 {
                let width = self.picture.width() as f64;
                if touch && x < width / 3. {
                    obj.skip(false);
                } else if touch && x > width * 2. / 3. {
                    obj.skip(true);
                } else {
                    let _ = self.picture.activate_action("win.toggle-fullscreen", None);
                }
            } else if n_press == 1 {
                if touch {
                    // A tap shows the controls first
                    if !self.controls.reveals_child() {
                        self.show_controls();
                        return;
                    }
                }
                // Wait for a possible second click
                let delay = gtk::Settings::default()
                    .map_or(400, |settings| settings.gtk_double_click_time())
                    .max(100) as u64;
                let source = glib::timeout_add_local_once(
                    Duration::from_millis(delay),
                    clone!(
                        #[weak]
                        obj,
                        move || {
                            obj.imp().click_timeout.take();
                            obj.toggle_playing();
                        }
                    ),
                );
                self.click_timeout.replace(Some(source));
            }
        }

        pub(super) fn show_controls(&self) {
            self.controls.set_reveal_child(true);
            self.picture.set_cursor(None);
            self.schedule_hide();
        }

        fn schedule_hide(&self) {
            let obj = self.obj();
            if let Some(source) = self.hide_timeout.take() {
                source.remove();
            }
            if !self.playing.get() {
                return;
            }
            let source = glib::timeout_add_local_once(
                HIDE_DELAY,
                clone!(
                    #[weak]
                    obj,
                    move || {
                        let imp = obj.imp();
                        imp.hide_timeout.take();
                        let over_controls = imp
                            .controls
                            .observe_controllers()
                            .iter::<glib::Object>()
                            .flatten()
                            .filter_map(|c| c.downcast::<gtk::EventControllerMotion>().ok())
                            .any(|motion| motion.contains_pointer());
                        let menu_open = imp.volume_button.is_active();
                        if imp.playing.get() && !over_controls && !menu_open {
                            imp.controls.set_reveal_child(false);
                            imp.picture.set_cursor_from_name(Some("none"));
                        }
                    }
                ),
            );
            self.hide_timeout.replace(Some(source));
        }

        pub(super) fn apply_volume(&self) {
            let Some(playbin) = self.playbin.get() else {
                return;
            };
            if let Some(volume) = playbin.dynamic_cast_ref::<gst_audio::StreamVolume>() {
                use gst_audio::prelude::StreamVolumeExt;
                volume.set_volume(gst_audio::StreamVolumeFormat::Cubic, VOLUME.get());
                volume.set_mute(MUTED.get());
            }
            let icon = if MUTED.get() || VOLUME.get() <= 0. {
                "audio-volume-muted-symbolic"
            } else if VOLUME.get() < 0.34 {
                "audio-volume-low-symbolic"
            } else if VOLUME.get() < 0.67 {
                "audio-volume-medium-symbolic"
            } else {
                "audio-volume-high-symbolic"
            };
            self.volume_button.set_icon_name(icon);
            if self.mute_button.is_active() != MUTED.get() {
                self.mute_button.set_active(MUTED.get());
            }
            crate::mpris::changed();
        }

        pub(super) fn position(&self) -> Option<gst::ClockTime> {
            self.playbin.get()?.query_position::<gst::ClockTime>()
        }

        pub(super) fn update_position(&self) {
            let Some(position) = self.position() else {
                return;
            };
            self.position_label.set_label(&format_time(position));
            if let Some(duration) = self.duration.get() {
                self.updating.set(true);
                self.seek.set_range(0., duration.mseconds() as f64 / 1000.);
                self.seek.set_value(position.mseconds() as f64 / 1000.);
                self.updating.set(false);
            }
        }

        fn update_duration(&self) {
            let Some(duration) = self
                .playbin
                .get()
                .and_then(|p| p.query_duration::<gst::ClockTime>())
            else {
                return;
            };
            self.duration.set(Some(duration));
            self.duration_label.set_label(&format_time(duration));
            self.update_position();
        }

        pub(super) fn on_message(&self, message: &gst::Message) {
            let obj = self.obj();
            match message.view() {
                gst::MessageView::Eos(_) => {
                    if self.looping.get() {
                        obj.seek_to(gst::ClockTime::ZERO);
                    } else {
                        forget_position(obj.file().as_ref());
                        obj.set_playing(false);
                        obj.emit_by_name::<()>("ended", &[]);
                    }
                }
                gst::MessageView::AsyncDone(_) => {
                    self.update_duration();
                    if !self.prerolled.replace(true) {
                        self.resume();
                    }
                    if !self.playing.get() {
                        self.update_position();
                        glib::spawn_future_local(clone!(
                            #[weak]
                            obj,
                            async move { obj.imp().show_frame_in_image().await }
                        ));
                    }
                }
                gst::MessageView::DurationChanged(_) => self.update_duration(),
                gst::MessageView::Element(_) => {
                    if let Ok(missing) = gst_pbutils::MissingPluginMessage::parse(message) {
                        self.missing_codec
                            .replace(Some(missing.description().to_string()));
                    }
                }
                gst::MessageView::Error(err) => {
                    let details = format!(
                        "{}\n\n{}",
                        err.error(),
                        err.debug().map(|x| x.to_string()).unwrap_or_default()
                    );
                    let description = match self.missing_codec.borrow().as_ref() {
                        // Translators: {} is the name of a video or audio format
                        Some(codec) => gettext_f(
                            "Playing this video needs support for “{}”, which is not installed.",
                            [codec],
                        ),
                        None => gettext(
                            "The video could not be played. The file may be damaged or use an unsupported format.",
                        ),
                    };
                    tracing::warn!("Video error: {details}");
                    obj.set_playing(false);
                    obj.emit_by_name::<()>("error", &[&description, &details]);
                }
                _ => {}
            }
        }

        /// Continues where the video was left the last time
        fn resume(&self) {
            let obj = self.obj();
            let (Some(position), Some(duration)) =
                (saved_position(obj.file().as_ref()), self.duration.get())
            else {
                return;
            };
            let duration = duration.seconds_f64();
            if position > RESUME_MARGIN && position < duration - RESUME_MARGIN {
                tracing::debug!("Resuming video at {position} s");
                obj.seek_to(gst::ClockTime::from_seconds_f64(position));
            }
        }

        pub(super) fn save_position(&self) {
            let obj = self.obj();
            let (Some(position), Some(duration)) = (self.position(), self.duration.get()) else {
                return;
            };
            let (position, duration) = (position.seconds_f64(), duration.seconds_f64());
            if position > RESUME_MARGIN && position < duration - RESUME_MARGIN {
                store_position(obj.file().as_ref(), Some(position));
            } else {
                forget_position(obj.file().as_ref());
            }
        }

        pub(super) fn inhibit(&self) {
            if self.inhibit_cookie.get() != 0 {
                return;
            }
            let window = self.picture.root().and_downcast::<gtk::Window>();
            if let Some(app) = gio::Application::default().and_downcast::<gtk::Application>() {
                let cookie = app.inhibit(
                    window.as_ref(),
                    gtk::ApplicationInhibitFlags::IDLE,
                    Some(&gettext("Playing a video")),
                );
                self.inhibit_cookie.set(cookie);
            }
        }

        pub(super) fn uninhibit(&self) {
            let cookie = self.inhibit_cookie.replace(0);
            if cookie != 0
                && let Some(app) = gio::Application::default().and_downcast::<gtk::Application>()
            {
                app.uninhibit(cookie);
            }
        }

        /// Puts the paused frame into the image, for zoom, live text and
        /// copying
        pub(super) async fn show_frame_in_image(&self) {
            let obj = self.obj();
            let Some(image) = self.image.borrow().clone() else {
                return;
            };
            let position = self.position();
            if self.playing.get() || position.is_none() || self.frame_position.get() == position
            {
                self.picture.set_visible(self.frame_position.get() != position);
                return;
            }
            let Some(texture) = obj.frame_texture() else {
                return;
            };
            let path = frame_path(obj.file().as_ref());
            if texture.save_to_png(&path).is_err() {
                return;
            }
            image.load_video_frame(&gio::File::for_path(&path)).await;
            if !self.playing.get() && self.position() == position {
                self.frame_position.set(position);
                // The image shows the same frame and takes the input
                self.picture.set_visible(false);
            }
        }
    }
}

glib::wrapper! {
    pub struct LpVideoPlayer(ObjectSubclass<imp::LpVideoPlayer>);
}

impl LpVideoPlayer {
    /// Starts loading `file`; the picture and controls go over `image`,
    /// which shows the paused frames. Without an image, as for the motion
    /// part of a photo, the picture stays on.
    pub fn new(file: &gio::File, overlay: &gtk::Overlay, image: Option<&LpImage>) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.file.replace(Some(file.clone()));
        imp.image.replace(image.cloned());

        overlay.add_overlay(&imp.picture);
        overlay.add_overlay(&imp.controls);
        overlay.set_measure_overlay(&imp.picture, false);

        if let Err(err) = obj.build_pipeline(file) {
            let description = gettext(
                "Video playback is not available. The GStreamer “playbin3” and “gtk4paintablesink” elements are needed.",
            );
            glib::idle_add_local_once(clone!(
                #[weak]
                obj,
                move || {
                    obj.emit_by_name::<()>("error", &[&description, &err.to_string()]);
                }
            ));
        }

        obj
    }

    fn build_pipeline(&self, file: &gio::File) -> anyhow::Result<()> {
        let imp = self.imp();

        let sink = gst::ElementFactory::make("gtk4paintablesink").build()?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        imp.picture.set_paintable(Some(&paintable));

        // Use OpenGL when the sink offers a context
        let video_sink = if paintable
            .property::<Option<gdk::GLContext>>("gl-context")
            .is_some()
        {
            gst::ElementFactory::make("glsinkbin")
                .property("sink", &sink)
                .build()
                .unwrap_or(sink.clone())
        } else {
            sink.clone()
        };

        let playbin = gst::ElementFactory::make("playbin3")
            .property("uri", file.uri().as_str())
            .property("video-sink", &video_sink)
            .build()?;

        // Keep the pitch when playing faster or slower
        if let Ok(scaletempo) = gst::ElementFactory::make("scaletempo").build() {
            playbin.set_property("audio-filter", &scaletempo);
        }

        let bus = playbin.bus().ok_or_else(|| anyhow::anyhow!("No bus"))?;
        let guard = bus.add_watch_local(clone!(
            #[weak(rename_to = obj)]
            self,
            #[upgrade_or]
            glib::ControlFlow::Break,
            move |_, message| {
                obj.imp().on_message(message);
                glib::ControlFlow::Continue
            }
        ))?;
        imp.bus_watch.replace(Some(guard));

        let _ = imp.video_sink.set(sink);
        let _ = imp.playbin.set(playbin.clone());
        imp.apply_volume();

        // Shows the first frame
        playbin.set_state(gst::State::Paused)?;

        Ok(())
    }

    pub fn file(&self) -> Option<gio::File> {
        self.imp().file.borrow().clone()
    }

    /// Stops and takes the picture and controls off `overlay`
    pub fn detach(&self, overlay: &gtk::Overlay) {
        let imp = self.imp();
        self.set_active(false);
        if let Some(playbin) = imp.playbin.get() {
            let _ = playbin.set_state(gst::State::Null);
        }
        overlay.remove_overlay(&imp.picture);
        overlay.remove_overlay(&imp.controls);
    }

    pub fn is_active(&self) -> bool {
        self.imp().active.get()
    }

    pub fn is_playing(&self) -> bool {
        self.imp().playing.get()
    }

    /// Only the shown video plays and reacts to keys; it starts playing the
    /// first time it is shown
    pub fn set_active(&self, active: bool) {
        let imp = self.imp();
        if imp.active.replace(active) == active {
            return;
        }
        if active {
            crate::mpris::set_player(Some(self));
            if !imp.started.replace(true) {
                self.set_playing(true);
            }
        } else {
            self.set_playing(false);
            crate::mpris::unset_player(self);
        }
    }

    pub fn position(&self) -> Option<gst::ClockTime> {
        self.imp().position()
    }

    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.imp().duration.get()
    }

    pub fn rate(&self) -> f64 {
        self.imp().rate.get()
    }

    /// Goes to the next or previous file in the folder
    pub fn navigate(&self, forward: bool) {
        let action = if forward {
            "win.image-right-instant"
        } else {
            "win.image-left-instant"
        };
        let _ = self.imp().picture.activate_action(action, None);
    }

    /// Brings the window with the video to the front
    pub fn present(&self) {
        if let Some(window) = self.imp().picture.root().and_downcast::<gtk::Window>() {
            window.present();
        }
    }

    pub fn set_playing(&self, playing: bool) {
        let imp = self.imp();
        let Some(playbin) = imp.playbin.get() else {
            return;
        };
        if playing && !imp.active.get() {
            return;
        }

        let was_playing = imp.playing.replace(playing);
        let state = if playing {
            gst::State::Playing
        } else {
            gst::State::Paused
        };
        if let Err(err) = playbin.set_state(state) {
            tracing::warn!("Could not change video state: {err}");
        }

        if playing {
            imp.picture.set_visible(true);
            imp.frame_position.set(None);
            if let Some(image) = imp.image.borrow().as_ref() {
                image.zoom_best_fit();
            }
            imp.play_button
                .set_icon_name("media-playback-pause-symbolic");
            imp.play_button.set_tooltip_text(Some(&gettext("Pause")));
            imp.inhibit();
            if imp.tick.borrow().is_none() {
                let source = glib::timeout_add_local(
                    Duration::from_millis(250),
                    clone!(
                        #[weak(rename_to = obj)]
                        self,
                        #[upgrade_or]
                        glib::ControlFlow::Break,
                        move || {
                            obj.imp().update_position();
                            glib::ControlFlow::Continue
                        }
                    ),
                );
                imp.tick.replace(Some(source));
            }
        } else {
            imp.play_button
                .set_icon_name("media-playback-start-symbolic");
            imp.play_button.set_tooltip_text(Some(&gettext("Play")));
            imp.uninhibit();
            if let Some(source) = imp.tick.take() {
                source.remove();
            }
            if was_playing {
                imp.save_position();
            }
            imp.update_position();
        }
        imp.show_controls();
        crate::mpris::changed();
    }

    pub fn toggle_playing(&self) {
        self.set_playing(!self.is_playing());
    }

    pub fn toggle_mute(&self) {
        MUTED.set(!MUTED.get());
        self.imp().apply_volume();
    }

    /// Moves forward or back by ten seconds
    pub fn skip(&self, forward: bool) {
        let imp = self.imp();
        let Some(position) = imp.position() else {
            return;
        };
        let target = if forward {
            let target = position + SKIP;
            match imp.duration.get() {
                Some(duration) if target > duration => duration,
                _ => target,
            }
        } else {
            position.saturating_sub(SKIP)
        };
        self.seek_to(target);
        imp.show_controls();
    }

    pub fn seek_to(&self, position: gst::ClockTime) {
        let imp = self.imp();
        let Some(playbin) = imp.playbin.get() else {
            return;
        };
        let flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
        let result = playbin.seek(
            imp.rate.get(),
            flags,
            gst::SeekType::Set,
            position,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        if let Err(err) = result {
            tracing::debug!("Seek failed: {err}");
        }
        imp.position_label.set_label(&format_time(position));
    }

    fn set_rate(&self, rate: f64) {
        let imp = self.imp();
        imp.rate.set(rate);
        if let Some(position) = imp.position() {
            self.seek_to(position);
        }
    }

    /// Shows the next or previous frame, pausing first
    pub fn step_frame(&self, forward: bool) {
        let imp = self.imp();
        let Some(playbin) = imp.playbin.get() else {
            return;
        };
        if self.is_playing() {
            self.set_playing(false);
        }
        if forward {
            playbin.send_event(gst::event::Step::new(
                gst::format::Buffers::ONE,
                1.,
                true,
                false,
            ));
            glib::timeout_add_local_once(
                Duration::from_millis(100),
                clone!(
                    #[weak(rename_to = obj)]
                    self,
                    move || {
                        obj.imp().update_position();
                        glib::spawn_future_local(async move {
                            obj.imp().show_frame_in_image().await
                        });
                    }
                ),
            );
        } else if let Some(position) = imp.position() {
            self.seek_to(position.saturating_sub(self.frame_duration()));
        }
    }

    /// Length of one frame, from the video's frame rate
    fn frame_duration(&self) -> gst::ClockTime {
        let fps = self
            .imp()
            .video_sink
            .get()
            .and_then(|sink| sink.static_pad("sink"))
            .and_then(|pad| pad.current_caps())
            .and_then(|caps| gst_video::VideoInfo::from_caps(&caps).ok())
            .map(|info| info.fps())
            .filter(|fps| fps.numer() > 0 && fps.denom() > 0);
        match fps {
            Some(fps) => gst::ClockTime::from_nseconds(
                gst::ClockTime::SECOND.nseconds() * fps.denom() as u64 / fps.numer() as u64,
            ),
            None => gst::ClockTime::from_mseconds(40),
        }
    }

    /// The frame on screen, as a texture
    pub fn frame_texture(&self) -> Option<gdk::Texture> {
        let imp = self.imp();
        let paintable = imp.picture.paintable()?.current_image();
        let (width, height) = (
            paintable.intrinsic_width(),
            paintable.intrinsic_height(),
        );
        if width <= 0 || height <= 0 {
            return None;
        }
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, width as f64, height as f64);
        let node = snapshot.to_node()?;
        let renderer = imp.picture.native()?.renderer()?;
        Some(renderer.render_texture(
            node,
            Some(&graphene::Rect::new(0., 0., width as f32, height as f32)),
        ))
    }

    pub fn connect_error<F: Fn(&Self, String, String) + 'static>(&self, f: F) {
        self.connect_closure(
            "error",
            false,
            glib::closure_local!(move |obj: &Self, description: String, details: String| {
                f(obj, description, details)
            }),
        );
    }

    pub fn connect_ended<F: Fn(&Self) + 'static>(&self, f: F) {
        self.connect_closure(
            "ended",
            false,
            glib::closure_local!(move |obj: &Self| f(obj)),
        );
    }
}

/// A frame of `file` at `position`, or a tenth into the video, about
/// `width` pixels wide; decoded on a thread
pub async fn video_frame(
    file: &gio::File,
    position: Option<gst::ClockTime>,
    width: u32,
) -> anyhow::Result<gdk::Texture> {
    let uri = file.uri().to_string();
    let (data, width, height, stride) =
        gio::spawn_blocking(move || grab_frame(&uri, position, width))
            .await
            .map_err(|_| anyhow::anyhow!("Frame thread failed"))??;
    Ok(gdk::MemoryTexture::new(
        width,
        height,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(data),
        stride,
    )
    .upcast())
}

/// Pixels, width, height and stride of an RGBA frame
fn grab_frame(
    uri: &str,
    position: Option<gst::ClockTime>,
    width: u32,
) -> anyhow::Result<(Vec<u8>, i32, i32, usize)> {
    let fakesink = || gst::ElementFactory::make("fakesink").build();
    let playbin = gst::ElementFactory::make("playbin3")
        .property("uri", uri)
        .property("audio-sink", fakesink()?)
        .property("video-sink", fakesink()?)
        .build()?;

    let result = (|| {
        let timeout = gst::ClockTime::from_seconds(5);
        playbin.set_state(gst::State::Paused)?;
        let (state, ..) = playbin.state(timeout);
        state?;

        let position = position.or_else(|| {
            playbin
                .query_duration::<gst::ClockTime>()
                .map(|duration| duration / 10)
        });
        if let Some(position) = position {
            playbin.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, position)?;
            let (state, ..) = playbin.state(timeout);
            state?;
        }

        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("width", width as i32)
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build();
        let sample = playbin
            .emit_by_name::<Option<gst::Sample>>("convert-sample", &[&caps])
            .ok_or_else(|| anyhow::anyhow!("No frame"))?;
        let info = gst_video::VideoInfo::from_caps(
            sample.caps().ok_or_else(|| anyhow::anyhow!("No caps"))?,
        )?;
        let buffer = sample.buffer().ok_or_else(|| anyhow::anyhow!("No buffer"))?;
        let map = buffer.map_readable()?;
        anyhow::Ok((
            map.as_slice().to_vec(),
            info.width() as i32,
            info.height() as i32,
            info.stride()[0] as usize,
        ))
    })();

    let _ = playbin.set_state(gst::State::Null);
    result
}

/// What the properties show about a video
#[derive(Debug, Default)]
pub struct VideoInfo {
    pub size: Option<(u32, u32)>,
    pub duration: Option<gst::ClockTime>,
    pub frame_rate: Option<f64>,
    pub container: Option<String>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
}

/// Reads `uri` with GStreamer's discoverer; blocks
pub fn video_info(uri: &str) -> Option<VideoInfo> {
    use gst_pbutils::prelude::DiscovererStreamInfoExt;

    let discoverer = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(5)).ok()?;
    let info = discoverer.discover_uri(uri).ok()?;
    let description = |caps: Option<gst::Caps>| {
        caps.map(|caps| gst_pbutils::pb_utils_get_codec_description(&caps).to_string())
    };

    let mut result = VideoInfo {
        duration: info.duration(),
        container: info
            .stream_info()
            .and_then(|stream| description(stream.caps())),
        ..Default::default()
    };
    if let Some(video) = info.video_streams().first() {
        result.size = Some((video.width(), video.height()));
        let fps = video.framerate();
        if fps.numer() > 0 && fps.denom() > 0 {
            result.frame_rate = Some(fps.numer() as f64 / fps.denom() as f64);
        }
        result.video_codec = description(video.caps());
    }
    if let Some(audio) = info.audio_streams().first() {
        result.audio_codec = description(audio.caps());
    }
    Some(result)
}

/// Volume of all videos, from 0 to 1
pub fn volume() -> f64 {
    if MUTED.get() { 0. } else { VOLUME.get() }
}

/// `1:05` or `1:02:05`
pub fn format_time(time: gst::ClockTime) -> String {
    let seconds = time.seconds();
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Where paused frames are written for the image
fn frame_path(file: Option<&gio::File>) -> PathBuf {
    let dir = glib::user_cache_dir().join("blink").join("frames");
    let _ = std::fs::create_dir_all(&dir);
    let key = file.map(|f| f.uri().to_string()).unwrap_or_default();
    let hash = glib::compute_checksum_for_string(glib::ChecksumType::Sha1, &key)
        .map(|x| x.to_string())
        .unwrap_or_default();
    dir.join(format!("{hash}.png"))
}

/// Remembered playback positions, one per video
fn positions_path() -> PathBuf {
    glib::user_data_dir().join("blink").join("video-positions")
}

fn position_key(file: &gio::File) -> String {
    glib::compute_checksum_for_string(glib::ChecksumType::Sha1, file.uri().as_str())
        .map(|x| x.to_string())
        .unwrap_or_default()
}

const POSITIONS_GROUP: &str = "positions";

fn saved_position(file: Option<&gio::File>) -> Option<f64> {
    let file = file?;
    let key_file = glib::KeyFile::new();
    key_file
        .load_from_file(positions_path(), glib::KeyFileFlags::NONE)
        .ok()?;
    key_file.double(POSITIONS_GROUP, &position_key(file)).ok()
}

fn store_position(file: Option<&gio::File>, position: Option<f64>) {
    let Some(file) = file else {
        return;
    };
    let path = positions_path();
    let key_file = glib::KeyFile::new();
    let _ = key_file.load_from_file(&path, glib::KeyFileFlags::NONE);
    let key = position_key(file);
    match position {
        Some(position) => key_file.set_double(POSITIONS_GROUP, &key, position),
        None => {
            if key_file.remove_key(POSITIONS_GROUP, &key).is_err() {
                return;
            }
        }
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = key_file.save_to_file(&path) {
        tracing::warn!("Could not save the video position: {err}");
    }
}

fn forget_position(file: Option<&gio::File>) {
    store_position(file, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_formats() {
        assert_eq!(format_time(gst::ClockTime::from_seconds(5)), "0:05");
        assert_eq!(format_time(gst::ClockTime::from_seconds(65)), "1:05");
        assert_eq!(format_time(gst::ClockTime::from_seconds(3725)), "1:02:05");
    }
}
