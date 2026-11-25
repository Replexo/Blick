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

//! Trimmer for the video editor, like the one in iPhone Photos
//!
//! A filmstrip of the video with a frame around the kept range, handles at
//! both ends, a dimmed area outside and a white playhead. The frame is grey
//! until the trimmer is touched or the range changes, then it takes the
//! accent colour. Holding a handle still, or moving it slowly, zooms the
//! filmstrip in until single frames show, so the cut can be set on the
//! exact frame; letting go zooms back out. Left and Right move the last
//! used handle by one frame.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::clone;
use glib::subclass::Signal;
use gst::prelude::*;

use crate::deps::*;
use crate::util::gettext::*;

const HEIGHT: i32 = 56;
const HANDLE_WIDTH: f32 = 16.;
const BORDER: f32 = 3.;
const RADIUS: f32 = 8.;
/// Shortest kept range
const MIN_LENGTH: f64 = 0.1;
/// Frames visible when zoomed in
const ZOOM_FRAMES: f64 = 24.;
/// A handle held this long zooms in
const HOLD_TIME: Duration = Duration::from_millis(500);
/// Slower than this, in pixels per second, counts as a fine adjustment
const SLOW_SPEED: f64 = 40.;

/// Part of the trimmer being dragged
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Start,
    End,
    Playhead,
}

/// Decodes frames of a video on a thread, one pipeline for all requests
pub struct FrameGrabber {
    sender: std::sync::mpsc::Sender<(f64, bool, async_channel::Sender<Option<gdk::Texture>>)>,
}

impl FrameGrabber {
    /// Frames come `height` pixels high
    pub fn new(uri: String, height: u32) -> Self {
        let (sender, receiver) =
            std::sync::mpsc::channel::<(f64, bool, async_channel::Sender<Option<gdk::Texture>>)>();
        std::thread::spawn(move || {
            let Some((playbin, sink)) = grabber_pipeline(&uri, height) else {
                return;
            };
            while let Ok((seconds, accurate, reply)) = receiver.recv() {
                let frame = grab(&playbin, &sink, seconds, accurate);
                let _ = reply.send_blocking(frame);
            }
            let _ = playbin.set_state(gst::State::Null);
        });
        Self { sender }
    }

    /// The frame at `seconds`; `accurate` decodes the exact frame instead of
    /// the nearest keyframe
    pub async fn frame(&self, seconds: f64, accurate: bool) -> Option<gdk::Texture> {
        let (reply, receive) = async_channel::bounded(1);
        self.sender.send((seconds, accurate, reply)).ok()?;
        receive.recv().await.ok().flatten()
    }
}

fn grabber_pipeline(uri: &str, height: u32) -> Option<(gst::Element, gst::Element)> {
    let video_sink = gst::parse::bin_from_description(
        &format!(
            "videoconvert ! videoscale ! video/x-raw,format=RGBA,height={height},pixel-aspect-ratio=1/1 \
             ! fakesink name=sink enable-last-sample=true sync=false"
        ),
        true,
    )
    .ok()?;
    let sink = video_sink.by_name("sink")?;
    let playbin = gst::ElementFactory::make("playbin3")
        .property("uri", uri)
        .property("audio-sink", gst::ElementFactory::make("fakesink").build().ok()?)
        .property("video-sink", &video_sink)
        .build()
        .ok()?;
    playbin.set_state(gst::State::Paused).ok()?;
    let (state, ..) = playbin.state(gst::ClockTime::from_seconds(5));
    state.ok()?;
    Some((playbin, sink))
}

