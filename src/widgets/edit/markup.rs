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

//! Bottom bar of the Markup view: tools and colour, with an options pill above

use std::cell::{Cell, RefCell};

use adw::prelude::*;
use adw::subclass::prelude::*;

use crate::deps::*;
use crate::editing::markup::{COLORS, Color, Font, HIGHLIGHTER_ALPHA, Tool};
use crate::util::gettext::*;
use crate::widgets::edit::frost::PageFade;
use crate::widgets::edit::{LpCenterSlider, LpColorCard};

const TOOLS: [(&str, Tool); 9] = [
    ("select", Tool::Select),
    ("brush", Tool::Brush),
    ("highlighter", Tool::Highlighter),
    ("line", Tool::Line),
    ("arrow", Tool::Arrow),
    ("rectangle", Tool::Rectangle),
    ("ellipse", Tool::Ellipse),
    ("text", Tool::Text),
    ("eraser", Tool::Eraser),
];

const FONTS: [(&str, Font); 4] = [
    ("sans", Font::Sans),
    ("serif", Font::Serif),
    ("mono", Font::Mono),
    ("hand", Font::Hand),
];

pub fn font_from_name(name: &str) -> Option<Font> {
    FONTS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, font)| *font)
}

/// CSS class names of [`COLORS`], in the same order
const COLOR_NAMES: [&str; 5] = ["blue", "yellow", "red", "orange", "green"];

/// The size slider covers factors from 1 / `FACTOR_RANGE` to `FACTOR_RANGE`
const FACTOR_RANGE: f64 = 4.;

/// Size factor of a size slider value from -1 to 1, on a log scale so that
/// 0 is the default size
pub fn factor_from_slider(value: f64) -> f64 {
    FACTOR_RANGE.powf(value.clamp(-1., 1.))
}

/// Size slider value of a size factor
pub fn slider_from_factor(factor: f64) -> f64 {
    (factor.max(f64::MIN_POSITIVE).ln() / FACTOR_RANGE.ln()).clamp(-1., 1.)
}

fn color_label(name: &str) -> String {
    match name {
        "blue" => gettext("Blue"),
        "yellow" => gettext("Yellow"),
        "red" => gettext("Red"),
        "orange" => gettext("Orange"),
        "green" => gettext("Green"),
        _ => gettext("Custom"),
    }
}

fn font_label(name: &str) -> String {
    match name {
        "serif" => gettext("Serif"),
        "mono" => gettext("Monospace"),
        "hand" => gettext("Handwriting"),
        _ => gettext("Sans"),
    }
}

pub fn tool_from_name(name: &str) -> Option<Tool> {
    TOOLS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, tool)| *tool)
}

/// How far a stroke has grown, from 0 for 1 px to 1 for 48 px or more
fn growth(thickness: f64) -> f64 {
    (thickness.max(1.).ln() / 48_f64.ln()).clamp(0., 1.)
}

/// Stroke width of a preview: grows from 1.5 px with the real thickness
/// up to `cap`, so that a thick stroke never fills the small box
fn preview_width(thickness: f64, cap: f64) -> f64 {
    1.5 + (cap.max(1.5) - 1.5) * growth(thickness)
}

