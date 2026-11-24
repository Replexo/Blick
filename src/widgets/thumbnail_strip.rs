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

//! Row of thumbnails for the images of a set or of a folder
//!
//! Clicking a thumbnail shows that image. For a set, its close button removes
//! it from the set (the file itself stays). Thumbnails are only made for the
//! visible part of the row, so large folders stay fast.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::LazyLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::subclass::Signal;

use crate::deps::*;
use crate::util::gettext::*;

/// Thumbnail edge length in logical pixels
const THUMBNAIL_SIZE: i32 = 46;
/// Height of the compact folder strip's thumbnails
const COMPACT_HEIGHT: i32 = 36;
/// Width of the other photos in the compact strip: narrow and tall
const COMPACT_SIZE: i32 = 24;
/// Widest the current photo gets, which shows its real shape
const COMPACT_CURRENT_MAX: i32 = 64;
/// Width of the fades at the ends of a strip wider than the window
const EDGE_FADE: f32 = 32.;
/// Space around each thumbnail from the CSS, see `thumbnails.css`
const ITEM_SPACE: i32 = 8;
const COMPACT_ITEM_SPACE: i32 = 2;
/// Extra space around the current thumbnail in the compact strip
const COMPACT_CURRENT_SPACE: i32 = 8;
/// Thumbnails kept in memory before the cache starts over
const CACHE_LIMIT: usize = 300;
/// Wait before loading, so thumbnails only scrolled past aren't loaded
const LOAD_DELAY: std::time::Duration = std::time::Duration::from_millis(80);

mod imp {
    use super::*;

    #[derive(Debug)]
    pub struct LpThumbnailStrip {
        pub(super) store: gio::ListStore,
        pub(super) list_view: gtk::ListView,
        pub(super) scrolled: gtk::ScrolledWindow,
        pub(super) current: RefCell<Option<gio::File>>,
        /// Show close buttons that remove images from the set
        pub(super) removable: Cell<bool>,
        /// Small thumbnails, only the current one is larger
        pub(super) compact: Cell<bool>,
        pub(super) cache: RefCell<HashMap<String, gdk::Texture>>,
        /// All list items, to restyle them without binding them again
        pub(super) list_items: RefCell<Vec<glib::WeakRef<gtk::ListItem>>>,
    }

    impl Default for LpThumbnailStrip {
        fn default() -> Self {
            Self {
                store: gio::ListStore::new::<gio::File>(),
                list_view: gtk::ListView::default(),
                scrolled: gtk::ScrolledWindow::default(),
                current: Default::default(),
                removable: Cell::new(true),
                compact: Cell::new(false),
                cache: Default::default(),
                list_items: Default::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpThumbnailStrip {
        const NAME: &'static str = "LpThumbnailStrip";
        type Type = super::LpThumbnailStrip;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("lpthumbnailstrip");
        }
    }

    impl ObjectImpl for LpThumbnailStrip {
        fn signals() -> &'static [Signal] {
            static SIGNALS: LazyLock<Vec<Signal>> = LazyLock::new(|| {
                vec![
                    Signal::builder("activate-file")
                        .param_types([gio::File::static_type()])
                        .build(),
                    Signal::builder("remove-file")
                        .param_types([gio::File::static_type()])
                        .build(),
                ]
            });
            SIGNALS.as_ref()
        }

        fn constructed(&self) {
            self.parent_constructed();

            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(glib::clone!(
                #[weak(rename_to = imp)]
                self,
                move |_, item| {
                    let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                    imp.setup_item(item);
                }
            ));
            factory.connect_bind(glib::clone!(
                #[weak(rename_to = imp)]
                self,
                move |_, item| {
                    let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                    imp.bind_item(item);
                }
            ));

            self.list_view
                .set_model(Some(&gtk::NoSelection::new(Some(self.store.clone()))));
            self.list_view.set_factory(Some(&factory));
            self.list_view.set_orientation(gtk::Orientation::Horizontal);

            // Few thumbnails stay centered, the list view itself ignores its alignment
            // inside the scrolled window
            self.scrolled.set_halign(gtk::Align::Center);
            self.scrolled.set_vscrollbar_policy(gtk::PolicyType::Never);
            self.scrolled
                .set_hscrollbar_policy(gtk::PolicyType::External);
            self.scrolled.set_propagate_natural_width(true);
            self.scrolled.set_child(Some(&self.list_view));

            self.obj().set_child(Some(&self.scrolled));
        }
    }

