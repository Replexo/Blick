// Copyright (c) 2020-2023 Christopher Davis
// Copyright (c) 2022-2026 Sophie Herold
// Copyright (c) 2023 Matteo Nardi
// Copyright (c) 2023 Julian Hofer
// Copyright (c) 2023 Huan Nguyen
// Copyright (c) 2024 Balló György
// Copyright (c) 2025 Abhinandan Thakur
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

use adw::prelude::*;
use adw::subclass::prelude::*;

use gettextrs::gettext;

use crate::config;
use crate::deps::*;
use crate::updates::{self, Release};
use crate::widgets::LpWindow;

mod imp {
    use super::*;
    use crate::widgets::LpShyBin;

    // The basic struct that holds our
    // state and widgets
    #[derive(Debug, glib::Properties)]
    #[properties(wrapper_type = super::LpApplication)]
    pub struct LpApplication {
        pub settings: gio::Settings,
        /// Watches the default apps for the menu item
        pub defaults_monitor: std::cell::RefCell<Option<gio::FileMonitor>>,
        /// A newer release found on GitHub
        pub update: std::cell::RefCell<Option<Release>>,
        #[property(get)]
        pub update_available: std::cell::Cell<bool>,
    }

    impl Default for LpApplication {
        fn default() -> Self {
            Self {
                settings: gio::Settings::new(config::APP_ID),
                defaults_monitor: Default::default(),
                update: Default::default(),
                update_available: Default::default(),
            }
        }
    }

    // Sets up the basics for the GObject
    // The `#[glib::object_subclass] macro implements
    // some boilerplate code for the object setup, e.g. get_type()
    #[glib::object_subclass]
    impl ObjectSubclass for LpApplication {
        const NAME: &'static str = "LpApplication";
        type Type = super::LpApplication;
        type ParentType = adw::Application;
    }

    #[glib::derived_properties]
    impl ObjectImpl for LpApplication {
        fn constructed(&self) {
            let obj = self.obj();

            self.parent_constructed();

            // Set up the actions
            obj.setup_actions();
        }
    }

    impl ApplicationImpl for LpApplication {
        fn startup(&self) {
            tracing::trace!("Startup");
            self.parent_startup();

            // Blick replaces Glance
            if let Err(err) = crate::default_app::take_over_from_glance() {
                tracing::debug!("Could not take over Glance's file types: {err:#}");
            }
            // On its first start Blick opens all images and videos; a
            // development build leaves them alone
            let settings = self.obj().settings();
            if !config::APP_ID.ends_with(".Devel") && !settings.boolean("made-default") {
                for kind in [crate::default_app::Kind::Image, crate::default_app::Kind::Video] {
                    if let Err(err) = crate::default_app::make_default(kind) {
                        tracing::warn!("Could not make Blick the default app: {err:#}");
                    }
                }
                let _ = settings.set_boolean("made-default", true);
            }

            // Force dark theme
            self.obj()
                .style_manager()
                .set_color_scheme(adw::ColorScheme::PreferDark);

            LpShyBin::ensure_type();
            gtk::Window::set_default_icon_name(config::APP_ID);

            if config::UPDATE_CHECK {
                // Shortly after starting, so opening the first image comes first
                glib::timeout_add_seconds_local_once(3, || {
                    glib::spawn_future_local(async {
                        if let Err(err) = super::LpApplication::default().check_for_updates().await
                        {
                            tracing::debug!("Could not check for updates: {err:#}");
                        }
                    });
                });
            }
        }

        fn activate(&self) {
            tracing::debug!("Showing window via 'activate'");
            let application = self.obj();
            let window = LpWindow::new(&*application);
            window.present();
        }

        // Handles opening files from the command line or other applications
        fn open(&self, files: &[gio::File], _hint: &str) {
            tracing::debug!("Open {} file(s)", files.len());
            let application = self.obj();
            let win = LpWindow::new(&*application);
            win.image_view().set_images_from_files(files.to_vec());
        }
    }

    // This is empty, but we still need to provide an
    // empty implementation for each type we subclass.
    impl GtkApplicationImpl for LpApplication {}
    impl AdwApplicationImpl for LpApplication {}
}

