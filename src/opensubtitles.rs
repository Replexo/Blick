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

//! Searches and downloads subtitles from opensubtitles.com
//!
//! Uses the REST API at `api.opensubtitles.com/api/v1`. A search first
//! tries the file's hash, which finds subtitles timed for this exact file,
//! and then its name. An optional login, kept in the keyring, raises the
//! daily download limit.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use gtk::prelude::*;
use soup::prelude::*;
use glib::translate::IntoGlib;

use crate::config;
use crate::deps::*;
use crate::util::gettext::*;

const API: &str = "https://api.opensubtitles.com/api/v1";
/// Keyring attributes of the stored login
const SECRET_SERVICE: &str = "opensubtitles.com";

/// What went wrong, in words for the user
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", gettext("Online subtitle search is not set up in this build of Blink."))]
    NoApiKey,
    #[error("{}", gettext("Could not reach opensubtitles.com. Check your internet connection."))]
    Network(#[source] glib::Error),
    #[error("{}", gettext("The daily download limit is reached. Log in to OpenSubtitles in Preferences for more downloads, or try again tomorrow."))]
    Limit,
    #[error("{}", gettext("The OpenSubtitles username or password is wrong."))]
    Login,
    #[error("{}", gettext_f("opensubtitles.com answered with an error ({}).", [.0.to_string()]))]
    Server(u32),
    #[error("{}", gettext("opensubtitles.com sent an answer Blink does not understand."))]
    Answer,
}

/// One subtitle found online
#[derive(Debug, Clone)]
pub struct Subtitle {
    pub file_id: i64,
    pub name: String,
    pub language: String,
    pub downloads: i64,
    /// Timed for this exact file
    pub hash_match: bool,
}

/// OpenSubtitles' hash: the file size plus the 64-bit little-endian words of
/// the first and last 64 KiB
pub fn movie_hash(path: &Path) -> std::io::Result<String> {
    const CHUNK: u64 = 65536;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size < CHUNK {
        return Err(std::io::Error::other("File too small"));
    }

    let mut hash = size;
    let mut buffer = vec![0u8; CHUNK as usize];
    for offset in [0, size - CHUNK] {
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut buffer)?;
        for word in buffer.as_chunks::<8>().0 {
            hash = hash.wrapping_add(u64::from_le_bytes(*word));
        }
    }
    Ok(format!("{hash:016x}"))
}

fn session() -> soup::Session {
    let session = soup::Session::new();
    session.set_user_agent(&format!("Blink v{}", config::VERSION));
    session
}

fn request(method: &str, url: &str, token: Option<&str>) -> Result<soup::Message, Error> {
    if config::OPENSUBTITLES_API_KEY.is_empty() {
        return Err(Error::NoApiKey);
    }
    let message = soup::Message::new(method, url).map_err(|_| Error::Answer)?;
    if let Some(headers) = message.request_headers() {
        headers.append("Api-Key", config::OPENSUBTITLES_API_KEY);
        headers.append("Accept", "application/json");
        if let Some(token) = token {
            headers.append("Authorization", &format!("Bearer {token}"));
        }
    }
    Ok(message)
}

async fn send(message: &soup::Message, body: Option<serde_json::Value>) -> Result<serde_json::Value, Error> {
    if let Some(body) = body {
        let bytes = glib::Bytes::from_owned(body.to_string().into_bytes());
        message.set_request_body_from_bytes(Some("application/json"), Some(&bytes));
    }
    let bytes = session()
        .send_and_read_future(message, glib::Priority::DEFAULT)
        .await
        .map_err(Error::Network)?;
    match message.status() {
        soup::Status::Ok => {}
        soup::Status::Unauthorized => return Err(Error::Login),
        // 406 and 429 mean the download limit or the request rate is reached
        soup::Status::NotAcceptable => return Err(Error::Limit),
        status if status.into_glib() == 429 => return Err(Error::Limit),
        status => return Err(Error::Server(status.into_glib() as u32)),
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Answer)
}

/// Subtitles in `language` (like `en`) for the file with `hash`, or else
/// for `query`
pub async fn search(
    hash: Option<&str>,
    query: &str,
    language: &str,
) -> Result<Vec<Subtitle>, Error> {
    if let Some(hash) = hash {
        let results = search_with(&[("moviehash", hash), ("languages", language)]).await?;
        if !results.is_empty() {
            return Ok(results);
        }
    }
    search_with(&[("query", query), ("languages", language)]).await
}

