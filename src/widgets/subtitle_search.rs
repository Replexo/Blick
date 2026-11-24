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

//! Dialog that searches opensubtitles.com for subtitles of a video,
//! saves the chosen one next to it and shows it right away

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::deps::*;
use crate::opensubtitles::{self, Subtitle};
use crate::util::gettext::*;
use crate::widgets::LpVideoPlayer;
use crate::widgets::video_player::user_language;

/// Languages offered for the search, as two letter codes
const LANGUAGES: [&str; 24] = [
    "ar", "cs", "da", "de", "el", "en", "es", "fi", "fr", "he", "hu", "it", "ja", "ko", "nl", "no",
    "pl", "pt", "ro", "ru", "sv", "tr", "uk", "zh",
];

struct Search {
    player: glib::WeakRef<LpVideoPlayer>,
    video: gio::File,
    hash: RefCell<Option<String>>,
    dialog: adw::Dialog,
    language: gtk::DropDown,
    languages: Vec<String>,
    entry: gtk::SearchEntry,
    stack: gtk::Stack,
    list: gtk::ListBox,
    status: adw::StatusPage,
    toasts: adw::ToastOverlay,
}

/// Opens the search for `video`, played by `player`
pub fn present(player: &LpVideoPlayer, video: &gio::File) {
    let mut languages: Vec<String> = LANGUAGES.iter().map(|x| x.to_string()).collect();
    let preferred = {
        let saved = crate::application::LpApplication::default()
            .settings()
            .string("subtitle-language")
            .to_string();
        if saved.is_empty() { user_language() } else { saved }
    };
    if !languages.contains(&preferred) {
        languages.push(preferred.clone());
    }
    let names: Vec<String> = languages
        .iter()
        .map(|code| {
            gst_tag::language_codes::language_name(code)
                .map(|x| x.to_string())
                .unwrap_or_else(|| code.clone())
        })
        .collect();
    let language = gtk::DropDown::from_strings(&names.iter().map(|x| x.as_str()).collect::<Vec<_>>());
    language.set_selected(languages.iter().position(|x| *x == preferred).unwrap_or(0) as u32);
    language.set_tooltip_text(Some(&gettext("Language")));
    language.set_valign(gtk::Align::Center);

    let entry = gtk::SearchEntry::new();
    entry.set_hexpand(true);
    entry.set_text(&search_name(video));
    entry.set_placeholder_text(Some(&gettext("Title")));

    let search_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    search_box.set_margin_start(12);
    search_box.set_margin_end(12);
    search_box.set_margin_top(6);
    search_box.set_margin_bottom(6);
    search_box.append(&entry);
    search_box.append(&language);

    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list.set_valign(gtk::Align::Start);
    list.set_margin_start(12);
    list.set_margin_end(12);
    list.set_margin_top(6);
    list.set_margin_bottom(12);
    let scrolled = gtk::ScrolledWindow::new();
    scrolled.set_child(Some(&list));
    scrolled.set_vexpand(true);
    scrolled.set_hscrollbar_policy(gtk::PolicyType::Never);

    let status = adw::StatusPage::new();
    status.set_icon_name(Some("glance-subtitles-symbolic"));
    status.add_css_class("compact");
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);

    let stack = gtk::Stack::new();
    stack.add_named(&spinner, Some("loading"));
    stack.add_named(&scrolled, Some("results"));
    stack.add_named(&status, Some("status"));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&search_box);
    content.append(&stack);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&content));
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));

    let dialog = adw::Dialog::new();
    dialog.set_title(&gettext("Search Subtitles"));
    dialog.set_content_width(480);
    dialog.set_content_height(560);
    dialog.set_child(Some(&toasts));

    let search = Rc::new(Search {
        player: player.downgrade(),
        video: video.clone(),
        hash: RefCell::new(None),
        dialog: dialog.clone(),
        language,
        languages,
        entry,
        stack,
        list,
        status,
        toasts,
    });

    search.entry.connect_activate(glib::clone!(
        #[strong]
        search,
        move |_| Search::start(&search, false)
    ));
    search.language.connect_selected_notify(glib::clone!(
        #[strong]
        search,
        move |_| {
            let _ = crate::application::LpApplication::default()
                .settings()
                .set_string("subtitle-language", &search.language_code());
            Search::start(&search, true);
        }
    ));

    let window = gio::Application::default()
        .and_downcast::<gtk::Application>()
        .and_then(|app| app.active_window());
    dialog.present(window.as_ref());
    Search::start(&search, true);
}