/// Gtk textures are not `Send`, so frames are made on the thread and only
/// moved
fn grab(playbin: &gst::Element, sink: &gst::Element, seconds: f64, accurate: bool) -> Option<gdk::Texture> {
    let flags = if accurate {
        gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE
    } else {
        gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT | gst::SeekFlags::SNAP_NEAREST
    };
    playbin
        .seek_simple(flags, gst::ClockTime::from_seconds_f64(seconds.max(0.)))
        .ok()?;
    let (state, ..) = playbin.state(gst::ClockTime::from_seconds(5));
    state.ok()?;
    let sample = sink.property::<Option<gst::Sample>>("last-sample")?;
    let info = gst_video::VideoInfo::from_caps(sample.caps()?).ok()?;
    let buffer = sample.buffer()?;
    let map = buffer.map_readable().ok()?;
    Some(
        gdk::MemoryTexture::new(
            info.width() as i32,
            info.height() as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from(map.as_slice()),
            info.stride()[0] as usize,
        )
        .upcast(),
    )
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct LpVideoTrimmer {
        pub(super) duration: Cell<f64>,
        pub(super) frame_duration: Cell<f64>,
        pub(super) start: Cell<f64>,
        pub(super) end: Cell<f64>,
        pub(super) position: Cell<f64>,
        /// Shown time span, from all of the video to single frames
        pub(super) view: Cell<(f64, f64)>,
        pub(super) zoom_from: Cell<(f64, f64)>,
        pub(super) zoom_to: Cell<(f64, f64)>,
        pub(super) zoomed: Cell<bool>,
        pub(super) animation: OnceCell<adw::TimedAnimation>,

        /// Touched since it opened, so the frame shows the accent colour
        pub(super) active: Cell<bool>,
        pub(super) dragging: Cell<Option<Part>>,
        /// Last handle used, for the arrow keys
        pub(super) selected: Cell<Option<Part>>,
        /// Pointer x and time of the dragged part when the drag or the last
        /// zoom began
        pub(super) anchor: Cell<(f64, f64)>,
        pub(super) drag_x: Cell<f64>,
        pub(super) last_motion: Cell<Option<(Instant, f64)>>,
        pub(super) slow_since: Cell<Option<Instant>>,
        pub(super) hold_timeout: RefCell<Option<glib::SourceId>>,

        pub(super) grabber: OnceCell<FrameGrabber>,
        /// Thumbnails by millisecond, and if they show the exact frame
        /// rather than the keyframe before it
        pub(super) thumbnails: RefCell<BTreeMap<i64, (gdk::Texture, bool)>>,
        pub(super) wanted: RefCell<Vec<i64>>,
        pub(super) loading: Cell<bool>,
        pub(super) thumbnail_aspect: Cell<f64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpVideoTrimmer {
        const NAME: &'static str = "LpVideoTrimmer";
        type Type = super::LpVideoTrimmer;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("lpvideotrimmer");
            klass.set_accessible_role(gtk::AccessibleRole::Slider);
        }
    }

    impl ObjectImpl for LpVideoTrimmer {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // A handle shows this frame while it moves
                    Signal::builder("preview")
                        .param_types([f64::static_type()])
                        .build(),
                    // The playhead was moved
                    Signal::builder("seek").param_types([f64::static_type()]).build(),
                    // The kept range changed, after a drag or a key
                    Signal::builder("range-changed")
                        .param_types([f64::static_type(), f64::static_type()])
                        .build(),
                    // Zoomed in to single frames, or back out
                    Signal::builder("zoom-changed")
                        .param_types([bool::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_focusable(true);
            obj.set_hexpand(true);
            obj.set_height_request(HEIGHT);
            obj.update_property(&[gtk::accessible::Property::Label(&gettext("Trim"))]);
            self.thumbnail_aspect.set(16. / 9.);

            let drag = gtk::GestureDrag::new();
            drag.connect_drag_begin(clone!(
                #[weak]
                obj,
                move |_, x, _| obj.imp().drag_begin(x)
            ));
            drag.connect_drag_update(clone!(
                #[weak]
                obj,
                move |gesture, dx, _| {
                    if let Some((x, _)) = gesture.start_point() {
                        obj.imp().drag_update(x + dx);
                    }
                }
            ));
            drag.connect_drag_end(clone!(
                #[weak]
                obj,
                move |_, _, _| obj.imp().drag_end()
            ));
            obj.add_controller(drag);

            let keys = gtk::EventControllerKey::new();
            keys.connect_key_pressed(clone!(
                #[weak]
                obj,
                #[upgrade_or]
                glib::Propagation::Proceed,
                move |_, key, _, _| {
                    let step = match key {
                        gdk::Key::Left => -1.,
                        gdk::Key::Right => 1.,
                        _ => return glib::Propagation::Proceed,
                    };
                    obj.imp().step(step);
                    glib::Propagation::Stop
                }
            ));
            obj.add_controller(keys);

            let target = adw::CallbackAnimationTarget::new(clone!(
                #[weak]
                obj,
                move |value| {
                    let imp = obj.imp();
                    let ((a0, a1), (b0, b1)) = (imp.zoom_from.get(), imp.zoom_to.get());
                    imp.view
                        .set((a0 + (b0 - a0) * value, a1 + (b1 - a1) * value));
                    imp.request_thumbnails();
                    obj.queue_draw();
                }
            ));
            let animation = adw::TimedAnimation::new(&*obj, 0., 1., 250, target);
            animation.set_easing(adw::Easing::EaseOutCubic);
            let _ = self.animation.set(animation);
        }

        fn dispose(&self) {
            if let Some(source) = self.hold_timeout.take() {
                source.remove();
            }
        }
    }

    impl WidgetImpl for LpVideoTrimmer {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            match orientation {
                gtk::Orientation::Horizontal => (120, 600, -1, -1),
                _ => (HEIGHT, HEIGHT, -1, -1),
            }
        }

        fn size_allocate(&self, _width: i32, _height: i32, _baseline: i32) {
            self.request_thumbnails();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            if width <= 2. * HANDLE_WIDTH || self.duration.get() <= 0. {
                return;
            }

            let inner = graphene::Rect::new(HANDLE_WIDTH, BORDER, width - 2. * HANDLE_WIDTH, height - 2. * BORDER);
            let track = gsk::RoundedRect::from_rect(inner, RADIUS / 2.);
            snapshot.push_rounded_clip(&track);
            snapshot.append_color(&gdk::RGBA::new(0.1, 0.1, 0.12, 1.), &inner);
            self.draw_filmstrip(snapshot, &inner);

            // Dim what is cut away
            let dim = gdk::RGBA::new(0., 0., 0., 0.6);
            let start_x = self.time_to_x(self.start.get()) as f32;
            let end_x = self.time_to_x(self.end.get()) as f32;
            if start_x > inner.x() {
                snapshot.append_color(
                    &dim,
                    &graphene::Rect::new(inner.x(), inner.y(), start_x - inner.x(), inner.height()),
                );
            }
            if end_x < inner.x() + inner.width() {
                snapshot.append_color(
                    &dim,
                    &graphene::Rect::new(end_x, inner.y(), inner.x() + inner.width() - end_x, inner.height()),
                );
            }
            snapshot.pop();

            // Frame with a handle at each end
            let color = self.frame_color();
            let left = start_x - HANDLE_WIDTH;
            for y in [0., height - BORDER] {
                snapshot.append_color(&color, &graphene::Rect::new(start_x, y, end_x - start_x, BORDER));
            }
            for (x, outer_left) in [(left, true), (end_x, false)] {
                let rect = graphene::Rect::new(x, 0., HANDLE_WIDTH, height);
                let corner = graphene::Size::new(RADIUS, RADIUS);
                let none = graphene::Size::new(0., 0.);
                let rounded = if outer_left {
                    gsk::RoundedRect::new(rect, corner, none, none, corner)
                } else {
                    gsk::RoundedRect::new(rect, none, corner, corner, none)
                };
                snapshot.push_rounded_clip(&rounded);
                snapshot.append_color(&color, &rect);
                snapshot.pop();
                // Grip line
                let grip = gdk::RGBA::new(0., 0., 0., 0.55);
                snapshot.append_color(
                    &grip,
                    &graphene::Rect::new(x + HANDLE_WIDTH / 2. - 1., height * 0.32, 2., height * 0.36),
                );
            }

            // Playhead
            let position = self.position.get();
            if position >= self.view.get().0 && position <= self.view.get().1 {
                let x = self.time_to_x(position) as f32;
                let rect = graphene::Rect::new(x - 1.5, -2., 3., height + 4.);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, 1.5));
                snapshot.append_color(&gdk::RGBA::WHITE, &rect);
                snapshot.pop();
            }
        }
    }

    impl LpVideoTrimmer {
        fn frame_color(&self) -> gdk::RGBA {
            let changed = self.start.get() > 0.001 || self.end.get() < self.duration.get() - 0.001;
            if self.active.get() || self.dragging.get().is_some() || changed {
                adw::StyleManager::default().accent_color_rgba()
            } else {
                gdk::RGBA::new(0.72, 0.72, 0.74, 1.)
            }
        }

        fn inner_width(&self) -> f64 {
            (self.obj().width() as f64 - 2. * HANDLE_WIDTH as f64).max(1.)
        }

        pub(super) fn time_to_x(&self, time: f64) -> f64 {
            let (from, to) = self.view.get();
            HANDLE_WIDTH as f64 + (time - from) / (to - from).max(1e-6) * self.inner_width()
        }

        fn seconds_per_pixel(&self) -> f64 {
            let (from, to) = self.view.get();
            (to - from) / self.inner_width()
        }

        /// Width of one thumbnail, and the time it covers
        fn slot(&self, inner: &graphene::Rect) -> (f64, f64) {
            let width = inner.height() as f64 * self.thumbnail_aspect.get();
            let mut time = width * self.seconds_per_pixel();
            // Never finer than one frame
            let frame = self.frame_duration.get().max(1e-3);
            time = (time / frame).ceil().max(1.) * frame;
            (time / self.seconds_per_pixel(), time)
        }

        fn draw_filmstrip(&self, snapshot: &gtk::Snapshot, inner: &graphene::Rect) {
            let (slot_width, slot_time) = self.slot(inner);
            let (from, to) = self.view.get();
            let thumbnails = self.thumbnails.borrow();
            let mut time = (from / slot_time).floor() * slot_time;
            while time < to {
                let key = (time * 1000.) as i64;
                // The closest thumbnail made so far
                let texture = thumbnails
                    .range(..=key)
                    .next_back()
                    .or_else(|| thumbnails.range(key..).next())
                    .map(|(_, (texture, _))| texture);
                if let Some(texture) = texture {
                    let x = self.time_to_x(time) as f32;
                    let rect = graphene::Rect::new(x, inner.y(), slot_width as f32 + 0.5, inner.height());
                    snapshot.push_clip(&rect);
                    let aspect = texture.width() as f32 / texture.height().max(1) as f32;
                    let width = inner.height() * aspect;
                    snapshot.append_texture(
                        texture,
                        &graphene::Rect::new(
                            x + (slot_width as f32 - width) / 2.,
                            inner.y(),
                            width,
                            inner.height(),
                        ),
                    );
                    snapshot.pop();
                }
                time += slot_time;
            }
        }

        /// Asks for thumbnails of the shown slots that are still missing
        pub(super) fn request_thumbnails(&self) {
            let obj = self.obj();
            if self.grabber.get().is_none() || obj.width() <= 0 || self.duration.get() <= 0. {
                return;
            }
            let inner = graphene::Rect::new(0., 0., self.inner_width() as f32, (obj.height() as f32 - 2. * BORDER).max(1.));
            let (_, slot_time) = self.slot(&inner);
            let (from, to) = self.view.get();
            let thumbnails = self.thumbnails.borrow();
            let mut wanted = Vec::new();
            let mut time = (from / slot_time).floor() * slot_time;
            while time < to.min(self.duration.get()) {
                let key = (time * 1000.) as i64;
                // Zoomed in, keyframe thumbnails can be seconds off
                let usable = thumbnails
                    .get(&key)
                    .is_some_and(|(_, exact)| *exact || !self.zoomed.get());
                if !usable {
                    wanted.push(key);
                }
                time += slot_time;
            }
            drop(thumbnails);
            self.wanted.replace(wanted);
            self.load_next();
        }

        fn load_next(&self) {
            if self.loading.get() {
                return;
            }
            let Some(key) = self.wanted.borrow_mut().pop() else {
                return;
            };
            self.loading.set(true);
            let obj = self.obj();
            let accurate = self.zoomed.get();
            glib::spawn_future_local(clone!(
                #[weak]
                obj,
                async move {
                    let imp = obj.imp();
                    let Some(grabber) = imp.grabber.get() else {
                        return;
                    };
                    let texture = grabber.frame(key as f64 / 1000., accurate).await;
                    if let Some(texture) = texture {
                        imp.thumbnail_aspect
                            .set(texture.width() as f64 / texture.height().max(1) as f64);
                        imp.thumbnails.borrow_mut().insert(key, (texture, accurate));
                        obj.queue_draw();
                    }
                    imp.loading.set(false);
                    imp.load_next();
                }
            ));
        }

        fn drag_begin(&self, x: f64) {
            let obj = self.obj();
            obj.grab_focus();
            let start_x = self.time_to_x(self.start.get());
            let end_x = self.time_to_x(self.end.get());
            let reach = HANDLE_WIDTH as f64 + 8.;
            let part = if x <= start_x && x >= start_x - reach {
                Part::Start
            } else if x >= end_x && x <= end_x + reach {
                Part::End
            } else if (x - start_x).abs() < reach / 2. && (x - start_x).abs() <= (x - end_x).abs() {
                Part::Start
            } else if (x - end_x).abs() < reach / 2. {
                Part::End
            } else {
                Part::Playhead
            };
            self.active.set(true);
            self.dragging.set(Some(part));
            if part != Part::Playhead {
                self.selected.set(Some(part));
            }
            self.anchor.set((x, self.part_time(part)));
            self.drag_x.set(x);
            self.last_motion.set(Some((Instant::now(), x)));
            self.slow_since.set(Some(Instant::now()));

            if part == Part::Playhead {
                self.move_part(part, self.x_to_time_absolute(x));
            } else {
                // Holding a handle still zooms in
                let source = glib::timeout_add_local_once(
                    HOLD_TIME,
                    clone!(
                        #[weak]
                        obj,
                        move || {
                            let imp = obj.imp();
                            imp.hold_timeout.take();
                            if imp.dragging.get().is_some_and(|p| p != Part::Playhead) {
                                imp.zoom_in();
                            }
                        }
                    ),
                );
                if let Some(old) = self.hold_timeout.replace(Some(source)) {
                    old.remove();
                }
            }
            obj.queue_draw();
        }

        fn x_to_time_absolute(&self, x: f64) -> f64 {
            let (from, _) = self.view.get();
            from + (x - HANDLE_WIDTH as f64) * self.seconds_per_pixel()
        }

        fn part_time(&self, part: Part) -> f64 {
            match part {
                Part::Start => self.start.get(),
                Part::End => self.end.get(),
                Part::Playhead => self.position.get(),
            }
        }

        fn drag_update(&self, x: f64) {
            let Some(part) = self.dragging.get() else {
                return;
            };
            self.drag_x.set(x);

            // Slow, small moves zoom in for fine adjustments
            let now = Instant::now();
            if let Some((time, last_x)) = self.last_motion.replace(Some((now, x))) {
                let elapsed = now.duration_since(time).as_secs_f64().max(1e-3);
                let speed = (x - last_x).abs() / elapsed;
                if speed > SLOW_SPEED {
                    self.slow_since.set(Some(now));
                    if let Some(source) = self.hold_timeout.take() {
                        source.remove();
                    }
                } else if part != Part::Playhead
                    && !self.zoomed.get()
                    && self
                        .slow_since
                        .get()
                        .is_some_and(|since| now.duration_since(since) > HOLD_TIME)
                {
                    self.zoom_in();
                }
            }

            let (anchor_x, anchor_time) = self.anchor.get();
            let time = anchor_time + (x - anchor_x) * self.seconds_per_pixel();
            self.move_part(part, time);
        }

        /// Moves `part` to `time` and tells the editor
        fn move_part(&self, part: Part, time: f64) {
            let obj = self.obj();
            let frame = self.frame_duration.get().max(1e-3);
            // Cuts land on frames
            let time = (time / frame).round() * frame;
            match part {
                Part::Start => {
                    let start = time.clamp(0., (self.end.get() - MIN_LENGTH).max(0.));
                    self.start.set(start);
                    self.position.set(start);
                    obj.emit_by_name::<()>("preview", &[&start]);
                }
                Part::End => {
                    // A video shorter than the shortest cut stays whole
                    let lower = self.start.get() + MIN_LENGTH;
                    let end = time.clamp(lower, self.duration.get().max(lower));
                    self.end.set(end);
                    self.position.set(end);
                    obj.emit_by_name::<()>("preview", &[&end]);
                }
                Part::Playhead => {
                    let position = time.clamp(self.start.get(), self.end.get());
                    self.position.set(position);
                    obj.emit_by_name::<()>("seek", &[&position]);
                }
            }
            obj.queue_draw();
        }

        fn drag_end(&self) {
            let obj = self.obj();
            if let Some(source) = self.hold_timeout.take() {
                source.remove();
            }
            let part = self.dragging.take();
            if self.zoomed.get() {
                self.zoom_out();
            }
            if matches!(part, Some(Part::Start | Part::End)) {
                obj.emit_by_name::<()>("range-changed", &[&self.start.get(), &self.end.get()]);
            }
            obj.queue_draw();
        }

        /// Zooms in around the dragged handle, which stays under the pointer
        fn zoom_in(&self) {
            let Some(part) = self.dragging.get() else {
                return;
            };
            if self.zoomed.replace(true) {
                return;
            }
            self.obj().emit_by_name::<()>("zoom-changed", &[&true]);
            let time = self.part_time(part);
            let x = self.time_to_x(time);
            let span = (ZOOM_FRAMES * self.frame_duration.get()).max(0.4);
            let from = time - (x - HANDLE_WIDTH as f64) / self.inner_width() * span;
            self.animate_view((from, from + span));
            // New anchor, so the handle follows the pointer at the new scale
            self.anchor.set((self.drag_x.get(), time));
        }

        fn zoom_out(&self) {
            self.zoomed.set(false);
            self.obj().emit_by_name::<()>("zoom-changed", &[&false]);
            self.animate_view((0., self.duration.get()));
        }

        fn animate_view(&self, target: (f64, f64)) {
            let Some(animation) = self.animation.get() else {
                return;
            };
            self.zoom_from.set(self.view.get());
            self.zoom_to.set(target);
            // Thumbnails are made for the new scale
            animation.reset();
            animation.play();
        }

        /// Moves the last used handle by `frames`
        fn step(&self, frames: f64) {
            let obj = self.obj();
            let part = self.selected.get().unwrap_or(Part::Start);
            self.active.set(true);
            let time = self.part_time(part) + frames * self.frame_duration.get();
            self.move_part(part, time);
            obj.emit_by_name::<()>("range-changed", &[&self.start.get(), &self.end.get()]);
        }
    }
}