async fn search_with(parameters: &[(&str, &str)]) -> Result<Vec<Subtitle>, Error> {
    let query = parameters
        .iter()
        .map(|(key, value)| format!("{key}={}", glib::Uri::escape_string(value, None, false)))
        .collect::<Vec<_>>()
        .join("&");
    let message = request("GET", &format!("{API}/subtitles?{query}"), None)?;
    let json = send(&message, None).await?;

    let results = json["data"]
        .as_array()
        .map(|data| {
            data.iter()
                .filter_map(|item| {
                    let attributes = &item["attributes"];
                    let file = attributes["files"].as_array()?.first()?;
                    Some(Subtitle {
                        file_id: file["file_id"].as_i64()?,
                        name: attributes["release"]
                            .as_str()
                            .filter(|x| !x.is_empty())
                            .or_else(|| file["file_name"].as_str())
                            .unwrap_or_default()
                            .to_string(),
                        language: attributes["language"].as_str().unwrap_or_default().to_string(),
                        downloads: attributes["download_count"].as_i64().unwrap_or_default(),
                        hash_match: attributes["moviehash_match"].as_bool().unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut results = results;
    results.sort_by(|a, b| {
        b.hash_match
            .cmp(&a.hash_match)
            .then(b.downloads.cmp(&a.downloads))
    });
    Ok(results)
}

/// The subtitle file's text
pub async fn download(subtitle: &Subtitle) -> Result<glib::Bytes, Error> {
    let token = match stored_login().await {
        Some((username, password)) => login(&username, &password).await.ok(),
        None => None,
    };
    let message = request("POST", &format!("{API}/download"), token.as_deref())?;
    let json = send(&message, Some(serde_json::json!({ "file_id": subtitle.file_id }))).await?;
    let link = json["link"].as_str().ok_or(Error::Answer)?;

    let message = soup::Message::new("GET", link).map_err(|_| Error::Answer)?;
    let bytes = session()
        .send_and_read_future(&message, glib::Priority::DEFAULT)
        .await
        .map_err(Error::Network)?;
    if message.status() != soup::Status::Ok {
        return Err(Error::Server(message.status().into_glib() as u32));
    }
    Ok(bytes)
}

/// Token for a logged in user
pub async fn login(username: &str, password: &str) -> Result<String, Error> {
    let message = request("POST", &format!("{API}/login"), None)?;
    let json = send(
        &message,
        Some(serde_json::json!({ "username": username, "password": password })),
    )
    .await?;
    json["token"].as_str().map(|x| x.to_string()).ok_or(Error::Answer)
}

fn schema() -> libsecret::Schema {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert("service", libsecret::SchemaAttributeType::String);
    attributes.insert("username", libsecret::SchemaAttributeType::String);
    libsecret::Schema::new(
        config::APP_ID,
        libsecret::SchemaFlags::NONE,
        attributes,
    )
}

/// The stored username; the password is in the keyring
fn stored_username() -> Option<String> {
    let username = crate::application::LpApplication::default()
        .settings()
        .string("opensubtitles-username")
        .to_string();
    (!username.is_empty()).then_some(username)
}

/// Username and password kept in the keyring
pub async fn stored_login() -> Option<(String, String)> {
    let username = stored_username()?;
    let mut attributes = std::collections::HashMap::new();
    attributes.insert("service", SECRET_SERVICE);
    attributes.insert("username", username.as_str());
    let password = libsecret::password_lookup_future(Some(&schema()), attributes)
        .await
        .ok()??;
    Some((username, password.to_string()))
}

/// Logged in username, if any
pub fn logged_in_user() -> Option<String> {
    stored_username()
}

/// Checks the login with OpenSubtitles and keeps it in the keyring
pub async fn store_login(username: &str, password: &str) -> Result<(), Error> {
    login(username, password).await?;
    forget_login().await;
    let mut attributes = std::collections::HashMap::new();
    attributes.insert("service", SECRET_SERVICE);
    attributes.insert("username", username);
    libsecret::password_store_future(
        Some(&schema()),
        attributes,
        Some(libsecret::COLLECTION_DEFAULT.as_str()),
        &gettext("OpenSubtitles login for Blink"),
        password,
    )
    .await
    .map_err(Error::Network)?;
    let _ = crate::application::LpApplication::default()
        .settings()
        .set_string("opensubtitles-username", username);
    Ok(())
}

pub async fn forget_login() {
    let mut attributes = std::collections::HashMap::new();
    attributes.insert("service", SECRET_SERVICE);
    let _ = libsecret::password_clear_future(Some(&schema()), attributes).await;
    let _ = crate::application::LpApplication::default()
        .settings()
        .set_string("opensubtitles-username", "");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_adds_size_and_words() {
        let dir = std::env::temp_dir().join(format!("blink-hash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("video");
        // 128 KiB of zeros, so only the size counts
        std::fs::write(&path, vec![0u8; 131072]).unwrap();
        assert_eq!(movie_hash(&path).unwrap(), "0000000000020000");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