    impl WidgetImpl for LpThumbnailStrip {
        /// A strip wider than the window fades out at both ends
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let obj = self.obj();
            let adjustment = self.scrolled.hadjustment();
            if adjustment.upper() <= adjustment.page_size() + 1. {
                self.parent_snapshot(snapshot);
                return;
            }
            let (width, height) = (obj.width() as f32, obj.height() as f32);
            let fade = (EDGE_FADE / width.max(1.)).min(0.5);
            let clear = gdk::RGBA::new(0., 0., 0., 0.);
            let solid = gdk::RGBA::new(0., 0., 0., 1.);
            snapshot.push_mask(gsk::MaskMode::Alpha);
            snapshot.append_linear_gradient(
                &graphene::Rect::new(0., 0., width, height),
                &graphene::Point::new(0., 0.),
                &graphene::Point::new(width, 0.),
                &[
                    gsk::ColorStop::new(0., clear),
                    gsk::ColorStop::new(fade, solid),
                    gsk::ColorStop::new(1. - fade, solid),
                    gsk::ColorStop::new(1., clear),
                ],
            );
            snapshot.pop();
            self.parent_snapshot(snapshot);
            snapshot.pop();
        }
    }
    impl BinImpl for LpThumbnailStrip {}

    impl LpThumbnailStrip {
        fn setup_item(&self, item: &gtk::ListItem) {
            let obj = self.obj();
            item.set_activatable(false);
            item.set_selectable(false);
            item.set_focusable(false);
            self.list_items.borrow_mut().push(item.downgrade());

            let picture = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Cover)
                .width_request(THUMBNAIL_SIZE)
                .height_request(THUMBNAIL_SIZE)
                .build();
            picture.add_css_class("thumbnail-picture");

            let button = gtk::Button::builder().child(&picture).build();
            button.add_css_class("thumbnail");
            // Rounded corners also for the picture
            button.set_overflow(gtk::Overflow::Hidden);
            button.add_css_class("flat");
            button.connect_clicked(glib::clone!(
                #[weak]
                obj,
                #[weak]
                item,
                move |_| {
                    if let Some(file) = item.item().and_downcast::<gio::File>() {
                        obj.emit_by_name::<()>("activate-file", &[&file]);
                    }
                }
            ));

            let close = gtk::Button::builder()
                .icon_name("glance-close-symbolic")
                .tooltip_text(gettext("Remove From Set"))
                .halign(gtk::Align::End)
                .valign(gtk::Align::Start)
                .build();
            close.add_css_class("circular");
            close.add_css_class("osd");
            close.add_css_class("thumbnail-close");
            close.connect_clicked(glib::clone!(
                #[weak]
                obj,
                #[weak]
                item,
                move |_| {
                    if let Some(file) = item.item().and_downcast::<gio::File>() {
                        obj.emit_by_name::<()>("remove-file", &[&file]);
                    }
                }
            ));

            let overlay = gtk::Overlay::new();
            // Smaller thumbnails stay centered next to the larger current one
            overlay.set_valign(gtk::Align::Center);
            overlay.set_child(Some(&button));
            overlay.add_overlay(&close);
            item.set_child(Some(&overlay));
        }