glib::wrapper! {
    pub struct LpVideoTrimmer(ObjectSubclass<imp::LpVideoTrimmer>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl LpVideoTrimmer {
    pub fn new(file: &gio::File, duration: f64, frame_duration: f64) -> Self {
        let obj: Self = glib::Object::new();
        let imp = obj.imp();
        imp.duration.set(duration);
        imp.frame_duration.set(frame_duration.max(1e-3));
        imp.start.set(0.);
        imp.end.set(duration);
        imp.view.set((0., duration));
        let _ = imp
            .grabber
            .set(FrameGrabber::new(file.uri().to_string(), (HEIGHT as f32 - 2. * BORDER) as u32 * 2));
        imp.request_thumbnails();
        obj
    }

    pub fn range(&self) -> (f64, f64) {
        (self.imp().start.get(), self.imp().end.get())
    }

    /// Sets the kept range, as after undo
    pub fn set_range(&self, start: f64, end: f64) {
        let imp = self.imp();
        imp.start.set(start);
        imp.end.set(end);
        self.queue_draw();
    }

    pub fn set_position(&self, position: f64) {
        let imp = self.imp();
        if imp.dragging.get().is_none() {
            imp.position.set(position);
            self.queue_draw();
        }
    }

    pub fn connect_preview<F: Fn(&Self, f64) + 'static>(&self, f: F) {
        self.connect_closure(
            "preview",
            false,
            glib::closure_local!(move |obj: &Self, time: f64| f(obj, time)),
        );
    }

    pub fn connect_seek<F: Fn(&Self, f64) + 'static>(&self, f: F) {
        self.connect_closure(
            "seek",
            false,
            glib::closure_local!(move |obj: &Self, time: f64| f(obj, time)),
        );
    }

    pub fn connect_zoom_changed<F: Fn(&Self, bool) + 'static>(&self, f: F) {
        self.connect_closure(
            "zoom-changed",
            false,
            glib::closure_local!(move |obj: &Self, zoomed: bool| f(obj, zoomed)),
        );
    }

    pub fn connect_range_changed<F: Fn(&Self, f64, f64) + 'static>(&self, f: F) {
        self.connect_closure(
            "range-changed",
            false,
            glib::closure_local!(move |obj: &Self, start: f64, end: f64| f(obj, start, end)),
        );
    }
}