/// Draws the mark of `tool` in a box of `width` × `height`, as a preview of
/// what the tool draws with `thickness` pixels
///
/// The previews are drawn in `color`, which is the foreground, never the
/// colour of the marks. The brush is one soft wave so that it differs from
/// the straight line.
fn draw_mark(
    cr: &cairo::Context,
    width: f64,
    height: f64,
    tool: Tool,
    thickness: f64,
    (red, green, blue): (f64, f64, f64),
    filled: bool,
) {
    let max = (height - 4.).max(1.);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_source_rgb(red, green, blue);
    match tool {
        Tool::Brush => {
            let t = preview_width(thickness, height * 0.2);
            let x0 = t / 2. + 2.;
            let x1 = width - x0;
            let middle = height / 2.;
            let amplitude = ((height - t) / 2. - 1.5).max(1.) * 0.7;
            let reach = (x1 - x0) / 2.;
            cr.set_line_width(t);
            cr.move_to(x0, middle + amplitude);
            cr.curve_to(
                x0 + reach,
                middle + amplitude,
                x1 - reach,
                middle - amplitude,
                x1,
                middle - amplitude,
            );
            let _ = cr.stroke();
        }
        Tool::Highlighter => {
            // A short slanted swipe of a marker, flat at both ends
            let t = preview_width(thickness, height * 0.3).max(3.);
            cr.set_source_rgba(red, green, blue, HIGHLIGHTER_ALPHA);
            cr.set_line_cap(cairo::LineCap::Butt);
            cr.set_line_width(t);
            let inset = t / 2. + 3.;
            cr.move_to(inset, height * 0.7);
            cr.line_to(width - inset, height * 0.3);
            let _ = cr.stroke();
        }
        Tool::Line | Tool::Arrow => {
            let t = preview_width(thickness, height * 0.2);
            let inset = (t / 2. + 3.).min(width / 2. - 1.);
            let (from, to) = ((inset, height - inset), (width - inset, inset));
            cr.set_line_width(t);
            if tool == Tool::Line {
                cr.move_to(from.0, from.1);
                cr.line_to(to.0, to.1);
                let _ = cr.stroke();
            } else {
                let (dx, dy) = (to.0 - from.0, to.1 - from.1);
                let length = dx.hypot(dy);
                let head = (t * 4.).min(length * 0.6);
                let angle = dy.atan2(dx);
                let spread = 0.45;
                let shaft = (length - head * 0.6) / length;
                cr.move_to(from.0, from.1);
                cr.line_to(from.0 + dx * shaft, from.1 + dy * shaft);
                let _ = cr.stroke();
                cr.move_to(to.0, to.1);
                cr.line_to(
                    to.0 - head * (angle - spread).cos(),
                    to.1 - head * (angle - spread).sin(),
                );
                cr.line_to(
                    to.0 - head * (angle + spread).cos(),
                    to.1 - head * (angle + spread).sin(),
                );
                cr.close_path();
                let _ = cr.fill_preserve();
                cr.set_line_width(t * 0.5);
                let _ = cr.stroke();
            }
        }
        Tool::Rectangle | Tool::Ellipse => {
            let t = preview_width(thickness, height * 0.2);
            cr.set_line_width(t);
            if tool == Tool::Rectangle {
                let margin_x = t / 2. + 2.;
                let margin_y = t / 2. + height * 0.12;
                cr.set_line_join(cairo::LineJoin::Miter);
                cr.rectangle(
                    margin_x,
                    margin_y,
                    width - 2. * margin_x,
                    height - 2. * margin_y,
                );
            } else {
                // A circle, however wide the box is
                let diameter = (width.min(height) - t - 4.).max(2.);
                cr.arc(
                    width / 2.,
                    height / 2.,
                    diameter / 2.,
                    0.,
                    std::f64::consts::TAU,
                );
            }
            if filled {
                let _ = cr.fill_preserve();
            }
            let _ = cr.stroke();
        }
        Tool::Eraser => {
            let diameter = 4. + (max - 4.).max(0.) * growth(thickness);
            cr.set_line_width(1.5);
            cr.arc(
                width / 2.,
                height / 2.,
                (diameter - 1.5) / 2.,
                0.,
                std::f64::consts::TAU,
            );
            let _ = cr.stroke();
        }
        Tool::Select | Tool::Text => {}
    }
}