        fn bind_item(&self, item: &gtk::ListItem) {
            let Some(file) = item.item().and_downcast::<gio::File>() else {
                return;
            };
            let Some(overlay) = item.child().and_downcast::<gtk::Overlay>() else {
                return;
            };
            let Some(button) = overlay.child().and_downcast::<gtk::Button>() else {
                return;
            };
            let Some(picture) = button.child().and_downcast::<gtk::Picture>() else {
                return;
            };
            button.set_tooltip_text(util::get_file_display_name(&file).as_deref());
            let uri = file.uri().to_string();
            let cached = self.cache.borrow().get(&uri).cloned();
            picture.set_paintable(cached.as_ref());
            self.style_item(item);
            if cached.is_some() {
                return;
            }

            let obj = self.obj();
            glib::spawn_future_local(glib::clone!(
                #[weak]
                obj,
                #[weak]
                item,
                #[weak]
                picture,
                async move {
                    let still_bound = |item: &gtk::ListItem| {
                        item.item()
                            .and_downcast::<gio::File>()
                            .is_some_and(|x| x.equal(&file))
                    };

                    glib::timeout_future(LOAD_DELAY).await;
                    if !still_bound(&item) {
                        return;
                    }

                    let size = THUMBNAIL_SIZE * picture.scale_factor() * 2;
                    match load_thumbnail(file.clone(), size as u32).await {
                        Ok(texture) => {
                            let mut cache = obj.imp().cache.borrow_mut();
                            if cache.len() >= CACHE_LIMIT {
                                cache.clear();
                            }
                            cache.insert(uri, texture.clone());
                            drop(cache);
                            if still_bound(&item) {
                                picture.set_paintable(Some(&texture));
                                // The current photo's width follows its shape
                                obj.imp().style_item(&item);
                            }
                        }
                        Err(err) => tracing::debug!("No thumbnail: {err}"),
                    }
                }
            ));
        }

        /// Applies current, compact and removable to a bound item
        pub(super) fn style_item(&self, item: &gtk::ListItem) {
            let Some(file) = item.item().and_downcast::<gio::File>() else {
                return;
            };
            let Some(overlay) = item.child().and_downcast::<gtk::Overlay>() else {
                return;
            };

            let current = self.current.borrow().as_ref().is_some_and(|x| x.equal(&file));
            if current {
                overlay.add_css_class("current");
            } else {
                overlay.remove_css_class("current");
            }

            if let Some(close) = overlay.last_child() {
                close.set_visible(self.removable.get());
            }

            let picture = overlay
                .child()
                .and_downcast::<gtk::Button>()
                .and_then(|button| button.child())
                .and_downcast::<gtk::Picture>();
            let Some(picture) = picture else {
                return;
            };
            let (width, height) = match (self.compact.get(), current) {
                (true, false) => (COMPACT_SIZE, COMPACT_HEIGHT),
                // The current photo shows its real shape
                (true, true) => (self.current_width(&picture), COMPACT_HEIGHT),
                (false, _) => (THUMBNAIL_SIZE, THUMBNAIL_SIZE),
            };
            picture.set_size_request(width, height);
        }

        /// Width of the current photo in the compact strip, from its shape
        fn current_width(&self, picture: &gtk::Picture) -> i32 {
            let aspect = picture
                .paintable()
                .map(|p| p.intrinsic_aspect_ratio())
                .filter(|aspect| *aspect > 0.)
                .unwrap_or(1.);
            ((COMPACT_HEIGHT as f64 * aspect).round() as i32).clamp(COMPACT_SIZE, COMPACT_CURRENT_MAX)
        }

        /// Limits the width to the thumbnails
        ///
        /// The list view's own natural width is an estimate that is too large for
        /// few thumbnails, which moves them off center.
        pub(super) fn update_width(&self) {
            let n = self.store.n_items() as i32;
            let width = if n == 0 {
                -1
            } else if self.compact.get() {
                n * (COMPACT_SIZE + COMPACT_ITEM_SPACE) + (COMPACT_CURRENT_MAX - COMPACT_SIZE)
                    + COMPACT_CURRENT_SPACE
            } else {
                n * (THUMBNAIL_SIZE + ITEM_SPACE)
            };
            self.scrolled.set_max_content_width(width);
        }

