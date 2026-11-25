// Copyright (c) 2026 Blick contributors
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
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
/// Paused frames go to the image this long after the last pause or seek
const FRAME_HANDOFF_DELAY: Duration = Duration::from_millis(700);
/// Width of the frame preview over the seek bar
const PREVIEW_WIDTH: i32 = 112;
/// Space between the seek bar and the frame preview above it
const PREVIEW_GAP: i32 = 12;
/// Positions closer than this to the start or end are not remembered
const RESUME_MARGIN: f64 = 5.;
/// Subtitle files that are picked up next to a video
pub const SUBTITLE_EXTENSIONS: [&str; 5] = ["srt", "vtt", "ass", "ssa", "sub"];
/// Subtitle text heights for small, medium and large, as parts of the
/// video's height
const SUBTITLE_SIZES: [(&str, f64); 3] = [("small", 0.045), ("medium", 0.06), ("large", 0.08)];
/// A subtitle's text, if it is Pango markup, and how long it shows
type Subtitle = (String, bool, Option<gst::ClockTime>);

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
        pub(super) title_label: gtk::Label,
        pub(super) sound_button: gtk::MenuButton,
        /// The end time shows the time left instead of the length
        pub(super) show_remaining: Cell<bool>,
        /// Played to the end; playing again starts over
        pub(super) ended: Cell<bool>,
        /// Subtitle delay in nanoseconds
        pub(super) subtitle_offset: Cell<i64>,
        /// Hands the paused frame to the image once pausing and seeking
        /// settled, and whether that is underway
        pub(super) frame_timeout: RefCell<Option<glib::SourceId>>,
        pub(super) handing_off: Cell<bool>,
        /// Size read from the file before the first frame is decoded
        pub(super) size_hint: Cell<Option<(i32, i32)>>,
        /// Asked to play or pause before the pipeline was ready
        pub(super) pending_playing: Cell<Option<bool>>,
        /// Counts seeks, so only the newest one runs
        pub(super) seek_generation: Arc<AtomicU64>,
        /// Room kept below the controls for the folder strip
        pub(super) bottom_inset: Cell<i32>,
        /// Tell if the pointer rests on the buttons or the bottom bar
        pub(super) hover_areas: RefCell<Vec<gtk::EventControllerMotion>>,
        /// Pointer movement anywhere on the page, also over a paused frame
        pub(super) page_motion: RefCell<Option<gtk::EventControllerMotion>>,
        /// Where the pointer was, as GTK also reports motion when only the
        /// widgets under it change
        pub(super) last_pointer: Cell<Option<(f64, f64)>>,
        /// The controls fade by their opacity
        pub(super) controls_shown: Cell<bool>,
        pub(super) fades: RefCell<Vec<adw::TimedAnimation>>,
        pub(super) play_button: gtk::Button,
        pub(super) position_label: gtk::Label,
        pub(super) duration_label: gtk::Label,
        pub(super) seek: gtk::Scale,
        pub(super) tracks_button: RefCell<Option<gtk::MenuButton>>,
        /// Shows the speed, opens the speeds
        pub(super) speed_button: gtk::MenuButton,
        /// Subtitles drawn over the picture, sharp at any size
        pub(super) subtitle_label: gtk::Label,
        /// Hides the subtitle when its time is over
        pub(super) subtitle_timeout: RefCell<Option<glib::SourceId>>,
        /// A new subtitle track was chosen; its text is fetched for the
        /// current position once the switch is done
        pub(super) refresh_subtitle: Cell<bool>,
        /// The subtitle's time ran out while paused
        pub(super) subtitle_expired: Cell<bool>,
        /// Text height as a part of the video's height, and in pixels
        pub(super) subtitle_scale: Cell<f64>,
        pub(super) subtitle_pixels: Cell<i32>,
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
        pub(super) inhibit_cookie: Cell<u32>,
        /// The frame shown in the image belongs to this position
        pub(super) frame_position: Cell<Option<gst::ClockTime>>,
        pub(super) missing_codec: RefCell<Option<String>>,

        /// Frame preview over the seek bar
        pub(super) preview: gtk::Box,
        /// The page's overlay, where the frame preview floats
        pub(super) overlay: glib::WeakRef<gtk::Overlay>,
        pub(super) preview_picture: gtk::Picture,
        pub(super) preview_label: gtk::Label,
        /// Where the pointer is on the seek bar, and if a frame is loading
        pub(super) preview_wanted: Cell<Option<gst::ClockTime>>,
        pub(super) preview_loading: Cell<bool>,

        /// Tracks of the video and which are chosen; no text stream means
        /// subtitles are off
        pub(super) streams: RefCell<Option<gst::StreamCollection>>,
        pub(super) audio_stream: RefCell<Option<String>>,
        pub(super) text_stream: RefCell<Option<String>>,
        /// Subtitle file loaded next to the video, chosen when it appears
        pub(super) subtitle_file: RefCell<Option<gio::File>>,
        pub(super) select_new_subtitle: Cell<bool>,
        /// Position and playing state to restore after reloading
        pub(super) restore: Cell<Option<(gst::ClockTime, bool)>>,
        /// Pages of the settings menu
        /// Pages of the settings menu, and the subtitle size and delay rows
        pub(super) settings_stack: gtk::Stack,
        pub(super) size_row: RefCell<Option<gtk::Box>>,
        pub(super) delay_row: RefCell<Option<gtk::Box>>,
        pub(super) delay_label: gtk::Label,
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
                    // The controls show or hide
                    Signal::builder("controls-shown")
                        .param_types([bool::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.rate.set(1.);
            self.subtitle_scale.set(SUBTITLE_SIZES[1].1);
            self.build_controls();
        }

        fn dispose(&self) {
            self.save_position();
            let _ = std::fs::remove_file(frame_path(self.file.borrow().as_ref()));
            self.uninhibit();
            if let Some(source) = self.tick.take() {
                source.remove();
            }
            if let Some(source) = self.hide_timeout.take() {
                source.remove();
            }
            if let Some(source) = self.subtitle_timeout.take() {
                source.remove();
            }
            if let Some(source) = self.frame_timeout.take() {
                source.remove();
            }
            self.bus_watch.take();
            if let Some(playbin) = self.playbin.get() {
                set_state_in_background(playbin, gst::State::Null);
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

            // Laid out like GNOME's video player: skip and play buttons in
            // the middle, title, sound, settings, seek bar and times at the
            // bottom, over a shade
            self.play_button.set_icon_name("media-playback-start-symbolic");
            self.play_button.update_property(&[gtk::accessible::Property::Label(&gettext("Play"))]);
            self.play_button.set_widget_name("play-button");
            self.play_button.set_margin_start(12);
            self.play_button.set_margin_end(12);
            self.play_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.toggle_playing()
            ));

            let backwards = gtk::Button::from_icon_name("glance-skip-back-symbolic");
            backwards.set_widget_name("backwards-button");
            backwards.update_property(&[gtk::accessible::Property::Label(&gettext("Go Back 10 Seconds"))]);
            backwards.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.skip(false)
            ));
            let forwards = gtk::Button::from_icon_name("glance-skip-forward-symbolic");
            forwards.set_widget_name("forwards-button");
            forwards.update_property(&[gtk::accessible::Property::Label(&gettext("Go Forward 10 Seconds"))]);
            forwards.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.skip(true)
            ));

            let controls_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            controls_box.set_halign(gtk::Align::Center);
            controls_box.set_valign(gtk::Align::Center);
            // Centred below the header, as in GNOME's video player
            controls_box.set_margin_top(48);
            controls_box.append(&backwards);
            controls_box.append(&self.play_button);
            controls_box.append(&forwards);

            self.title_label.set_hexpand(true);
            self.title_label.set_halign(gtk::Align::Start);
            self.title_label.set_margin_start(12);
            self.title_label.set_margin_end(12);
            self.title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            self.title_label.add_css_class("title-4");
            self.title_label.add_css_class("overlaid");

            self.seek.set_hexpand(true);
            self.seek.set_draw_value(false);
            self.seek.set_range(0., 1.);
            self.seek.add_css_class("highlighted");
            self.seek.add_css_class("overlaid");
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

            // Times under the seek bar; the end one switches between the
            // length and the time left
            self.position_label.set_hexpand(true);
            self.position_label.set_halign(gtk::Align::Start);
            self.position_label.set_margin_start(12);
            self.position_label.set_margin_end(3);
            let end_button = gtk::Button::new();
            end_button.set_child(Some(&self.duration_label));
            end_button.set_widget_name("end-timestamp-button");
            end_button.set_hexpand(true);
            end_button.set_halign(gtk::Align::End);
            end_button.set_margin_start(3);
            end_button.update_property(&[gtk::accessible::Property::Label(&gettext(
                "Toggle Duration/Remaining",
            ))]);
            end_button.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| {
                    let imp = obj.imp();
                    imp.show_remaining.set(!imp.show_remaining.get());
                    imp.update_position();
                }
            ));
            for widget in [self.position_label.upcast_ref::<gtk::Widget>(), end_button.upcast_ref()] {
                widget.add_css_class("caption-heading");
                widget.add_css_class("numeric");
                widget.add_css_class("timestamp");
                widget.add_css_class("overlaid");
            }
            self.position_label.set_label("0:00");
            self.duration_label.set_label("0:00");
            let timestamp_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            timestamp_box.set_valign(gtk::Align::End);
            timestamp_box.set_margin_bottom(3);
            timestamp_box.append(&self.position_label);
            timestamp_box.append(&end_button);

            // Volume: mute toggle and a slider in a popover
            // Vertical, rising from the sound button
            self.volume_scale.set_orientation(gtk::Orientation::Vertical);
            self.volume_scale.set_inverted(true);
            self.volume_scale.set_size_request(-1, 140);
            self.volume_scale.set_halign(gtk::Align::Center);
            self.volume_scale.set_range(0., 1.);
            self.volume_scale.set_increments(0.2, 0.2);
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

            self.mute_button.set_icon_name("glance-volume-muted-symbolic");
            self.mute_button
                .update_property(&[gtk::accessible::Property::Label(&gettext("Mute/Unmute"))]);
            self.mute_button.set_active(MUTED.get());
            self.mute_button.add_css_class("circular");
            self.mute_button.add_css_class("flat");
            self.mute_button.connect_toggled(clone!(
                #[weak]
                obj,
                move |button| {
                    MUTED.set(button.is_active());
                    obj.imp().apply_volume();
                }
            ));

            let volume_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
            volume_box.set_margin_top(6);
            volume_box.set_margin_bottom(2);
            volume_box.append(&self.volume_scale);
            volume_box.append(&self.mute_button);
            let volume_popover = gtk::Popover::new();
            volume_popover.set_child(Some(&volume_box));
            self.sound_button.set_icon_name("glance-volume-high-symbolic");
            self.sound_button.set_valign(gtk::Align::Center);
            self.sound_button.set_direction(gtk::ArrowType::Up);
            self.sound_button.set_popover(Some(&volume_popover));
            self.sound_button
                .update_property(&[gtk::accessible::Property::Label(&gettext("Adjust Volume"))]);

            // Speed and looping, in the settings menu
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
                        if let Some(label) = obj.imp().speed_button.child().and_downcast::<gtk::Label>() {
                            label.set_label(&speed_label(rate));
                        }
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
                    action.set_state(&looping.to_variant());
                    obj.imp().looping.set(looping);
                }
            ));
            self.actions.add_action(&looping);

            let settings = self.build_tracks_button();
            self.tracks_button.replace(Some(settings.clone()));
            self.build_speed_button();

            let repeat = gtk::ToggleButton::new();
            repeat.set_icon_name("glance-repeat-symbolic");
            repeat.set_action_name(Some("video.loop"));
            repeat.set_tooltip_text(Some(&gettext("Repeat")));
            // Dimmed when off, bright with a dot when on
            repeat.add_css_class("repeat");

            for button in [
                backwards.upcast_ref::<gtk::Widget>(),
                self.play_button.upcast_ref(),
                forwards.upcast_ref(),
                self.sound_button.upcast_ref(),
                settings.upcast_ref(),
                self.speed_button.upcast_ref(),
                repeat.upcast_ref(),
            ] {
                button.add_css_class("circular");
                button.add_css_class("highlighted");
                button.add_css_class("overlaid");
                button.set_valign(gtk::Align::Center);
            }
            end_button.add_css_class("highlighted");

            let header = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            header.set_widget_name("bottom-overlay-box");
            header.set_valign(gtk::Align::End);
            header.append(&self.title_label);
            header.append(&settings);
            header.append(&self.speed_button);
            header.append(&repeat);
            header.append(&self.sound_button);

            let bottom = gtk::Box::new(gtk::Orientation::Vertical, 0);
            bottom.append(&header);
            bottom.append(&self.seek);
            bottom.append(&timestamp_box);

            let toolbar = gtk::Box::new(gtk::Orientation::Vertical, 0);
            toolbar.set_margin_start(12);
            toolbar.set_margin_end(12);
            toolbar.set_margin_bottom(12);
            toolbar.set_valign(gtk::Align::End);
            toolbar.append(&bottom);

            // The play buttons sit in the middle of the whole page
            let layer = gtk::Overlay::new();
            layer.set_child(Some(&toolbar));
            layer.add_overlay(&controls_box);
            layer.insert_action_group("video", Some(&self.actions));

            let clamp = adw::Clamp::new();
            clamp.set_maximum_size(1920);
            clamp.set_tightening_threshold(1920);
            clamp.set_child(Some(&layer));
            clamp.add_css_class("shade");
            clamp.add_css_class("video-player");

            self.controls.set_child(Some(&clamp));
            self.controls
                .set_transition_type(gtk::RevealerTransitionType::None);
            self.controls.set_reveal_child(true);
            self.controls.set_can_focus(false);
            self.controls_shown.set(true);
            let target = adw::PropertyAnimationTarget::new(&self.controls, "opacity");
            self.fades
                .borrow_mut()
                .push(adw::TimedAnimation::new(&self.controls, 1., 1., 200, target));

            for button in [&settings, &self.speed_button, &self.sound_button] {
                button.connect_active_notify(clone!(
                    #[weak]
                    obj,
                    move |button| {
                        if button.is_active() {
                            keep_in_window(button);
                        } else {
                            obj.imp().schedule_hide();
                        }
                    }
                ));
                if let Some(popover) = button.popover() {
                    // Shifted to stay inside the window, so no arrow
                    popover.set_has_arrow(false);
                    popover.connect_show(clone!(
                        #[weak]
                        button,
                        move |_| keep_in_window(&button)
                    ));
                }
            }

            // Moving the pointer on the page shows the controls, see `new()`;
            // they stay while it rests on the buttons or the bottom bar
            for widget in [controls_box.upcast_ref::<gtk::Widget>(), bottom.upcast_ref()] {
                let motion = gtk::EventControllerMotion::new();
                widget.add_controller(motion.clone());
                self.hover_areas.borrow_mut().push(motion);
            }

            // A double click toggles fullscreen and a double tap on the left
            // or right third skips; playing and pausing is for the play
            // button and Space. The shade gets the clicks while the
            // controls show.
            for widget in [self.picture.upcast_ref::<gtk::Widget>(), clamp.upcast_ref()] {
                let click = gtk::GestureClick::new();
                click.connect_pressed(clone!(
                    #[weak]
                    obj,
                    move |gesture, n_press, x, y| {
                        // Clicks on the buttons and the seek bar are theirs
                        let on_control = gesture
                            .widget()
                            .and_then(|widget| widget.pick(x, y, gtk::PickFlags::DEFAULT))
                            .is_some_and(|picked| {
                                picked.is::<gtk::Button>()
                                    || picked.is::<gtk::Scale>()
                                    || picked.ancestor(gtk::Button::static_type()).is_some()
                                    || picked.ancestor(gtk::Scale::static_type()).is_some()
                            });
                        if !on_control {
                            obj.imp().clicked(gesture, n_press, x);
                        }
                    }
                ));
                widget.add_controller(click);
            }

            self.setup_shortcuts();
        }

        /// Hovering the seek bar shows the frame at that point
        fn setup_preview(&self) {
            let obj = self.obj();

            // A small frosted card in the page, not a popup, so showing it
            // never takes the pointer off the seek bar
            self.preview_picture
                .set_size_request(PREVIEW_WIDTH, PREVIEW_WIDTH * 9 / 16);
            self.preview_picture
                .set_content_fit(gtk::ContentFit::Contain);
            self.preview_label.add_css_class("numeric");
            self.preview_label.add_css_class("caption");
            self.preview.set_orientation(gtk::Orientation::Vertical);
            self.preview.set_spacing(4);
            self.preview.add_css_class("video-preview");
            // The frame's own size would make the card large
            let clamp = adw::Clamp::new();
            clamp.set_maximum_size(PREVIEW_WIDTH);
            clamp.set_tightening_threshold(PREVIEW_WIDTH);
            clamp.set_child(Some(&self.preview_picture));
            self.preview_picture.set_can_shrink(true);
            self.preview.append(&clamp);
            self.preview.append(&self.preview_label);
            self.preview.set_halign(gtk::Align::Start);
            self.preview.set_valign(gtk::Align::End);
            self.preview.set_can_target(false);
            self.preview.set_visible(false);

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
                    imp.preview.set_visible(false);
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
            // A hidden widget measures as 0 wide, so it shows first
            self.preview.set_visible(true);
            // Centred above the pointer, inside the page
            if let Some(overlay) = self.overlay.upgrade()
                && let Some(point) =
                    self.seek.compute_point(&overlay, &graphene::Point::new(x as f32, 0.))
            {
                // The measured width includes the margin set last time
                let (_, width, _, _) = self.preview.measure(gtk::Orientation::Horizontal, -1);
                let width = width - self.preview.margin_start() - self.preview.margin_end();
                let max = (overlay.width() - width - 8).max(8);
                let start = (point.x() as i32 - width / 2).clamp(8, max);
                self.preview.set_margin_start(start);
                // Above the seek bar
                self.preview
                    .set_margin_bottom(overlay.height() - point.y() as i32 + PREVIEW_GAP);
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
                    let texture = video_frame(&file, Some(position), PREVIEW_WIDTH as u32 * 2).await;
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

        /// Settings button: subtitles, audio tracks, speed and looping
        fn build_tracks_button(&self) -> gtk::MenuButton {
            let obj = self.obj();

            let subtitle = gio::SimpleAction::new_stateful(
                "subtitle",
                Some(glib::VariantTy::STRING),
                &"".to_variant(),
            );
            subtitle.connect_activate(clone!(
                #[weak]
                obj,
                move |action, target| {
                    let Some(id) = target.and_then(|x| x.get::<String>()) else {
                        return;
                    };
                    action.set_state(&id.to_variant());
                    let imp = obj.imp();
                    imp.hide_subtitle();
                    imp.refresh_subtitle.set(!id.is_empty());
                    imp.text_stream
                        .replace((!id.is_empty()).then_some(id));
                    imp.select_streams();
                }
            ));
            self.actions.add_action(&subtitle);

            let audio = gio::SimpleAction::new_stateful(
                "audio",
                Some(glib::VariantTy::STRING),
                &"".to_variant(),
            );
            audio.connect_activate(clone!(
                #[weak]
                obj,
                move |action, target| {
                    let Some(id) = target.and_then(|x| x.get::<String>()) else {
                        return;
                    };
                    action.set_state(&id.to_variant());
                    obj.imp().audio_stream.replace(Some(id));
                    obj.imp().select_streams();
                }
            ));
            self.actions.add_action(&audio);

            let size = gio::SimpleAction::new_stateful(
                "subtitle-size",
                Some(glib::VariantTy::STRING),
                &"medium".to_variant(),
            );
            size.connect_activate(clone!(
                #[weak]
                obj,
                move |action, target| {
                    let Some(name) = target.and_then(|x| x.get::<String>()) else {
                        return;
                    };
                    action.set_state(&name.to_variant());
                    obj.imp().apply_subtitle_size(&name);
                }
            ));
            self.actions.add_action(&size);

            let open = gio::SimpleAction::new("open-subtitles", None);
            open.connect_activate(clone!(
                #[weak]
                obj,
                move |_, _| {
                    glib::spawn_future_local(async move { obj.choose_subtitles().await });
                }
            ));
            self.actions.add_action(&open);

            // Subtitle delay, in steps of 0.1 s
            self.delay_label.add_css_class("numeric");
            self.delay_label.set_width_chars(6);
            let earlier = gtk::Button::from_icon_name("minus-large-symbolic");
            earlier.update_property(&[gtk::accessible::Property::Label(&gettext("Show Subtitles Earlier"))]);
            let later = gtk::Button::from_icon_name("plus-large-symbolic");
            later.update_property(&[gtk::accessible::Property::Label(&gettext("Show Subtitles Later"))]);
            for button in [&earlier, &later] {
                button.add_css_class("flat");
                button.add_css_class("circular");
            }
            earlier.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.imp().shift_subtitles(-100)
            ));
            later.connect_clicked(clone!(
                #[weak]
                obj,
                move |_| obj.imp().shift_subtitles(100)
            ));
            let delay_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            delay_buttons.append(&earlier);
            delay_buttons.append(&self.delay_label);
            delay_buttons.append(&later);
            self.update_delay_label(0);
            self.delay_row
                .replace(Some(setting_row(&gettext("Delay"), &delay_buttons)));

            // Subtitle size
            let size_group = adw::ToggleGroup::new();
            size_group.add_css_class("round");
            for (name, label) in [
                // Translators: Small, medium and large subtitles
                ("small", gettext("S")),
                ("medium", gettext("M")),
                ("large", gettext("L")),
            ] {
                let toggle = adw::Toggle::new();
                toggle.set_name(Some(name));
                toggle.set_label(Some(&label));
                size_group.add(toggle);
            }
            size_group.set_active_name(Some("medium"));
            size_group.connect_active_name_notify(clone!(
                #[weak]
                obj,
                move |group| {
                    if let Some(name) = group.active_name() {
                        obj.imp().actions.activate_action("subtitle-size", Some(&name.to_variant()));
                    }
                }
            ));
            self.size_row
                .replace(Some(setting_row(&gettext("Size"), &size_group)));

            // The menu: subtitle tracks and settings, and a page for audio
            // tracks; filled whenever it opens
            self.settings_stack
                .set_transition_type(gtk::StackTransitionType::SlideLeftRight);
            self.settings_stack.set_transition_duration(200);
            self.settings_stack.set_vhomogeneous(false);
            self.settings_stack.set_interpolate_size(true);
            self.settings_stack.set_size_request(260, -1);
            let popover = gtk::Popover::new();
            popover.add_css_class("menu");
            popover.add_css_class("video-settings");
            popover.set_child(Some(&self.settings_stack));
            popover.connect_show(clone!(
                #[weak]
                obj,
                move |_| {
                    let imp = obj.imp();
                    imp.fill_settings();
                    imp.settings_stack
                        .set_visible_child_full("main", gtk::StackTransitionType::None);
                }
            ));
            let button = gtk::MenuButton::new();
            button.set_icon_name("glance-subtitles-symbolic");
            button.set_halign(gtk::Align::End);
            button.set_direction(gtk::ArrowType::Up);
            button.set_popover(Some(&popover));
            button.set_tooltip_text(Some(&gettext("Subtitles")));
            button
        }

        /// Speed button showing the current speed, with a list of speeds
        fn build_speed_button(&self) {
            let obj = self.obj();
            let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let popover = gtk::Popover::new();
            popover.add_css_class("menu");
            popover.add_css_class("video-settings");
            popover.set_child(Some(&list));
            popover.connect_show(clone!(
                #[weak]
                obj,
                #[weak]
                list,
                move |popover| {
                    while let Some(child) = list.first_child() {
                        list.remove(&child);
                    }
                    let rate = obj.rate();
                    for speed in SPEEDS {
                        let row = check_row(&speed_label(speed), (speed - rate).abs() < 0.01);
                        row.connect_clicked(clone!(
                            #[weak]
                            obj,
                            #[weak]
                            popover,
                            move |_| {
                                obj.imp()
                                    .actions
                                    .activate_action("speed", Some(&speed.to_string().to_variant()));
                                popover.popdown();
                            }
                        ));
                        list.append(&row);
                    }
                }
            ));
            list.set_size_request(160, -1);
            let button = &self.speed_button;
            // A label child, so no menu arrow next to it
            let label = gtk::Label::new(Some(&speed_label(1.)));
            label.add_css_class("speed");
            button.set_child(Some(&label));
            button.set_halign(gtk::Align::End);
            button.set_direction(gtk::ArrowType::Up);
            button.set_popover(Some(&popover));
            button.set_tooltip_text(Some(&gettext("Speed")));
        }

        /// Builds the settings menu's pages with the current choices
        fn fill_settings(&self) {
            let obj = self.obj();
            let stack = &self.settings_stack;
            while let Some(child) = stack.first_child() {
                stack.remove(&child);
            }
            for row in [self.size_row.borrow().clone(), self.delay_row.borrow().clone()]
                .into_iter()
                .flatten()
            {
                if let Some(parent) = row.parent().and_downcast::<gtk::Box>() {
                    parent.remove(&row);
                }
            }

            let state = |name: &str| {
                self.actions
                    .lookup_action(name)
                    .and_then(|action| action.state())
                    .and_then(|state| state.get::<String>())
                    .unwrap_or_default()
            };
            let close = clone!(
                #[weak]
                stack,
                move || {
                    if let Some(popover) = stack.ancestor(gtk::Popover::static_type()) {
                        popover.downcast::<gtk::Popover>().unwrap().popdown();
                    }
                }
            );
            let page = |name: &str, title: Option<&str>| {
                let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
                if let Some(title) = title {
                    let back = menu_row(title, None, true);
                    back.add_css_class("menu-back");
                    back.connect_clicked(clone!(
                        #[weak]
                        stack,
                        move |_| stack.set_visible_child_name("main")
                    ));
                    page.append(&back);
                    page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                }
                stack.add_named(&page, Some(name));
                page
            };
            // A row that picks `target` for `action`, with a check when chosen
            let choice = |label: &str, action: &'static str, target: String, chosen: bool| {
                let row = check_row(label, chosen);
                row.connect_clicked(clone!(
                    #[weak]
                    obj,
                    #[strong]
                    close,
                    move |_| {
                        obj.imp().actions.activate_action(action, Some(&target.to_variant()));
                        close();
                    }
                ));
                row
            };
            let opens = |label: &str, value: &str, name: &'static str| {
                let row = menu_row(label, Some(value), false);
                row.connect_clicked(clone!(
                    #[weak]
                    stack,
                    move |_| stack.set_visible_child_name(name)
                ));
                row
            };

            let texts = self.stream_ids(gst::StreamType::TEXT);
            let audios = self.stream_ids(gst::StreamType::AUDIO);
            let subtitle = state("subtitle");
            let audio = self
                .audio_stream
                .borrow()
                .clone()
                .or_else(|| audios.first().map(|(id, _)| id.clone()))
                .unwrap_or_default();
            let name_of = |list: &[(String, String)], id: &str| {
                list.iter().find(|(x, _)| x == id).map(|(_, name)| name.clone())
            };

            // Main page: subtitle tracks, a file, size and delay, then the
            // audio tracks when there is a choice
            let main = page("main", None);
            main.append(&choice(&gettext("Off"), "subtitle", String::new(), subtitle.is_empty()));
            for (id, label) in texts {
                let chosen = id == subtitle;
                main.append(&choice(&label, "subtitle", id, chosen));
            }
            let add = menu_row(&gettext("Add Subtitle File…"), None, false);
            add.connect_clicked(clone!(
                #[weak]
                obj,
                #[strong]
                close,
                move |_| {
                    close();
                    obj.imp().actions.activate_action("open-subtitles", None);
                }
            ));
            main.append(&add);
            main.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            for row in [self.size_row.borrow().clone(), self.delay_row.borrow().clone()]
                .into_iter()
                .flatten()
            {
                main.append(&row);
            }
            if audios.len() > 1 {
                main.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
                main.append(&opens(
                    &gettext("Audio"),
                    &name_of(&audios, &audio).unwrap_or_default(),
                    "audio",
                ));
            }

            if audios.len() > 1 {
                let page = page("audio", Some(&gettext("Audio")));
                for (id, label) in audios {
                    let chosen = id == audio;
                    page.append(&choice(&label, "audio", id, chosen));
                }
            }
        }

        /// Stream ids of `stream_type` with a readable name each
        pub(super) fn stream_ids(&self, stream_type: gst::StreamType) -> Vec<(String, String)> {
            let Some(collection) = self.streams.borrow().clone() else {
                return Vec::new();
            };
            collection
                .iter()
                .filter(|stream| stream.stream_type().contains(stream_type))
                .enumerate()
                .filter_map(|(index, stream)| {
                    Some((stream.stream_id()?.to_string(), stream_label(&stream, index + 1)))
                })
                .collect()
        }

        /// Shows the chosen tracks, also in an open settings menu
        pub(super) fn update_tracks_menu(&self) {
            if let Some(action) = self.actions.lookup_action("subtitle") {
                let id = self.text_stream.borrow().clone().unwrap_or_default();
                action.change_state(&id.to_variant());
            }
            if let Some(action) = self.actions.lookup_action("audio") {
                let id = self.audio_stream.borrow().clone().unwrap_or_default();
                action.change_state(&id.to_variant());
            }
            if self.settings_stack.is_mapped() {
                let page = self.settings_stack.visible_child_name();
                self.fill_settings();
                if let Some(page) = page {
                    self.settings_stack
                        .set_visible_child_full(&page, gtk::StackTransitionType::None);
                }
            }
        }

        /// Plays the chosen audio track and subtitles, with the first video
        pub(super) fn select_streams(&self) {
            let Some(playbin) = self.playbin.get() else {
                return;
            };
            let mut ids: Vec<String> = Vec::new();
            ids.extend(self.stream_ids(gst::StreamType::VIDEO).into_iter().take(1).map(|x| x.0));
            match self.audio_stream.borrow().clone() {
                Some(audio) => ids.push(audio),
                None => ids.extend(
                    self.stream_ids(gst::StreamType::AUDIO).into_iter().take(1).map(|x| x.0),
                ),
            }
            match self.text_stream.borrow().clone() {
                Some(text) => ids.push(text),
                None => self.hide_subtitle(),
            }
            let playbin = playbin.clone();
            in_background(move || {
                playbin.send_event(gst::event::SelectStreams::new(ids.iter().map(|x| x.as_str())));
            });
        }

        pub(super) fn apply_subtitle_size(&self, name: &str) {
            let scale = SUBTITLE_SIZES
                .iter()
                .find(|(n, _)| *n == name)
                .map_or(SUBTITLE_SIZES[1].1, |(_, scale)| *scale);
            self.subtitle_scale.set(scale);
            self.subtitle_pixels.set(0);
            self.subtitle_label.queue_resize();
        }

        /// Shows a subtitle from the text sink, or hides it when empty
        pub(super) fn show_subtitle(&self, (text, markup, duration): Subtitle) {
            if let Some(source) = self.subtitle_timeout.take() {
                source.remove();
            }
            self.subtitle_expired.set(false);
            if text.is_empty() || self.text_stream.borrow().is_none() {
                self.subtitle_label.set_visible(false);
                return;
            }
            if markup {
                self.subtitle_label.set_markup(&text);
            } else {
                self.subtitle_label.set_text(&text);
            }
            self.subtitle_label.set_visible(true);
            let Some(duration) = duration else {
                return;
            };
            let obj = self.obj();
            let milliseconds = duration.mseconds() as f64 / self.rate.get().max(0.1);
            let source = glib::timeout_add_local_once(
                Duration::from_millis(milliseconds as u64),
                clone!(
                    #[weak]
                    obj,
                    move || {
                        let imp = obj.imp();
                        imp.subtitle_timeout.take();
                        // Paused, it stays until playing goes on
                        if imp.playing.get() {
                            imp.subtitle_label.set_visible(false);
                        } else {
                            imp.subtitle_expired.set(true);
                        }
                    }
                ),
            );
            self.subtitle_timeout.replace(Some(source));
        }

        pub(super) fn hide_subtitle(&self) {
            if let Some(source) = self.subtitle_timeout.take() {
                source.remove();
            }
            self.subtitle_label.set_visible(false);
        }

        /// Where the subtitle goes: centred near the bottom of the video,
        /// with text sized to the video
        pub(super) fn subtitle_position(&self, width: i32, height: i32) -> gdk::Rectangle {
            let (video_width, video_height) = match self.obj().video_size() {
                Some((w, h)) => {
                    let zoom = f64::min(width as f64 / w as f64, height as f64 / h as f64);
                    (w as f64 * zoom, h as f64 * zoom)
                }
                None => (width as f64, height as f64),
            };
            let pixels = (video_height * self.subtitle_scale.get()).round().max(12.) as i32;
            if self.subtitle_pixels.replace(pixels) != pixels {
                // Not while being laid out
                let label = self.subtitle_label.clone();
                glib::idle_add_local_once(move || {
                    let attrs = gtk::pango::AttrList::new();
                    attrs.insert(gtk::pango::AttrSize::new_size_absolute(
                        pixels * gtk::pango::SCALE,
                    ));
                    label.set_attributes(Some(&attrs));
                });
            }
            let max_width = (video_width * 0.9) as i32;
            let (_, natural_width, _, _) =
                self.subtitle_label.measure(gtk::Orientation::Horizontal, -1);
            let label_width = natural_width.min(max_width).max(1);
            let (_, label_height, _, _) = self
                .subtitle_label
                .measure(gtk::Orientation::Vertical, label_width);
            let mut bottom = (height as f64 + video_height) / 2. - video_height * 0.06;
            // Above the controls while they show
            if self.controls_shown.get()
                && let Some(overlay) = self.overlay.upgrade()
                && let Some(bounds) = self.title_label.compute_bounds(&overlay)
            {
                bottom = bottom.min(bounds.y() as f64 - 12.);
            }
            let y = bottom - label_height as f64;
            gdk::Rectangle::new((width - label_width) / 2, y as i32, label_width, label_height)
        }

        /// Moves subtitles by `milliseconds`, later for positive values
        fn shift_subtitles(&self, milliseconds: i64) {
            let Some(playbin) = self.playbin.get() else {
                return;
            };
            let offset = self.subtitle_offset.get() + milliseconds * 1_000_000;
            self.subtitle_offset.set(offset);
            let playbin = playbin.clone();
            in_background(move || playbin.set_property("text-offset", offset));
            self.update_delay_label(offset);
        }

        fn update_delay_label(&self, offset_ns: i64) {
            let seconds = offset_ns as f64 / 1e9;
            // Translators: Subtitle delay in seconds, {} is a number like +0.5
            self.delay_label
                .set_label(&gettext_f("{} s", [format!("{seconds:+.1}")]));
        }

        fn setup_shortcuts(&self) {
            let obj = self.obj();
            let shortcuts = gtk::ShortcutController::new();
            shortcuts.set_scope(gtk::ShortcutScope::Global);
            // Before a focused button, so Space plays instead of pressing it
            shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);

            let add = |key: gdk::Key, callback: fn(&super::LpVideoPlayer)| {
                shortcuts.add_shortcut(gtk::Shortcut::new(
                    Some(gtk::KeyvalTrigger::new(key, gdk::ModifierType::empty())),
                    Some(gtk::CallbackAction::new(clone!(
                        #[weak]
                        obj,
                        #[upgrade_or]
                        glib::Propagation::Proceed,
                        move |_, _| {
                            // Typing, like a zoom level, keeps its keys
                            let typing = obj
                                .imp()
                                .picture
                                .root()
                                .and_then(|root| root.focus())
                                .is_some_and(|focus| focus.is::<gtk::Text>() || focus.is::<gtk::Editable>());
                            if !obj.is_active() || typing {
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

            if n_press == 2 {
                let width = self.picture.width() as f64;
                if touch && x < width / 3. {
                    obj.skip(false);
                } else if touch && x > width * 2. / 3. {
                    obj.skip(true);
                } else {
                    let _ = self.picture.activate_action("win.toggle-fullscreen", None);
                }
            } else if n_press == 1 && touch && !self.controls_shown.get() {
                // A tap shows the controls
                self.show_controls();
            }
        }

        pub(super) fn show_controls(&self) {
            self.fade_controls(true);
            self.picture.set_cursor(None);
            self.schedule_hide();
        }

        fn fade_controls(&self, shown: bool) {
            if self.controls_shown.replace(shown) == shown {
                return;
            }
            self.controls.set_can_target(shown);
            self.subtitle_label.queue_resize();
            self.obj().emit_by_name::<()>("controls-shown", &[&shown]);
            let to = if shown { 1. } else { 0. };
            for fade in self.fades.borrow().iter() {
                let from = fade.widget().opacity();
                fade.set_value_from(from);
                fade.set_value_to(to);
                fade.play();
            }
        }

        pub(super) fn schedule_hide(&self) {
            let obj = self.obj();
            if let Some(source) = self.hide_timeout.take() {
                source.remove();
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
                            .hover_areas
                            .borrow()
                            .iter()
                            .any(|motion| motion.contains_pointer());
                        let menu_open = imp.sound_button.is_active()
                            || imp.speed_button.is_active()
                            || imp
                                .tracks_button
                                .borrow()
                                .as_ref()
                                .is_some_and(|b| b.is_active());
                        if !over_controls && !menu_open {
                            imp.fade_controls(false);
                            if imp.playing.get() {
                                imp.picture.set_cursor_from_name(Some("none"));
                            }
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
            let (level, muted) = (VOLUME.get(), MUTED.get());
            let playbin = playbin.clone();
            in_background(move || {
                if let Some(volume) = playbin.dynamic_cast_ref::<gst_audio::StreamVolume>() {
                    use gst_audio::prelude::StreamVolumeExt;
                    volume.set_volume(gst_audio::StreamVolumeFormat::Cubic, level);
                    volume.set_mute(muted);
                }
            });
            let icon = if MUTED.get() || VOLUME.get() <= 0. {
                "glance-volume-muted-symbolic"
            } else if VOLUME.get() < 0.5 {
                "glance-volume-low-symbolic"
            } else {
                "glance-volume-high-symbolic"
            };
            self.mute_button.set_icon_name(icon);
            self.sound_button.set_icon_name(icon);
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
                if self.show_remaining.get() {
                    // Translators: Time left in a video, {} is a time like 1:05
                    self.duration_label.set_label(&gettext_f(
                        "−{}",
                        [format_time(duration.saturating_sub(position))],
                    ));
                } else {
                    self.duration_label.set_label(&format_time(duration));
                }
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
                        self.ended.set(true);
                        obj.emit_by_name::<()>("ended", &[]);
                    }
                }
                gst::MessageView::AsyncDone(_) => {
                    self.update_duration();
                    if let Some((position, playing)) = self.restore.take() {
                        obj.seek_to(position);
                        if playing {
                            obj.set_playing(true);
                        }
                    }
                    if !self.prerolled.replace(true) {
                        self.resume();
                    }
                    if !self.playing.get() {
                        self.update_position();
                        self.schedule_frame_handoff();
                    }
                }
                gst::MessageView::DurationChanged(_) => self.update_duration(),
                gst::MessageView::StreamCollection(msg) => {
                    self.streams.replace(Some(msg.stream_collection()));
                    if self.select_new_subtitle.replace(false) {
                        // The newest text stream is the loaded file
                        let last_text = self.stream_ids(gst::StreamType::TEXT).pop();
                        self.text_stream.replace(last_text.map(|(id, _)| id));
                        self.select_streams();
                    }
                    self.update_tracks_menu();
                }
                gst::MessageView::StreamsSelected(msg) => {
                    let mut audio = None;
                    let mut text = None;
                    for stream in msg.streams() {
                        let id = stream.stream_id().map(|x| x.to_string());
                        if stream.stream_type().contains(gst::StreamType::AUDIO) {
                            audio = id;
                        } else if stream.stream_type().contains(gst::StreamType::TEXT) {
                            text = id;
                        }
                    }
                    let has_text = text.is_some();
                    self.audio_stream.replace(audio);
                    self.text_stream.replace(text);
                    self.update_tracks_menu();
                    // The new track's text shows now, not at its next line
                    if self.refresh_subtitle.take()
                        && has_text
                        && let Some(position) = self.position()
                    {
                        self.obj().seek_to(position);
                    }
                }
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

        /// Hands the paused frame to the image after pausing and seeking
        /// settled, as copying a large frame stalls a moment
        pub(super) fn schedule_frame_handoff(&self) {
            if let Some(source) = self.frame_timeout.take() {
                source.remove();
            }
            let obj = self.obj();
            let source = glib::timeout_add_local_once(
                FRAME_HANDOFF_DELAY,
                clone!(
                    #[weak]
                    obj,
                    move || {
                        let imp = obj.imp();
                        imp.frame_timeout.take();
                        if imp.handing_off.get() {
                            imp.schedule_frame_handoff();
                            return;
                        }
                        glib::spawn_future_local(async move {
                            let imp = obj.imp();
                            imp.handing_off.set(true);
                            imp.show_frame_in_image().await;
                            imp.handing_off.set(false);
                        });
                    }
                ),
            );
            self.frame_timeout.replace(Some(source));
        }

        /// Puts the paused frame into the image, for zoom, live text and
        /// copying
        async fn show_frame_in_image(&self) {
            let obj = self.obj();
            let Some(image) = self.image.borrow().clone() else {
                return;
            };
            if !self.active.get() {
                return;
            }
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
            // Encoding a large frame takes a while, so not on the main thread
            let saved = {
                let path = path.clone();
                gio::spawn_blocking(move || texture.save_to_png(&path).is_ok()).await
            };
            if !matches!(saved, Ok(true)) {
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
        if let Some(name) = file.path().as_deref().and_then(|p| p.file_stem()) {
            imp.title_label.set_label(&name.to_string_lossy());
        }

        overlay.add_overlay(&imp.picture);
        overlay.add_overlay(&imp.controls);
        overlay.add_overlay(&imp.subtitle_label);
        overlay.add_overlay(&imp.preview);
        imp.overlay.set(Some(overlay));
        overlay.set_measure_overlay(&imp.picture, false);

        imp.subtitle_label.set_visible(false);
        imp.subtitle_label.set_can_target(false);
        imp.subtitle_label.set_wrap(true);
        imp.subtitle_label.set_justify(gtk::Justification::Center);
        imp.subtitle_label.add_css_class("video-subtitle");
        overlay.connect_get_child_position(clone!(
            #[weak]
            obj,
            #[upgrade_or]
            None,
            move |overlay, widget| {
                let imp = obj.imp();
                (widget == imp.subtitle_label.upcast_ref::<gtk::Widget>())
                    .then(|| imp.subtitle_position(overlay.width(), overlay.height()))
            }
        ));

        let motion = gtk::EventControllerMotion::new();
        motion.set_propagation_phase(gtk::PropagationPhase::Capture);
        motion.connect_motion(clone!(
            #[weak]
            obj,
            move |_, x, y| {
                let imp = obj.imp();
                let moved = imp
                    .last_pointer
                    .replace(Some((x, y)))
                    .is_none_or(|(old_x, old_y)| (old_x - x).abs() + (old_y - y).abs() > 1.);
                if moved && obj.is_active() {
                    imp.show_controls();
                }
            }
        ));
        overlay.add_controller(motion.clone());
        imp.page_motion.replace(Some(motion));

        // The window waits for the video's size to open at it, but frames
        // only come once it shows, so the size is read from the file first
        let window_shown = gtk::Window::list_toplevels()
            .iter()
            .any(|window| window.is_visible());
        if !window_shown {
            let uri = file.uri().to_string();
            glib::spawn_future_local(clone!(
                #[weak]
                obj,
                async move {
                    let size = gio::spawn_blocking(move || display_size(&uri)).await.ok().flatten();
                    if let Some(size) = size {
                        obj.imp().size_hint.set(Some(size));
                        obj.notify_size_known();
                    }
                }
            ));
        }

        // Setting up the sink takes a while the first time, so only once
        // the page shows, after the window opened
        if imp.picture.is_mapped() {
            obj.start_pipeline(file);
        } else {
            let handler = std::rc::Rc::new(RefCell::new(None));
            let id = imp.picture.connect_map(clone!(
                #[weak]
                obj,
                #[strong]
                handler,
                #[strong]
                file,
                move |picture| {
                    if let Some(id) = handler.take() {
                        picture.disconnect(id);
                    }
                    obj.start_pipeline(&file);
                }
            ));
            handler.replace(Some(id));
        }

        obj
    }

    fn start_pipeline(&self, file: &gio::File) {
        if let Err(err) = self.build_pipeline(file) {
            let details = err.to_string();
            glib::idle_add_local_once(clone!(
                #[weak(rename_to = obj)]
                self,
                move || obj.report_missing_playback(&details)
            ));
        }
    }

    /// Tells the window the video's size, so a window opened for it can
    /// take it
    fn notify_size_known(&self) {
        if let Some(window) = self.imp().picture.root().and_downcast::<crate::widgets::LpWindow>() {
            window.video_size_known(self);
        }
    }

    /// The sink and its paintable are made here, as they belong to the main
    /// thread; the rest of the pipeline is put together on a worker thread,
    /// as linking the GL elements takes a while
    fn build_pipeline(&self, file: &gio::File) -> anyhow::Result<()> {
        let imp = self.imp();

        let sink = gst::ElementFactory::make("gtk4paintablesink").build()?;
        let paintable = sink.property::<gdk::Paintable>("paintable");
        imp.picture.set_paintable(Some(&paintable));
        // The window opens at the video's size once it is known
        paintable.connect_invalidate_size(clone!(
            #[weak(rename_to = obj)]
            self,
            move |_| obj.notify_size_known()
        ));
        let gl = paintable
            .property::<Option<gdk::GLContext>>("gl-context")
            .is_some();

        // Subtitles with the same name as the video
        let subtitle = sidecar_subtitle(file);
        if let Some(subtitle) = &subtitle {
            imp.subtitle_file.replace(Some(subtitle.clone()));
            imp.select_new_subtitle.set(true);
        }
        let uri = file.uri().to_string();
        let subtitle = subtitle.map(|file| file.uri().to_string());
        let video_sink_element = sink.clone();
        // Subtitles come as text and are drawn by GTK, so they stay sharp
        // when the video is shown larger than it is
        let (subtitle_sender, subtitle_receiver) = async_channel::unbounded::<Subtitle>();
        glib::spawn_future_local(clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                while let Ok(subtitle) = subtitle_receiver.recv().await {
                    obj.imp().show_subtitle(subtitle);
                }
            }
        ));

        glib::spawn_future_local(clone!(
            #[weak(rename_to = obj)]
            self,
            async move {
                let built = gio::spawn_blocking(move || -> anyhow::Result<gst::Element> {
                    // Use OpenGL when the sink offers a context
                    let video_sink = if gl {
                        gst::ElementFactory::make("glsinkbin")
                            .property("sink", &sink)
                            .build()
                            .unwrap_or(sink.clone())
                    } else {
                        sink.clone()
                    };
                    let playbin = gst::ElementFactory::make("playbin3")
                        .property("uri", uri.as_str())
                        .property("video-sink", &video_sink)
                        .build()?;
                    // Turned as the video's rotation metadata says, like phone videos
                    if let Ok(flip) = gst::ElementFactory::make("videoflip").build() {
                        flip.set_property_from_str("video-direction", "auto");
                        playbin.set_property("video-filter", &flip);
                    }
                    if let Some(subtitle) = &subtitle {
                        playbin.set_property("suburi", subtitle.as_str());
                    }
                    if let Ok(text_sink) = gst::ElementFactory::make("fakesink")
                        .property("sync", true)
                        .property("signal-handoffs", true)
                        .build()
                    {
                        for signal in ["handoff", "preroll-handoff"] {
                            let sender = subtitle_sender.clone();
                            text_sink.connect(signal, false, move |args| {
                                let buffer = args[1].get::<gst::Buffer>().ok()?;
                                let caps = args[2].get::<gst::Pad>().ok()?.current_caps()?;
                                let structure = caps.structure(0)?;
                                if structure.name() != "text/x-raw" {
                                    return None;
                                }
                                let markup = structure
                                    .get::<&str>("format")
                                    .is_ok_and(|format| format == "pango-markup");
                                let map = buffer.map_readable().ok()?;
                                let text = String::from_utf8_lossy(&map)
                                    .trim_matches(|c: char| c == '\0' || c.is_whitespace())
                                    .to_owned();
                                let _ = sender.try_send((text, markup, buffer.duration()));
                                None
                            });
                        }
                        playbin.set_property("text-sink", &text_sink);
                    }
                    // Keep the pitch when playing faster or slower
                    if let Ok(scaletempo) = gst::ElementFactory::make("scaletempo").build() {
                        playbin.set_property("audio-filter", &scaletempo);
                    }
                    // Shows the first frame
                    playbin.set_state(gst::State::Paused)?;
                    Ok(playbin)
                })
                .await;
                let imp = obj.imp();
                let playbin = match built {
                    Ok(Ok(playbin)) => playbin,
                    Ok(Err(err)) => {
                        obj.report_missing_playback(&err.to_string());
                        return;
                    }
                    Err(_) => {
                        obj.report_missing_playback("The pipeline thread failed");
                        return;
                    }
                };
                // Messages sent meanwhile wait on the bus
                if let Some(bus) = playbin.bus()
                    && let Ok(guard) = bus.add_watch_local(clone!(
                        #[weak]
                        obj,
                        #[upgrade_or]
                        glib::ControlFlow::Break,
                        move |_, message| {
                            obj.imp().on_message(message);
                            glib::ControlFlow::Continue
                        }
                    ))
                {
                    imp.bus_watch.replace(Some(guard));
                }
                let _ = imp.video_sink.set(video_sink_element);
                let _ = imp.playbin.set(playbin);
                imp.apply_volume();
                // Playing was asked for before the pipeline was ready
                if let Some(playing) = imp.pending_playing.take() {
                    obj.set_playing(playing);
                }
            }
        ));
        Ok(())
    }

    fn report_missing_playback(&self, details: &str) {
        let description = gettext(
            "Video playback is not available. The GStreamer “playbin3” and “gtk4paintablesink” elements are needed.",
        );
        self.emit_by_name::<()>("error", &[&description, &details.to_string()]);
    }

    pub fn file(&self) -> Option<gio::File> {
        self.imp().file.borrow().clone()
    }

    /// Stops and takes the picture and controls off `overlay`
    pub fn detach(&self, overlay: &gtk::Overlay) {
        let imp = self.imp();
        self.set_active(false);
        if let Some(playbin) = imp.playbin.get() {
            set_state_in_background(playbin, gst::State::Null);
        }
        overlay.remove_overlay(&imp.picture);
        overlay.remove_overlay(&imp.controls);
        overlay.remove_overlay(&imp.preview);
        if let Some(motion) = imp.page_motion.take() {
            overlay.remove_controller(&motion);
        }
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
            // Volume and mute are shared, and may have changed on another
            // video since this one was loaded
            imp.volume_scale.set_value(VOLUME.get());
            imp.mute_button.set_active(MUTED.get());
            imp.apply_volume();
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

    /// Size of the video's frames, once the first one is decoded
    pub fn video_size(&self) -> Option<(i32, i32)> {
        let imp = self.imp();
        imp.picture
            .paintable()
            .map(|paintable| (paintable.intrinsic_width(), paintable.intrinsic_height()))
            .filter(|(width, height)| *width > 0 && *height > 0)
            .or(imp.size_hint.get())
    }

    /// Room kept free below the controls, for the folder strip
    pub fn bottom_inset(&self) -> i32 {
        self.imp().bottom_inset.get()
    }

    pub fn set_bottom_inset(&self, inset: i32) {
        self.imp().bottom_inset.set(inset);
        // Inside the shade, so the shade still reaches the bottom; the
        // strip's own padding is enough space
        if let Some(toolbar) = self
            .imp()
            .controls
            .child()
            .and_downcast::<adw::Clamp>()
            .and_then(|clamp| clamp.child())
            .and_downcast::<gtk::Overlay>()
            .and_then(|layer| layer.child())
        {
            toolbar.set_margin_bottom((inset - 12).max(12));
        }
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
            // The pipeline is still being made; it starts once ready
            imp.pending_playing.set(Some(playing));
            return;
        };
        if playing && !imp.active.get() {
            return;
        }

        if playing && imp.ended.get() {
            self.seek_to(gst::ClockTime::ZERO);
        }
        let was_playing = imp.playing.replace(playing);
        let state = if playing {
            gst::State::Playing
        } else {
            gst::State::Paused
        };
        set_state_in_background(playbin, state);

        if playing {
            if imp.subtitle_expired.take() {
                imp.subtitle_label.set_visible(false);
            }
            imp.picture.set_visible(true);
            imp.frame_position.set(None);
            if let Some(image) = imp.image.borrow().as_ref() {
                image.zoom_best_fit();
            }
            imp.play_button
                .set_icon_name("media-playback-pause-symbolic");
            imp.play_button.update_property(&[gtk::accessible::Property::Label(&gettext("Pause"))]);
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
            imp.play_button.update_property(&[gtk::accessible::Property::Label(&gettext("Play"))]);
            imp.uninhibit();
            if let Some(source) = imp.tick.take() {
                source.remove();
            }
            if was_playing {
                imp.save_position();
            }
            // Pausing sends no new frame, so the shown one goes to the image;
            // not for a video being left, the copy would only cost time
            if was_playing && imp.active.get() {
                imp.schedule_frame_handoff();
            }
            imp.update_position();
        }
        imp.show_controls();
        crate::mpris::changed();
    }

    pub fn toggle_playing(&self) {
        self.set_playing(!self.is_playing());
    }

    /// Lets the user pick a subtitle file
    async fn choose_subtitles(&self) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some(&gettext("Subtitles")));
        for extension in SUBTITLE_EXTENSIONS {
            filter.add_suffix(extension);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title(gettext("Open Subtitles"))
            .filters(&filters)
            .modal(true)
            .build();
        if let Some(folder) = self.file().and_then(|f| f.parent()) {
            dialog.set_initial_folder(Some(&folder));
        }
        let window = self.imp().picture.root().and_downcast::<gtk::Window>();
        if let Ok(file) = dialog.open_future(window.as_ref()).await {
            self.load_subtitles(&file);
        }
    }

    /// Shows the subtitles in `file`, picking up where the video was
    pub fn load_subtitles(&self, file: &gio::File) {
        let imp = self.imp();
        let Some(playbin) = imp.playbin.get() else {
            return;
        };
        let position = imp.position().unwrap_or(gst::ClockTime::ZERO);
        imp.restore.set(Some((position, self.is_playing())));
        imp.playing.set(false);
        imp.subtitle_file.replace(Some(file.clone()));
        imp.select_new_subtitle.set(true);
        let (playbin, uri) = (playbin.clone(), file.uri().to_string());
        in_background(move || {
            let _ = playbin.set_state(gst::State::Ready);
            playbin.set_property("suburi", uri.as_str());
            let _ = playbin.set_state(gst::State::Paused);
        });
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
        imp.ended.set(false);
        let Some(playbin) = imp.playbin.get() else {
            return;
        };
        seek_in_background(playbin, &imp.seek_generation, imp.rate.get(), position);
        imp.position_label.set_label(&format_time(position));
        imp.hide_subtitle();
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
            let playbin = playbin.clone();
            in_background(move || {
                playbin.send_event(gst::event::Step::new(
                    gst::format::Buffers::ONE,
                    1.,
                    true,
                    false,
                ));
            });
            glib::timeout_add_local_once(
                Duration::from_millis(100),
                clone!(
                    #[weak(rename_to = obj)]
                    self,
                    move || {
                        obj.imp().update_position();
                        obj.imp().schedule_frame_handoff();
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

    pub fn connect_controls_shown<F: Fn(&Self, bool) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "controls-shown",
            false,
            glib::closure_local!(move |obj: &Self, shown: bool| f(obj, shown)),
        )
    }

    /// Shows the controls, as moving the pointer does
    pub fn show_controls(&self) {
        self.imp().show_controls();
    }

    /// The controls stay while the pointer rests on what `motion` watches,
    /// like the folder strip
    pub fn add_hover_area(&self, motion: &gtk::EventControllerMotion) {
        let mut areas = self.imp().hover_areas.borrow_mut();
        if !areas.contains(motion) {
            areas.push(motion.clone());
        }
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
    // Scaled to RGBA before the sink, so the last sample is ready to use
    let video_sink = gst::parse::bin_from_description(
        &format!(
            "videoconvert ! videoscale ! video/x-raw,format=RGBA,width={width},pixel-aspect-ratio=1/1 \
             ! fakesink name=sink enable-last-sample=true"
        ),
        true,
    )?;
    let sink = video_sink
        .by_name("sink")
        .ok_or_else(|| anyhow::anyhow!("No sink"))?;
    let playbin = gst::ElementFactory::make("playbin3")
        .property("uri", uri)
        .property("audio-sink", gst::ElementFactory::make("fakesink").build()?)
        .property("video-sink", &video_sink)
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
            // The exact frame: the nearest keyframe can lie before or after
            // the pointer, so moving along the seek bar would jump back
            playbin.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, position)?;
            let (state, ..) = playbin.state(timeout);
            state?;
        }

        let sample = sink
            .property::<Option<gst::Sample>>("last-sample")
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

/// Size the video shows at, turned as its rotation tag says; blocks
fn display_size(uri: &str) -> Option<(i32, i32)> {
    let discoverer = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(3)).ok()?;
    let info = discoverer.discover_uri(uri).ok()?;
    let video = info.video_streams().into_iter().next()?;
    let (width, height) = (video.width() as i32, video.height() as i32);
    let turned = gst_pbutils::prelude::DiscovererStreamInfoExt::tags(&video)
        .and_then(|tags| tags.get::<gst::tags::ImageOrientation>().map(|x| x.get().to_string()))
        .is_some_and(|orientation| orientation.contains("90") || orientation.contains("270"));
    let (width, height) = if turned { (height, width) } else { (width, height) };
    (width > 0 && height > 0).then_some((width, height))
}

/// `1×`, `1.5×`
fn speed_label(rate: f64) -> String {
    // Translators: Playback speed, {} is a number like 1.5
    gettext_f("{}×", [rate.to_string()])
}

/// Title or language of a track, or its number
fn stream_label(stream: &gst::Stream, number: usize) -> String {
    let tags = stream.tags();
    let title = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::Title>())
        .map(|title| title.get().to_string());
    let language = tags
        .as_ref()
        .and_then(|tags| tags.get::<gst::tags::LanguageCode>())
        .map(|code| {
            let code = code.get().to_string();
            gst_tag::language_codes::language_name(&code)
                .map(|name| name.to_string())
                .unwrap_or(code)
        });
    match (title, language) {
        (Some(title), Some(language)) if title != language => format!("{title} ({language})"),
        (Some(title), _) => title,
        (None, Some(language)) => language,
        // Translators: A subtitle or audio track without a name, {} is its number
        (None, None) => gettext_f("Track {}", [number.to_string()]),
    }
}

/// A subtitle file named like the video: `movie.srt`, or `movie.en.srt`
/// preferring the user's language
pub fn sidecar_subtitle(video: &gio::File) -> Option<gio::File> {
    let path = video.path()?;
    let stem = path.file_stem()?.to_str()?.to_string();
    let dir = path.parent()?;

    for extension in SUBTITLE_EXTENSIONS {
        let exact = dir.join(format!("{stem}.{extension}"));
        if exact.is_file() {
            return Some(gio::File::for_path(exact));
        }
    }

    let language = user_language();
    let mut fallback = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(&format!("{stem}.")) else {
            continue;
        };
        let Some((lang, extension)) = rest.rsplit_once('.') else {
            continue;
        };
        if !SUBTITLE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()) {
            continue;
        }
        if lang.eq_ignore_ascii_case(&language) {
            return Some(gio::File::for_path(entry.path()));
        }
        fallback.get_or_insert_with(|| gio::File::for_path(entry.path()));
    }
    fallback
}

/// Two letter code of the user's language, like `en`
pub fn user_language() -> String {
    glib::language_names()
        .iter()
        .map(|name| name.to_string())
        .find(|name| name != "C" && name != "POSIX")
        .and_then(|name| name.split(['_', '.', '@']).next().map(|x| x.to_string()))
        .filter(|code| code.len() == 2)
        .unwrap_or_else(|| "en".to_string())
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
    let dir = glib::user_cache_dir().join("blick").join("frames");
    let _ = std::fs::create_dir_all(&dir);
    let key = file.map(|f| f.uri().to_string()).unwrap_or_default();
    let hash = glib::compute_checksum_for_string(glib::ChecksumType::Sha1, &key)
        .map(|x| x.to_string())
        .unwrap_or_default();
    dir.join(format!("{hash}.png"))
}

/// Remembered playback positions, one per video
fn positions_path() -> PathBuf {
    glib::user_data_dir().join("blick").join("video-positions")
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




/// Changes the state of `pipeline` on the worker thread, see `in_background`
pub fn set_state_in_background(pipeline: &gst::Element, state: gst::State) {
    let pipeline = pipeline.clone();
    in_background(move || {
        if let Err(err) = pipeline.set_state(state) {
            tracing::warn!("Could not change video state: {err}");
        }
    });
}

/// Seeks `pipeline` to the exact frame at `position` on the worker thread.
/// `generation` counts the seeks: one that is no longer the newest is
/// skipped, so dragging doesn't queue up seeks.
pub fn seek_in_background(
    pipeline: &gst::Element,
    generation: &Arc<AtomicU64>,
    rate: f64,
    position: gst::ClockTime,
) {
    let this = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let (pipeline, generation) = (pipeline.clone(), generation.clone());
    in_background(move || {
        if generation.load(Ordering::SeqCst) != this {
            return;
        }
        let result = pipeline.seek(
            rate,
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE,
            gst::SeekType::Set,
            position,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        );
        if let Err(err) = result {
            tracing::debug!("Seek failed: {err}");
        }
    });
}

/// Runs `f` on a worker thread, one job after the other. Changing a
/// pipeline can wait for its video thread, which may be waiting for the
/// main thread while the video starts, so the main thread never does it.
pub fn in_background(f: impl FnOnce() + Send + 'static) {
    type Job = Box<dyn FnOnce() + Send>;
    static WORKER: OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> = OnceLock::new();
    let sender = WORKER.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Job>();
        std::thread::spawn(move || {
            while let Ok(job) = receiver.recv() {
                job();
            }
        });
        std::sync::Mutex::new(sender)
    });
    if let Ok(sender) = sender.lock() {
        let _ = sender.send(Box::new(f));
    }
}

/// Moves the popover of `button` sideways so it stays inside the window;
/// centred on a button near the edge, it would stick out
fn keep_in_window(button: &gtk::MenuButton) {
    const MARGIN: f32 = 8.;
    let (Some(popover), Some(root)) = (button.popover(), button.root()) else {
        return;
    };
    let Some(bounds) = button.compute_bounds(&root) else {
        return;
    };
    let (_, width, _, _) = popover.measure(gtk::Orientation::Horizontal, -1);
    let half = width as f32 / 2.;
    let center = bounds.x() + bounds.width() / 2.;
    let right = root.width() as f32 - MARGIN;
    let mut shift = (right - (center + half)).min(0.);
    shift += (MARGIN - (center + shift - half)).max(0.);
    popover.set_offset(shift.round() as i32, -6);
}

/// A row of the settings menu that opens a list or goes back: the label,
/// the current value dimmed, and a chevron pointing where it goes
fn menu_row(label: &str, value: Option<&str>, back: bool) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    if back {
        content.append(&gtk::Image::from_icon_name("glance-left-symbolic"));
    }
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.);
    text.set_hexpand(true);
    content.append(&text);
    if let Some(value) = value {
        let value = gtk::Label::new(Some(value));
        value.add_css_class("dim-label");
        value.set_ellipsize(gtk::pango::EllipsizeMode::End);
        value.set_max_width_chars(12);
        content.append(&value);
        content.append(&gtk::Image::from_icon_name("glance-right-symbolic"));
    }
    let row = gtk::Button::new();
    row.set_child(Some(&content));
    row.add_css_class("flat");
    row.add_css_class("menu-row");
    row
}

/// A choice in a list of the settings menu, checked when chosen
fn check_row(label: &str, chosen: bool) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let check = gtk::Image::from_icon_name("check-plain-symbolic");
    check.set_opacity(if chosen { 1. } else { 0. });
    content.append(&check);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.);
    text.set_hexpand(true);
    content.append(&text);
    let row = gtk::Button::new();
    row.set_child(Some(&content));
    row.add_css_class("flat");
    row.add_css_class("menu-row");
    if chosen {
        row.update_state(&[gtk::accessible::State::Checked(gtk::AccessibleTristate::True)]);
    }
    row
}

/// A row of the settings menu with a control at the end
fn setting_row(title: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("setting-row");
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.);
    label.set_hexpand(true);
    row.append(&label);
    control.as_ref().set_valign(gtk::Align::Center);
    row.append(control);
    row
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