mod imp {
    use super::*;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(file = "markup.ui")]
    pub struct LpEditMarkup {
        #[template_child]
        pub(super) custom_swatch: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub(super) color_popover: TemplateChild<gtk::Popover>,
        #[template_child]
        pub(super) color_card: TemplateChild<LpColorCard>,
        #[template_child]
        pub(super) swatch_blue: TemplateChild<gtk::Button>,
        #[template_child]
        pub(super) swatch_yellow: TemplateChild<gtk::Button>,
        #[template_child]
        pub(super) swatch_red: TemplateChild<gtk::Button>,
        #[template_child]
        pub(super) swatch_orange: TemplateChild<gtk::Button>,
        #[template_child]
        pub(super) swatch_green: TemplateChild<gtk::Button>,
        #[template_child]
        pub(super) chips_box: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) color_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub(super) color_dot: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub(super) options_button: TemplateChild<gtk::ToggleButton>,
        /// Holds the options and the colours, which share one place
        #[template_child]
        pub(super) slot: TemplateChild<gtk::Grid>,
        #[template_child]
        pub(super) options_page: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) colors_page: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) chip_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub(super) chip_face: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub(super) chip_text: TemplateChild<gtk::Label>,
        #[template_child]
        pub(super) preview_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub(super) preview: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub(super) preview_text: TemplateChild<gtk::Label>,
        #[template_child]
        pub(super) size_slider: TemplateChild<LpCenterSlider>,
        #[template_child]
        pub(super) size_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub(super) fill_sep: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) fill_row: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) fill_outline_icon: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub(super) fill_filled_icon: TemplateChild<gtk::DrawingArea>,
        #[template_child]
        pub(super) font_sep: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) font_row: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) background_sep: TemplateChild<gtk::Widget>,
        #[template_child]
        pub(super) background_button: TemplateChild<gtk::Widget>,

        /// What the slot shows: the options (0), the colours (1) or nothing
        pub(super) slot_page: Cell<Option<usize>>,
        pub(super) fade: PageFade,
        pub(super) tool: Cell<Tool>,
        /// Size of new shapes of the tool at factor 1, in image pixels
        pub(super) base_size: Cell<f64>,
        /// Screen pixels of one image pixel
        pub(super) screen_scale: Cell<f64>,
        /// Size factor in use, from the size slider
        pub(super) factor: Cell<f64>,
        /// New rectangles and ellipses are filled
        pub(super) filled: Cell<bool>,
        /// The size slider is set from the code
        pub(super) updating: Cell<bool>,
        /// Name of the font in use
        pub(super) font: RefCell<String>,

        /// Colour picked in the colour card
        pub(super) custom: Cell<Option<Color>>,
        pub(super) selected: RefCell<String>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LpEditMarkup {
        const NAME: &'static str = "LpEditMarkup";
        type Type = super::LpEditMarkup;
        type ParentType = adw::Bin;

        fn class_init(klass: &mut Self::Class) {
            LpColorCard::static_type();
            LpCenterSlider::static_type();
            Self::bind_template(klass);
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for LpEditMarkup {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();

            // Keep a gap to the pill below
            self.color_popover.set_offset(0, -10);
            self.fade.show_now(&self.pages(), None);
            self.size_slider
                .update_relation(&[gtk::accessible::Relation::DescribedBy(&[self
                    .size_label
                    .upcast_ref::<gtk::Accessible>(
                )])]);

            self.color_dot.set_draw_func(glib::clone!(
                #[weak]
                obj,
                move |_, cr, width, height| obj.imp().draw_color_dot(cr, width, height)
            ));
            self.base_size.set(6.);
            self.screen_scale.set(1.);
            self.factor.set(1.);
            self.font.replace("sans".to_owned());
            self.update_options();

            for area in [&*self.preview, &*self.chip_face] {
                area.set_draw_func(glib::clone!(
                    #[weak]
                    obj,
                    move |area, cr, width, height| obj.imp().draw_preview(
                        area,
                        cr,
                        width as f64,
                        height as f64
                    )
                ));
            }
            for (area, filled) in [
                (&*self.fill_outline_icon, false),
                (&*self.fill_filled_icon, true),
            ] {
                area.set_draw_func(glib::clone!(
                    #[weak]
                    obj,
                    move |area, cr, width, height| obj.imp().draw_fill_icon(
                        area,
                        cr,
                        width as f64,
                        height as f64,
                        filled
                    )
                ));
            }

            self.size_slider.connect_value_notify(glib::clone!(
                #[weak]
                obj,
                move |slider| {
                    if !obj.imp().updating.get() {
                        let factor = factor_from_slider(slider.value());
                        let _ = obj.activate_action("edit-markup.size", Some(&factor.to_variant()));
                    }
                }
            ));
            // Options and colours share one place, so only one is open
            self.options_button.connect_toggled(glib::clone!(
                #[weak]
                obj,
                move |button| {
                    let imp = obj.imp();
                    if button.is_active() {
                        imp.color_button.set_active(false);
                    }
                    imp.sync_pills();
                }
            ));
            self.color_button.connect_toggled(glib::clone!(
                #[weak]
                obj,
                move |button| {
                    let imp = obj.imp();
                    if button.is_active() {
                        imp.options_button.set_active(false);
                    }
                    imp.sync_pills();
                }
            ));

            self.custom_swatch.set_draw_func(glib::clone!(
                #[weak]
                obj,
                move |_, cr, width, height| obj.imp().draw_custom(cr, width, height)
            ));

            // Opening the card starts from and selects the custom colour
            self.color_popover.connect_show(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    let imp = obj.imp();
                    match imp.custom.get() {
                        // Setting the same colour again would lose the hue
                        // of greys
                        Some(color) if color != imp.color_card.color() => {
                            imp.color_card.set_color(color)
                        }
                        Some(_) => {}
                        None => imp.custom.set(Some(imp.color_card.color())),
                    }
                    imp.select_custom();
                }
            ));
            self.color_card.connect_pick_requested(glib::clone!(
                #[weak]
                obj,
                move |_| {
                    let imp = obj.imp();
                    imp.color_popover.popdown();
                    imp.refocus(&imp.colors_page, &imp.color_button);
                    imp.color_button.set_active(false);
                    let _ = obj.activate_action("edit-markup.pick-color", None);
                }
            ));
            self.color_card.connect_color_changed(glib::clone!(
                #[weak]
                obj,
                move |card| {
                    let imp = obj.imp();
                    imp.custom.set(Some(card.color()));
                    imp.select_custom();
                }
            ));
        }
    }

    impl WidgetImpl for LpEditMarkup {}
    impl BinImpl for LpEditMarkup {}

    impl LpEditMarkup {
        /// The pills of the slot, see `slot_page`
        pub(super) fn pages(&self) -> [gtk::Widget; 2] {
            [
                self.options_page.get().upcast(),
                self.colors_page.get().upcast(),
            ]
        }

        fn swatches(&self) -> [(&str, &gtk::Button); 5] {
            [
                ("blue", &*self.swatch_blue),
                ("yellow", &*self.swatch_yellow),
                ("red", &*self.swatch_red),
                ("orange", &*self.swatch_orange),
                ("green", &*self.swatch_green),
            ]
        }

        fn draw_swatches(&self) {
            self.custom_swatch.queue_draw();
            self.color_dot.queue_draw();
        }

        /// Colour of the preview marks: the foreground of the widget, so
        /// they show the size and nothing else
        fn mark_color(&self, area: &gtk::DrawingArea) -> (f64, f64, f64) {
            let foreground = WidgetExt::color(area);
            (
                foreground.red() as f64,
                foreground.green() as f64,
                foreground.blue() as f64,
            )
        }

        /// Size of new shapes in image pixels
        fn image_size(&self) -> f64 {
            self.base_size.get() * self.factor.get()
        }

        fn draw_preview(
            &self,
            area: &gtk::DrawingArea,
            cr: &cairo::Context,
            width: f64,
            height: f64,
        ) {
            // The real thickness on the screen
            let thickness = self.image_size() * self.screen_scale.get();
            draw_mark(
                cr,
                width,
                height,
                self.tool.get(),
                thickness,
                self.mark_color(area),
                self.filled.get(),
            );
        }

        /// Small rectangle or ellipse, outlined or solid, of the current shape
        fn draw_fill_icon(
            &self,
            area: &gtk::DrawingArea,
            cr: &cairo::Context,
            width: f64,
            height: f64,
            filled: bool,
        ) {
            let foreground = WidgetExt::color(area);
            let tool = if self.tool.get() == Tool::Ellipse {
                Tool::Ellipse
            } else {
                Tool::Rectangle
            };
            draw_mark(
                cr,
                width,
                height,
                tool,
                1.5,
                (
                    foreground.red() as f64,
                    foreground.green() as f64,
                    foreground.blue() as f64,
                ),
                filled,
            );
        }

        /// Shows the options or the colours, or neither: the options show
        /// when the options chip is on, except for the select tool
        pub(super) fn sync_pills(&self) {
            let colors = self.color_button.is_active() && self.color_button.is_visible();
            let options = self.options_button.is_active() && self.tool.get() != Tool::Select;
            let target = if colors {
                Some(1)
            } else if options {
                Some(0)
            } else {
                None
            };
            if self.slot_page.get() == target {
                return;
            }
            self.slot_page.set(target);
            if target.is_some() {
                self.slot.set_visible(true);
            }
            self.fade.show(
                &self.pages(),
                target,
                glib::clone!(
                    #[weak(rename_to = obj)]
                    self.obj(),
                    move || {
                        // The slot takes no room once it has faded out
                        let imp = obj.imp();
                        if imp.slot_page.get().is_none() {
                            imp.slot.set_visible(false);
                        }
                    }
                ),
            );
        }

        /// Moves the focus to `button` if it is inside `page`, which is about
        /// to fade out
        pub(super) fn refocus(&self, page: &gtk::Widget, button: &gtk::ToggleButton) {
            let inside = self
                .obj()
                .root()
                .and_then(|root| root.focus())
                .is_some_and(|focus| focus.is_ancestor(page));
            if inside {
                button.grab_focus();
            }
        }

        /// Updates the number, the previews and the accessible labels
        pub(super) fn update_options(&self) {
            let tool = self.tool.get();
            let size = self.image_size().round().max(1.) as i64;
            self.size_label.set_label(&size.to_string());

            let font = font_label(self.font.borrow().as_str());
            let (name, label) = match tool {
                Tool::Text => (
                    gettext("Text Style"),
                    gettext_f(
                        // Translators: Accessible name of the options button for text, {} is the font name and the size in pixels
                        "Text Style: {}, {}",
                        [font, size.to_string()],
                    ),
                ),
                Tool::Eraser => (
                    gettext("Eraser Size"),
                    gettext_f(
                        // Translators: Accessible name of the options button for the eraser, {} is the size in pixels
                        "Eraser Size: {}",
                        [size],
                    ),
                ),
                _ => (
                    gettext("Line Width"),
                    gettext_f(
                        // Translators: Accessible name of the options button for lines and shapes, {} is the width in pixels
                        "Line Width: {}",
                        [size],
                    ),
                ),
            };
            self.options_button.set_tooltip_text(Some(&name));
            self.options_button
                .update_property(&[gtk::accessible::Property::Label(&label)]);
            let slider_name = if tool == Tool::Text {
                gettext("Text Size")
            } else {
                name
            };
            self.size_slider.set_tooltip_text(Some(&slider_name));
            // Translators: Value of the size slider read by screen readers, {} is the size in pixels
            let value_text = ngettext("{} pixel", "{} pixels", size as u32);
            self.size_slider.update_property(&[
                gtk::accessible::Property::Label(&slider_name),
                gtk::accessible::Property::ValueText(&value_text),
            ]);

            self.preview.queue_draw();
            self.chip_face.queue_draw();
        }

        /// Colour in use, from a swatch or picked
        fn current_color(&self) -> Option<Color> {
            let selected = self.selected.borrow();
            if selected.as_str() == "custom" {
                return self.custom.get();
            }
            COLOR_NAMES
                .iter()
                .position(|n| *n == selected.as_str())
                .map(|index| COLORS[index])
        }

        fn draw_color_dot(&self, cr: &cairo::Context, width: i32, height: i32) {
            let Some(color) = self.current_color() else {
                return;
            };
            cr.set_source_rgb(
                color.r as f64 / 255.,
                color.g as f64 / 255.,
                color.b as f64 / 255.,
            );
            cr.arc(
                width as f64 / 2.,
                height as f64 / 2.,
                width.min(height) as f64 / 2.,
                0.,
                std::f64::consts::TAU,
            );
            let _ = cr.fill();
        }

        /// Accessible name of the colour chip, with the value it shows
        pub(super) fn update_labels(&self) {
            let color = gettext_f("Color: {}", [color_label(self.selected.borrow().as_str())]);
            self.color_button
                .update_property(&[gtk::accessible::Property::Label(&color)]);
        }

        pub(super) fn select_custom(&self) {
            self.draw_swatches();
            let _ = self
                .obj()
                .activate_action("edit-markup.color", Some(&"custom".to_variant()));
        }

        pub(super) fn show_color(&self, name: &str) {
            self.selected.replace(name.to_owned());
            for (swatch_name, button) in self.swatches() {
                if swatch_name == name {
                    button.add_css_class("selected");
                } else {
                    button.remove_css_class("selected");
                }
            }
            self.draw_swatches();
            self.update_labels();
        }

        /// Dashed ring with a plus while empty; after picking, one solid
        /// ring around the colour, white while the colour is in use
        fn draw_custom(&self, cr: &cairo::Context, width: i32, height: i32) {
            let (cx, cy) = (width as f64 / 2., height as f64 / 2.);
            let radius = cx.min(cy) - 1.5;
            let tau = std::f64::consts::TAU;
            let foreground = WidgetExt::color(&*self.obj());

            match self.custom.get() {
                None => {
                    cr.set_dash(&[3., 3.], 0.);
                    cr.set_line_width(1.5);
                    cr.set_source_rgba(0.42, 0.42, 0.45, 1.);
                    cr.arc(cx, cy, radius - 1., 0., tau);
                    let _ = cr.stroke();
                    cr.set_dash(&[], 0.);
                    cr.set_source_rgba(
                        foreground.red() as f64,
                        foreground.green() as f64,
                        foreground.blue() as f64,
                        0.8,
                    );
                }
                Some(color) => {
                    let in_use = self.selected.borrow().as_str() == "custom";
                    cr.set_line_width(2.);
                    if in_use {
                        cr.set_source_rgb(1., 1., 1.);
                    } else {
                        cr.set_source_rgba(0.42, 0.42, 0.45, 1.);
                    }
                    cr.arc(cx, cy, radius - 1., 0., tau);
                    let _ = cr.stroke();
                    cr.set_source_rgb(
                        color.r as f64 / 255.,
                        color.g as f64 / 255.,
                        color.b as f64 / 255.,
                    );
                    cr.arc(cx, cy, radius - 4., 0., tau);
                    let _ = cr.fill();
                    // Plus that stays readable on light colours
                    let luma =
                        0.2126 * color.r as f64 + 0.7152 * color.g as f64 + 0.0722 * color.b as f64;
                    let shade = if luma > 170. { 0. } else { 1. };
                    cr.set_source_rgb(shade, shade, shade);
                }
            }

            // Plus, centred on whole pixels
            let arm = 4.5;
            let (px, py) = (cx.round(), cy.round());
            cr.set_line_width(2.);
            cr.set_line_cap(cairo::LineCap::Round);
            cr.move_to(px - arm, py);
            cr.line_to(px + arm, py);
            cr.move_to(px, py - arm);
            cr.line_to(px, py + arm);
            let _ = cr.stroke();
        }
    }
}