        /// Restyles all bound items
        pub(super) fn style_items(&self) {
            self.list_items.borrow_mut().retain(|item| {
                let Some(item) = item.upgrade() else {
                    return false;
                };
                self.style_item(&item);
                true
            });
        }

        pub(super) fn position_of(&self, file: &gio::File) -> Option<u32> {
            (0..self.store.n_items()).find(|&i| {
                self.store
                    .item(i)
                    .and_downcast::<gio::File>()
                    .is_some_and(|x| x.equal(file))
            })
        }
    }
}

use crate::util;

async fn load_thumbnail(file: gio::File, size: u32) -> anyhow::Result<gdk::Texture> {
    if crate::widgets::image_page::is_video(&file) {
        return crate::widgets::video_player::video_frame(&file, None, size).await;
    }
    let mut image = glycin::Loader::new(file).load().await?;
    let frame = image
        .specific_frame(glycin::FrameRequest::new().scale(size, size))
        .await?;
    Ok(frame.texture())
}

glib::wrapper! {
    pub struct LpThumbnailStrip(ObjectSubclass<imp::LpThumbnailStrip>)
    @extends gtk::Widget, adw::Bin,
    @implements gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

impl Default for LpThumbnailStrip {
    fn default() -> Self {
        glib::Object::new()
    }
}

impl LpThumbnailStrip {
    /// Shows thumbnails for `files`
    pub fn set_files(&self, files: &[gio::File]) {
        let imp = self.imp();
        let unchanged = imp.store.n_items() as usize == files.len()
            && files.iter().enumerate().all(|(i, file)| {
                imp.store
                    .item(i as u32)
                    .and_downcast::<gio::File>()
                    .is_some_and(|x| x.equal(file))
            });
        if unchanged {
            return;
        }

        imp.store.splice(0, imp.store.n_items(), files);
        imp.update_width();
        if files.is_empty() {
            imp.cache.borrow_mut().clear();
        }
    }

    /// Shows close buttons that remove images from the set
    pub fn set_removable(&self, removable: bool) {
        let imp = self.imp();
        imp.removable.set(removable);
        imp.style_items();
    }

    /// Uses small thumbnails, only the current one is larger
    pub fn set_compact(&self, compact: bool) {
        let imp = self.imp();
        imp.compact.set(compact);
        if compact {
            self.add_css_class("compact");
        } else {
            self.remove_css_class("compact");
        }
        imp.update_width();
        imp.style_items();
    }

    /// Highlights the thumbnail of `file`
    pub fn set_current(&self, file: Option<&gio::File>) {
        let imp = self.imp();
        let old = imp.current.replace(file.cloned());
        if old.as_ref().zip(file).is_some_and(|(a, b)| a.equal(b)) {
            return;
        }

        imp.style_items();
        if let Some(position) = file.and_then(|file| imp.position_of(file)) {
            // Keep the shown image's thumbnail in view
            glib::idle_add_local_once(glib::clone!(
                #[weak(rename_to = obj)]
                self,
                move || {
                    obj.imp()
                        .list_view
                        .scroll_to(position, gtk::ListScrollFlags::NONE, None)
                }
            ));
        }
    }

    pub fn connect_activate_file<F: Fn(&Self, &gio::File) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "activate-file",
            false,
            glib::closure_local!(move |obj: &Self, file: &gio::File| f(obj, file)),
        )
    }

    pub fn connect_remove_file<F: Fn(&Self, &gio::File) + 'static>(
        &self,
        f: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            "remove-file",
            false,
            glib::closure_local!(move |obj: &Self, file: &gio::File| f(obj, file)),
        )
    }
}