// Creates a wrapper struct that inherits the functions
// from objects listed as @extends or interfaces it @implements.
// This is what allows us to do e.g. application.quit() on
// LpApplication without casting.
glib::wrapper! {
    pub struct LpApplication(ObjectSubclass<imp::LpApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

// This is where the member functions of LpApplication go.
#[allow(clippy::new_without_default)]
impl LpApplication {
    pub fn new() -> Self {
        glib::Object::builder()
            .property("application-id", config::APP_ID)
            .property("flags", gio::ApplicationFlags::HANDLES_OPEN)
            .property("resource-base-path", "/io/github/Replexo/Blick")
            .build()
    }

    pub fn setup_actions(&self) {
        // gio::ActionEntryBuilder allows us to build and store an action on an object
        // that implements gio::ActionMap. Here we build the application's actions and
        // add them with add_action_entries().
        let actions = [
            gio::ActionEntryBuilder::new("help")
                .activate(|app: &Self, _, _| app.show_help())
                .build(),
            gio::ActionEntryBuilder::new("quit")
                .activate(|app: &Self, _, _| {
                    // Give all windows the chance to handle close-request
                    for win in app.windows() {
                        win.close();
                    }
                })
                .build(),
            gio::ActionEntryBuilder::new("make-default")
                .activate(|app: &Self, _, _| app.make_default(crate::default_app::Kind::Image))
                .build(),
            gio::ActionEntryBuilder::new("preferences")
                .activate(|app: &Self, _, _| {
                    crate::widgets::present_preferences(app.active_window().as_ref())
                })
                .build(),
            gio::ActionEntryBuilder::new("make-default-video")
                .activate(|app: &Self, _, _| app.make_default(crate::default_app::Kind::Video))
                .build(),
            gio::ActionEntryBuilder::new("updates")
                .activate(|app: &Self, _, _| {
                    if let Some(window) = app.active_window() {
                        crate::widgets::update_dialog::present(&window);
                    }
                })
                .build(),
            gio::ActionEntryBuilder::new("new-window")
                .activate(|app: &Self, _, _| {
                    let win = LpWindow::new(app);
                    tracing::debug!("Showing new window");
                    win.present();
                })
                .build(),
        ];

        self.add_action_entries(actions);
        self.update_make_default();
        self.watch_defaults();

        self.set_accels_for_action("app.help", &["F1"]);
        self.set_accels_for_action("app.preferences", &["<Ctrl>comma"]);
        self.set_accels_for_action("app.quit", &["<Ctrl>Q"]);
        self.set_accels_for_action("window.close", &["<Ctrl>W"]);
        self.set_accels_for_action("app.new-window", &["<Ctrl>N"]);
    }

    /// The newer release found on GitHub, if any
    pub fn update(&self) -> Option<Release> {
        self.imp().update.borrow().clone()
    }

    /// Asks GitHub for the latest release and remembers it when it is newer
    pub async fn check_for_updates(&self) -> anyhow::Result<Option<Release>> {
        let release = updates::latest_release().await?;
        let newer = release.is_newer();
        tracing::debug!("Latest release {}, newer: {newer}", release.version);

        let imp = self.imp();
        imp.update.replace(newer.then(|| release.clone()));
        if imp.update_available.replace(newer) != newer {
            self.notify_update_available();
        }
        if newer {
            self.remind_of_update(&release);
        }

        Ok(newer.then_some(release))
    }

    /// Makes Blick the default image viewer or video player, after the user
    /// asked for it
    fn make_default(&self, kind: crate::default_app::Kind) {
        use crate::default_app::Kind;
        let window = self.active_window().and_downcast::<LpWindow>();
        let (done, failed) = match kind {
            Kind::Image => (
                gettext("Blick is now the default image viewer"),
                gettext("Could not make Blick the default image viewer"),
            ),
            Kind::Video => (
                gettext("Blick is now the default video player"),
                gettext("Could not make Blick the default video player"),
            ),
        };
        match crate::default_app::make_default(kind) {
            Ok(_) => {
                if let Some(window) = window {
                    window.show_toast(&done, adw::ToastPriority::High);
                }
            }
            Err(err) => {
                tracing::error!("Failed to set the default app for {kind:?}: {err:#}");
                if let Some(window) = window {
                    window.show_toast(&failed, adw::ToastPriority::High);
                }
            }
        }
        self.update_make_default();
    }

    /// Greys out the menu items while Blick is the default already, or
    /// when the desktop file is not installed
    fn update_make_default(&self) {
        use crate::default_app::Kind;
        for (name, kind) in [("make-default", Kind::Image), ("make-default-video", Kind::Video)] {
            if let Some(action) = self.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                action.set_enabled(
                    crate::default_app::is_available() && !crate::default_app::is_default(kind),
                );
            }
        }
    }

    /// Follows default apps changed elsewhere, like in the system settings
    fn watch_defaults(&self) {
        let file = gio::File::for_path(crate::default_app::mimeapps_path());
        match file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
            Ok(monitor) => {
                monitor.connect_changed(glib::clone!(
                    #[weak(rename_to = app)]
                    self,
                    move |_, _, _, _| app.update_make_default()
                ));
                self.imp().defaults_monitor.replace(Some(monitor));
            }
            Err(err) => tracing::debug!("Not watching default apps: {err}"),
        }
    }

    pub fn show_help(&self) {
        let context = self
            .active_window()
            .map(|w| gtk::prelude::WidgetExt::display(&w).app_launch_context());
        glib::spawn_future_local(async move {
            if let Err(e) =
                gio::AppInfo::launch_default_for_uri_future("help:blick", context.as_ref()).await
            {
                tracing::error!("Failed to launch help: {}", e.message());
            }
        });
    }

    /// Tells about a new release in a toast, at most once a day
    fn remind_of_update(&self, release: &Release) {
        let settings = self.settings();
        let now = glib::DateTime::now_utc().map_or(0, |now| now.to_unix());
        if now < settings.int64("update-reminder") {
            return;
        }
        let Some(window) = self.active_window().and_downcast::<LpWindow>() else {
            return;
        };
        // Shown or not looked at, it comes back tomorrow until updated
        let _ = settings.set_int64("update-reminder", now + 24 * 60 * 60);

        let version = release.version.trim_start_matches(['v', 'V']);
        let toast = adw::Toast::builder().timeout(10).build();
        let later = gtk::Button::builder()
            .label(gettext("_Later"))
            .use_underline(true)
            .css_classes(["flat"])
            .build();
        later.connect_clicked(glib::clone!(
            #[weak]
            toast,
            move |_| toast.dismiss()
        ));
        let update = gtk::Button::builder()
            .label(gettext("_Update"))
            .use_underline(true)
            .action_name("app.updates")
            .build();
        update.connect_clicked(glib::clone!(
            #[weak]
            toast,
            move |_| toast.dismiss()
        ));
        let content = gtk::Box::builder().spacing(6).build();
        let label = gtk::Label::new(Some(
            // Translators: {} is a version number like 1.3
            &gettext("Blick {} is available").replace("{}", version),
        ));
        label.set_margin_end(6);
        content.append(&label);
        content.append(&later);
        content.append(&update);
        // Later and Update are the only choices, so no close button
        content.connect_map(|content| {
            let mut toast = content.parent();
            while let Some(widget) = toast.clone() {
                if widget.type_().name() == "AdwToastWidget" {
                    break;
                }
                toast = widget.parent();
            }
            let mut child = toast.and_then(|toast| toast.first_child());
            while let Some(widget) = child {
                if widget.is::<gtk::Button>() && widget.has_css_class("circular") {
                    widget.set_visible(false);
                }
                child = widget.next_sibling();
            }
        });
        toast.set_custom_title(Some(&content));
        window.add_toast(toast);
    }

    pub fn settings(&self) -> gio::Settings {
        self.imp().settings.clone()
    }
}

impl Default for LpApplication {
    fn default() -> Self {
        gio::Application::default()
            .unwrap()
            .downcast::<LpApplication>()
            .unwrap()
    }
}