glib::wrapper! {
    pub struct LpEditMarkup(ObjectSubclass<imp::LpEditMarkup>)
    @extends gtk::Widget, adw::Bin,
    @implements gtk::Buildable, gtk::Accessible, gtk::ConstraintTarget;
}

impl LpEditMarkup {
    /// Colour for a swatch name, including the picked custom colour
    pub fn color(&self, name: &str) -> Option<Color> {
        if name == "custom" {
            return self.imp().custom.get();
        }
        COLOR_NAMES
            .iter()
            .position(|n| *n == name)
            .map(|index| COLORS[index])
    }

    /// Uses `color` as the custom colour and selects it
    pub fn set_custom_color(&self, color: Color) {
        let imp = self.imp();
        imp.custom.set(Some(color));
        if color != imp.color_card.color() {
            imp.color_card.set_color(color);
        }
        imp.select_custom();
    }

    /// Marks the swatch of the selected colour
    pub fn show_color(&self, name: &str) {
        self.imp().show_color(name);
    }

    /// Shows only the chips that `tool` uses
    pub fn set_tool(&self, tool: Tool) {
        let imp = self.imp();
        let previous = imp.tool.replace(tool);

        imp.chips_box.set_visible(tool != Tool::Select);
        let has_color = !matches!(tool, Tool::Select | Tool::Eraser);
        imp.color_button.set_visible(has_color);
        if !has_color {
            imp.color_button.set_active(false);
        }

        let text = tool == Tool::Text;
        let shape = matches!(tool, Tool::Rectangle | Tool::Ellipse);
        let page = if text { "text" } else { "draw" };
        imp.chip_stack.set_visible_child_name(page);
        imp.preview_stack.set_visible_child_name(page);
        imp.fill_sep.set_visible(shape);
        imp.fill_row.set_visible(shape);
        imp.fill_outline_icon.queue_draw();
        imp.fill_filled_icon.queue_draw();
        imp.font_sep.set_visible(text);
        imp.font_row.set_visible(text);
        imp.background_sep.set_visible(text);
        imp.background_button.set_visible(text);

        // Text is set up right after choosing it, so its options show
        if text && previous != Tool::Text {
            imp.options_button.set_active(true);
        }
        imp.sync_pills();
        imp.update_options();
        imp.update_labels();
    }

