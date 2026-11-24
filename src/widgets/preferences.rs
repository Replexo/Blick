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

//! Preferences: video playback

use adw::prelude::*;

use crate::application::LpApplication;
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

    dialog.add(&page);
    dialog.present(parent);
}