/// The video's name without extension and dots, as a search
fn search_name(video: &gio::File) -> String {
    video
        .path()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_default()
        .replace(['.', '_'], " ")
}

impl Search {
    fn language_code(&self) -> String {
        self.languages
            .get(self.language.selected() as usize)
            .cloned()
            .unwrap_or_else(|| "en".to_string())
    }

    /// Searches; by the file's hash first when `use_hash`
    fn start(this: &Rc<Self>, use_hash: bool) {
        this.stack.set_visible_child_name("loading");
        let this = this.clone();
        glib::spawn_future_local(async move {
            let path = this.video.path();
            if use_hash
                && this.hash.borrow().is_none()
                && let Some(path) = path
            {
                let hash = gio::spawn_blocking(move || opensubtitles::movie_hash(&path))
                    .await
                    .ok()
                    .and_then(|x| x.ok());
                this.hash.replace(hash);
            }
            let hash = if use_hash { this.hash.borrow().clone() } else { None };
            let query = this.entry.text().to_string();
            let result = opensubtitles::search(hash.as_deref(), &query, &this.language_code()).await;
            match result {
                Ok(results) if results.is_empty() => this.show_status(
                    &gettext("No Subtitles Found"),
                    &gettext("Try another title or language."),
                ),
                Ok(results) => this.show_results(results),
                Err(err) => this.show_status(&gettext("Search Failed"), &err.to_string()),
            }
        });
    }

    fn show_status(&self, title: &str, description: &str) {
        self.status.set_title(title);
        self.status.set_description(Some(description));
        self.stack.set_visible_child_name("status");
    }

    fn show_results(self: &Rc<Self>, results: Vec<Subtitle>) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        for subtitle in results {
            let row = adw::ActionRow::new();
            row.set_title(&glib::markup_escape_text(&subtitle.name));
            row.set_title_lines(2);
            let language = gst_tag::language_codes::language_name(&subtitle.language)
                .map(|x| x.to_string())
                .unwrap_or_else(|| subtitle.language.clone());
            // Translators: Below a subtitle, {} are its language and how often it was downloaded
            row.set_subtitle(&gettext_f(
                "{} · {} downloads",
                [language, subtitle.downloads.to_string()],
            ));
            if subtitle.hash_match {
                let badge = gtk::Label::new(Some(&gettext("Exact Match")));
                badge.set_tooltip_text(Some(&gettext("Timed for this video file")));
                badge.add_css_class("caption");
                badge.add_css_class("accent");
                badge.set_valign(gtk::Align::Center);
                row.add_suffix(&badge);
            }
            let button = gtk::Button::from_icon_name("glance-download-symbolic");
            button.set_tooltip_text(Some(&gettext("Download")));
            button.add_css_class("flat");
            button.set_valign(gtk::Align::Center);
            button.connect_clicked(glib::clone!(
                #[strong(rename_to = this)]
                self,
                move |button| this.download(button, subtitle.clone())
            ));
            row.add_suffix(&button);
            row.set_activatable_widget(Some(&button));
            self.list.append(&row);
        }
        self.stack.set_visible_child_name("results");
    }

    /// Saves the subtitle as `<video name>.<lang>.srt` and loads it
    fn download(self: &Rc<Self>, button: &gtk::Button, subtitle: Subtitle) {
        button.set_sensitive(false);
        let this = self.clone();
        let button = button.clone();
        glib::spawn_future_local(async move {
            let result = this.save(&subtitle).await;
            button.set_sensitive(true);
            match result {
                Ok(file) => {
                    if let Some(player) = this.player.upgrade() {
                        player.load_subtitles(&file);
                    }
                    this.dialog.close();
                }
                Err(message) => this.toasts.add_toast(adw::Toast::new(&message)),
            }
        });
    }

    async fn save(&self, subtitle: &Subtitle) -> Result<gio::File, String> {
        let bytes = opensubtitles::download(subtitle)
            .await
            .map_err(|err| err.to_string())?;
        let path = self
            .video
            .path()
            .ok_or_else(|| gettext("The video is not in a local folder."))?;
        let target = path.with_extension(format!("{}.srt", subtitle.language));
        let file = gio::File::for_path(&target);
        file.replace_contents_future(bytes, None, false, gio::FileCreateFlags::NONE)
            .await
            .map_err(|(_, err)| gettext_f("Could not save the subtitles: {}", [err.to_string()]))?;
        Ok(file)
    }
}