    /// Size of new shapes of the tool at size factor 1, in image pixels,
    /// and the screen pixels that one image pixel takes
    pub fn set_metrics(&self, base_size: f64, screen_scale: f64) {
        let imp = self.imp();
        if imp.base_size.replace(base_size) != base_size
            || imp.screen_scale.replace(screen_scale) != screen_scale
        {
            imp.update_options();
        }
    }

    /// Shows the size factor in use on the slider, the number and the previews
    pub fn show_size(&self, factor: f64) {
        let imp = self.imp();
        imp.factor.set(factor);
        imp.updating.set(true);
        imp.size_slider.set_value(slider_from_factor(factor));
        imp.updating.set(false);
        imp.update_options();
    }

    /// Shows if new rectangles and ellipses are filled
    pub fn show_fill(&self, filled: bool) {
        let imp = self.imp();
        imp.filled.set(filled);
        imp.preview.queue_draw();
        imp.chip_face.queue_draw();
    }

    /// Shows the font in use on the options chip and the text preview
    pub fn show_font(&self, name: &str) {
        let imp = self.imp();
        let old = imp.font.replace(name.to_owned());
        for label in [&*imp.chip_text, &*imp.preview_text] {
            label.remove_css_class(&format!("font-{old}"));
            label.add_css_class(&format!("font-{name}"));
        }
        imp.update_options();
    }

