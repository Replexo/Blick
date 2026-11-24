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

//! Finds the video part of motion photos
//!
//! Google and Samsung cameras append an MP4 to the JPEG or HEIC file and
//! mark it in the XMP data or with a `MotionPhoto_Data` marker. iPhone Live
//! Photos keep the video next to the photo, as a `.MOV` with the same name.

use std::path::{Path, PathBuf};

use gtk::prelude::*;

use crate::deps::*;

/// Markers that only motion photos carry
const MARKERS: [&[u8]; 4] = [
    b"MotionPhoto_Data",
    b"GCamera:MotionPhoto",
    b"GCamera:MicroVideo",
    b"Camera:MotionPhoto",
];

/// Paired video of a Live Photo, or the embedded video written to the cache
pub async fn find(file: &gio::File) -> Option<gio::File> {
    let path = file.path()?;
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(extension.as_str(), "jpg" | "jpeg" | "heic" | "heif") {
        return None;
    }

    if let Some(paired) = paired_video(&path) {
        return Some(gio::File::for_path(paired));
    }

    gio::spawn_blocking(move || extract_embedded(&path))
        .await
        .ok()
        .flatten()
        .map(gio::File::for_path)
}

/// `IMG_0001.MOV` next to `IMG_0001.HEIC`
fn paired_video(path: &Path) -> Option<PathBuf> {
    ["MOV", "mov"]
        .into_iter()
        .map(|extension| path.with_extension(extension))
        .find(|candidate| candidate.is_file())
}

fn extract_embedded(path: &Path) -> Option<PathBuf> {
    let data = std::fs::read(path).ok()?;
    let offset = embedded_offset(&data)?;

    let dir = glib::user_cache_dir().join("blick").join("motion");
    std::fs::create_dir_all(&dir).ok()?;
    let key = path.to_string_lossy().to_string();
    let hash = glib::compute_checksum_for_string(glib::ChecksumType::Sha1, key.as_str())?;
    let target = dir.join(format!("{hash}.mp4"));
    if !target.is_file() {
        std::fs::write(&target, &data[offset..]).ok()?;
    }
    Some(target)
}

/// Start of the MP4 inside a motion photo
fn embedded_offset(data: &[u8]) -> Option<usize> {
    let marked = MARKERS
        .iter()
        .any(|marker| data.windows(marker.len()).any(|w| w == *marker));
    if !marked {
        return None;
    }

    // The first `ftyp` box after the start of the photo; its size comes in
    // the four bytes before it
    let mut from = 16;
    while let Some(found) = data[from..].windows(4).position(|w| w == b"ftyp") {
        let index = from + found;
        let start = index.checked_sub(4)?;
        let size = u32::from_be_bytes(data[start..index].try_into().ok()?) as usize;
        let brand = data.get(index + 4..index + 8)?;
        if (8..=64).contains(&size)
            && brand.iter().all(|b| b.is_ascii_alphanumeric() || *b == b' ')
        {
            return Some(start);
        }
        from = index + 4;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_appended_mp4() {
        let mut data = vec![0xFF, 0xD8];
        data.extend_from_slice(b"....GCamera:MotionPhoto=\"1\"....");
        data.extend_from_slice(&[0; 20]);
        data.extend_from_slice(&[0xFF, 0xD9]);
        let start = data.len();
        data.extend_from_slice(&[0, 0, 0, 24]);
        data.extend_from_slice(b"ftypmp42");
        data.extend_from_slice(&[0; 12]);
        assert_eq!(embedded_offset(&data), Some(start));
    }

    #[test]
    fn ignores_plain_photos() {
        let mut data = vec![0xFF, 0xD8];
        data.extend_from_slice(&[0; 40]);
        data.extend_from_slice(&[0, 0, 0, 24]);
        data.extend_from_slice(b"ftypmp42");
        assert_eq!(embedded_offset(&data), None);
    }
}
