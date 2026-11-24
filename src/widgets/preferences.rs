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

//! Preferences: video playback and the optional OpenSubtitles login

use adw::prelude::*;

use crate::application::LpApplication;
use crate::deps::*;
use crate::opensubtitles;
use crate::util::gettext::*;

pub fn present(parent: Option<&gtk::Window>) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_search_enabled(false);

    let page = adw::PreferencesPage::new();
    page.set_title(&gettext("General"));
    page.set_icon_name(Some("preferences-system-symbolic"));

    // Videos
    let videos = adw::PreferencesGroup::new();
    videos.set_title(&gettext("Videos"));
    let autoplay = adw::SwitchRow::new();
    autoplay.set_title(&gettext("Play Next Video Automatically"));
    autoplay.set_subtitle(&gettext("When a video ends, the next video in the folder starts"));
    LpApplication::default()
        .settings()
        .bind("autoplay-next", &autoplay, "active")
        .build();
    videos.add(&autoplay);
    page.add(&videos);

    // OpenSubtitles
    let account = adw::PreferencesGroup::new();
    account.set_title(&gettext("OpenSubtitles"));
    account.set_description(Some(&gettext(
        "Logging in is optional and raises the daily number of subtitle downloads. The password is kept in your keyring.",
    )));

    let username = adw::EntryRow::new();
    username.set_title(&gettext("Username"));
    let password = adw::PasswordEntryRow::new();
    password.set_title(&gettext("Password"));
    let log_in = gtk::Button::with_label(&gettext("Log In"));
    log_in.add_css_class("suggested-action");
    log_in.add_css_class("pill");
    log_in.set_halign(gtk::Align::Center);
    log_in.set_margin_top(12);

    let logged_in = adw::ActionRow::new();
    let log_out = gtk::Button::with_label(&gettext("Log Out"));
    log_out.set_valign(gtk::Align::Center);
    logged_in.add_suffix(&log_out);

    account.add(&username);
    account.add(&password);
    account.add(&logged_in);
    let log_in_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    log_in_box.append(&log_in);
    account.add(&log_in_box);
    page.add(&account);

    let show_state = glib::clone!(
        #[weak]
        username,
        #[weak]
        password,
        #[weak]
        logged_in,
        #[weak]
        log_in_box,
        move || {
            let user = opensubtitles::logged_in_user();
            if let Some(user) = &user {
                // Translators: {} is the OpenSubtitles username
                logged_in.set_title(&gettext_f("Logged in as {}", [user]));
            }
            logged_in.set_visible(user.is_some());
            username.set_visible(user.is_none());
            password.set_visible(user.is_none());
            log_in_box.set_visible(user.is_none());
        }
    );
    show_state();

    let show_state = std::rc::Rc::new(show_state);
    log_in.connect_clicked(glib::clone!(
        #[weak]
        dialog,
        #[weak]
        username,
        #[weak]
        password,
        #[strong]
        show_state,
        move |button| {
            let (user, pass) = (username.text().to_string(), password.text().to_string());
            if user.is_empty() || pass.is_empty() {
                return;
            }
            button.set_sensitive(false);
            let button = button.clone();
            let show_state = show_state.clone();
            glib::spawn_future_local(async move {
                let result = opensubtitles::store_login(&user, &pass).await;
                button.set_sensitive(true);
                match result {
                    Ok(()) => {
                        password.set_text("");
                        show_state();
                    }
                    Err(err) => dialog.add_toast(adw::Toast::new(&err.to_string())),
                }
            });
        }
    ));
    log_out.connect_clicked(glib::clone!(
        #[strong]
        show_state,
        move |_| {
            let show_state = show_state.clone();
            glib::spawn_future_local(async move {
                opensubtitles::forget_login().await;
                show_state();
            });
        }
    ));

    dialog.add(&page);
    dialog.present(parent);
}