    /// Height of the bar with the options row, whether that is shown or not.
    /// The picture is fitted above it, so it does not jump when the row
    /// shows or hides.
    pub fn reserved_height(&self, for_size: i32) -> i32 {
        let imp = self.imp();
        let (_, natural, _, _) = self.measure(gtk::Orientation::Vertical, for_size);
        if imp.slot.is_visible() {
            return natural;
        }
        // The pills of a hidden slot are not measured with it
        let slot = imp
            .pages()
            .iter()
            .map(|page| page.measure(gtk::Orientation::Vertical, -1).1)
            .max()
            .unwrap_or(0);
        natural + 10 + slot
    }

    /// Closes the colours or the options pill, whichever is shown, and
    /// moves the focus to its chip. Returns whether one was shown.
    pub fn close_pills(&self) -> bool {
        let imp = self.imp();
        if imp.color_button.is_active() && imp.color_button.is_visible() {
            imp.color_button.set_active(false);
            imp.color_button.grab_focus();
            true
        } else if imp.options_button.is_active() && imp.tool.get() != Tool::Select {
            imp.options_button.set_active(false);
            imp.options_button.grab_focus();
            true
        } else {
            false
        }
    }

    /// Closes the colours, after one was picked
    pub fn close_colors(&self) {
        let imp = self.imp();
        imp.refocus(&imp.colors_page, &imp.color_button);
        imp.color_button.set_active(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_factor_mapping() {
        assert_eq!(factor_from_slider(0.), 1.);
        assert!((factor_from_slider(-1.) - 0.25).abs() < 1e-9);
        assert!((factor_from_slider(1.) - 4.).abs() < 1e-9);
        // Out of range values stay in range
        assert!((factor_from_slider(3.) - 4.).abs() < 1e-9);
        for value in [-1., -0.5, 0., 0.37, 1.] {
            assert!((slider_from_factor(factor_from_slider(value)) - value).abs() < 1e-9);
        }
        assert_eq!(slider_from_factor(100.), 1.);
        assert_eq!(slider_from_factor(0.), -1.);
    }
}
