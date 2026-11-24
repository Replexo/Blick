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

//! Saves edited videos with the `ffmpeg` program
//!
//! Cuts are frame accurate with a smart cut: only the pieces between each
//! cut and the next keyframe are encoded again, the rest is copied. When
//! that is not possible, the whole range is encoded again in the original
//! codec at a quality close to the source. Rotation and flips are stored
//! as display metadata, so they need no encoding.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gio::prelude::*;

use crate::deps::*;
use crate::util::gettext::*;

/// What to change in a video
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoEdit {
    /// Kept range, in seconds
    pub start: f64,
    pub end: f64,
    pub mute: bool,
    /// Counter-clockwise, in steps of 90°
    pub rotation: i32,
    pub flip: bool,
}

impl VideoEdit {
    pub fn new(duration: f64) -> Self {
        Self {
            start: 0.,
            end: duration,
            mute: false,
            rotation: 0,
            flip: false,
        }
    }

    pub fn is_trimmed(&self, duration: f64) -> bool {
        self.start > 0.001 || self.end < duration - 0.001
    }

    pub fn is_changed(&self, duration: f64) -> bool {
        self.is_trimmed(duration) || self.mute || self.rotation % 360 != 0 || self.flip
    }
}

/// Quality of a smaller copy
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    Small,
    Medium,
    Original,
}

impl Quality {
    /// Longest side, if the video gets scaled down
    fn max_size(self) -> Option<u32> {
        match self {
            Self::Small => Some(1280),
            Self::Medium => Some(1920),
            Self::Original => None,
        }
    }

    fn h264_crf(self) -> u32 {
        match self {
            Self::Small => 28,
            Self::Medium => 23,
            Self::Original => 18,
        }
    }

    fn vp9_crf(self) -> u32 {
        match self {
            Self::Small => 38,
            Self::Medium => 32,
            Self::Original => 24,
        }
    }

    /// Rough video bitrate, in bits per second, for the size estimate
    fn bitrate(self, source_bitrate: Option<u64>) -> u64 {
        let estimate = match self {
            Self::Small => 1_500_000,
            Self::Medium => 4_000_000,
            Self::Original => 10_000_000,
        };
        source_bitrate.map_or(estimate, |source| source.min(estimate))
    }
}

/// What the user reads when saving fails
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}", gettext("The “ffmpeg” program, which is needed to save videos, is not installed."))]
    NoFfmpeg,
    #[error("{}", gettext("Saving was canceled."))]
    Cancelled,
    #[error("{}", gettext_f("ffmpeg could not save the video: {}", [.0.as_str()]))]
    Failed(String),
}

/// Facts about the source the export needs
#[derive(Debug, Clone, Default)]
pub struct SourceInfo {
    pub duration: f64,
    pub codec: String,
    pub pixel_format: String,
    pub profile: String,
    pub bitrate: Option<u64>,
    pub keyframes: Vec<f64>,
    /// Counter-clockwise degrees stored in the file
    pub rotation: i32,
    pub has_audio: bool,
    /// Frames per second of the video
    pub frame_rate: f64,
}

/// Runs `program` and returns its output, with the whole run cancellable
async fn output(program: &str, args: &[&str], cancellable: &gio::Cancellable) -> Result<String, Error> {
    if glib::find_program_in_path(program).is_none() {
        return Err(Error::NoFfmpeg);
    }
    let mut argv: Vec<&std::ffi::OsStr> = vec![program.as_ref()];
    argv.extend(args.iter().map(std::ffi::OsStr::new));
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|err| Error::Failed(err.to_string()))?;
    let handle = cancellable.connect_cancelled_local(glib::clone!(
        #[weak]
        process,
        move |_| process.force_exit()
    ));
    let result = process.communicate_utf8_future(None).await;
    if let Some(handle) = handle {
        cancellable.disconnect_cancelled(handle);
    }
    if cancellable.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let (stdout, stderr) = result.map_err(|err| Error::Failed(err.to_string()))?;
    if !process.is_successful() {
        let message = stderr
            .map(|x| x.to_string())
            .unwrap_or_default()
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .to_string();
        return Err(Error::Failed(message));
    }
    Ok(stdout.map(|x| x.to_string()).unwrap_or_default())
}

/// Runs ffmpeg with `args`, reporting progress through the `seconds` it
/// has written of the `total`
async fn ffmpeg(
    args: &[String],
    total: f64,
    done_before: f64,
    progress: &Rc<dyn Fn(f64)>,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    if glib::find_program_in_path("ffmpeg").is_none() {
        return Err(Error::NoFfmpeg);
    }
    let mut argv: Vec<String> = vec![
        "ffmpeg".into(),
        "-hide_banner".into(),
        "-nostdin".into(),
        "-y".into(),
        "-loglevel".into(),
        "error".into(),
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
    ];
    argv.extend(args.iter().cloned());
    tracing::debug!("Running {}", argv.join(" "));
    let argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let process = gio::Subprocess::newv(
        &argv,
        gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_PIPE,
    )
    .map_err(|err| Error::Failed(err.to_string()))?;
    let handle = cancellable.connect_cancelled_local(glib::clone!(
        #[weak]
        process,
        move |_| process.force_exit()
    ));

    // Progress lines like `out_time_us=1234567`
    if let Some(stdout) = process.stdout_pipe() {
        let lines = gio::DataInputStream::new(&stdout);
        while let Ok(Some(line)) = lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|x| x.parse::<f64>().ok())
                && total > 0.
            {
                progress(((done_before + us / 1e6) / total).clamp(0., 1.));
            }
        }
    }
    let stderr = match process.stderr_pipe() {
        Some(stderr) => {
            let lines = gio::DataInputStream::new(&stderr);
            let mut last = String::new();
            while let Ok(Some(line)) = lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
                if !line.trim().is_empty() {
                    last = line.to_string();
                }
            }
            last
        }
        None => String::new(),
    };
    let _ = process.wait_future().await;
    if let Some(handle) = handle {
        cancellable.disconnect_cancelled(handle);
    }
    if cancellable.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if !process.is_successful() {
        return Err(Error::Failed(stderr));
    }
    Ok(())
}

/// Reads codec, keyframes and rotation with `ffprobe`
pub async fn probe(input: &Path, cancellable: &gio::Cancellable) -> Result<SourceInfo, Error> {
    let input = input.to_string_lossy().to_string();
    let json = output(
        "ffprobe",
        &[
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            &input,
        ],
        cancellable,
    )
    .await?;
    let json: serde_json::Value =
        serde_json::from_str(&json).map_err(|err| Error::Failed(err.to_string()))?;

    let streams = json["streams"].as_array().cloned().unwrap_or_default();
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .cloned()
        .unwrap_or_default();
    let text = |value: &serde_json::Value| value.as_str().unwrap_or_default().to_string();
    let rotation = video["side_data_list"]
        .as_array()
        .and_then(|list| list.iter().find_map(|data| data["rotation"].as_f64()))
        .unwrap_or_default() as i32;

    let keyframes = output(
        "ffprobe",
        &[
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-skip_frame",
            "nokey",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
            &input,
        ],
        cancellable,
    )
    .await?
    .lines()
    .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
    .collect();

    Ok(SourceInfo {
        duration: text(&json["format"]["duration"]).parse().unwrap_or_default(),
        codec: text(&video["codec_name"]),
        pixel_format: text(&video["pix_fmt"]),
        profile: text(&video["profile"]),
        bitrate: text(&json["format"]["bit_rate"]).parse().ok(),
        keyframes,
        rotation,
        has_audio: streams.iter().any(|s| s["codec_type"] == "audio"),
        frame_rate: parse_rate(&text(&video["r_frame_rate"])),
    })
}

/// `30000/1001` as a number
fn parse_rate(rate: &str) -> f64 {
    match rate.split_once('/') {
        Some((n, d)) => {
            let (n, d) = (n.parse::<f64>().unwrap_or(0.), d.parse::<f64>().unwrap_or(1.));
            if d > 0. { n / d } else { 0. }
        }
        None => rate.parse().unwrap_or(0.),
    }
}

/// Encoder and options that match `codec` at close to source quality
fn encoder_for(codec: &str) -> Option<Vec<&'static str>> {
    Some(match codec {
        "h264" => vec!["-c:v", "libx264", "-crf", "17", "-preset", "medium"],
        "hevc" => vec!["-c:v", "libx265", "-crf", "19", "-preset", "medium"],
        "vp9" => vec!["-c:v", "libvpx-vp9", "-crf", "24", "-b:v", "0"],
        "vp8" => vec!["-c:v", "libvpx", "-crf", "8", "-b:v", "10M"],
        "av1" => vec!["-c:v", "libsvtav1", "-crf", "28"],
        "mpeg4" => vec!["-c:v", "mpeg4", "-q:v", "2"],
        _ => return None,
    })
}

/// Rotation and flip as display metadata, for the input that follows
fn display_options(edit: &VideoEdit, source: &SourceInfo) -> Vec<String> {
    let rotation = (source.rotation + edit.rotation * 90).rem_euclid(360);
    let mut options = Vec::new();
    if rotation != source.rotation.rem_euclid(360) || edit.flip {
        options.extend(["-display_rotation:v:0".into(), rotation.to_string()]);
    }
    if edit.flip {
        options.push("-display_hflip:v:0".into());
    }
    options
}

fn seconds(value: f64) -> String {
    format!("{value:.6}")
}

/// Saves `edit` of `input` to `output`, with `progress` from 0 to 1
pub async fn save(
    input: &Path,
    output_path: &Path,
    edit: &VideoEdit,
    progress: impl Fn(f64) + 'static,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    let progress: Rc<dyn Fn(f64)> = Rc::new(progress);
    let source = probe(input, cancellable).await?;
    let duration = if source.duration > 0. { source.duration } else { edit.end };

    if !edit.is_trimmed(duration) {
        // Nothing to cut: copy with the new metadata
        return copy_range(input, output_path, edit, &source, 0., duration, &progress, cancellable)
            .await;
    }

    match smart_cut(input, output_path, edit, &source, &progress, cancellable).await {
        Ok(()) => Ok(()),
        Err(Error::Cancelled) => Err(Error::Cancelled),
        Err(err) => {
            tracing::info!("Smart cut not possible, encoding everything: {err}");
            reencode(input, output_path, edit, &source, &progress, cancellable).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn copy_range(
    input: &Path,
    output_path: &Path,
    edit: &VideoEdit,
    source: &SourceInfo,
    start: f64,
    end: f64,
    progress: &Rc<dyn Fn(f64)>,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    let mut args = display_options(edit, source);
    args.extend([
        "-ss".into(),
        seconds(start),
        "-to".into(),
        seconds(end),
        "-i".into(),
        input.to_string_lossy().to_string(),
        "-map".into(),
        "0".into(),
        "-c".into(),
        "copy".into(),
    ]);
    if edit.mute {
        args.push("-an".into());
    }
    args.push(output_path.to_string_lossy().to_string());
    ffmpeg(&args, end - start, 0., progress, cancellable).await
}

/// Encodes the pieces around the cuts and copies the middle
async fn smart_cut(
    input: &Path,
    output_path: &Path,
    edit: &VideoEdit,
    source: &SourceInfo,
    progress: &Rc<dyn Fn(f64)>,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    const EPSILON: f64 = 0.001;
    let encoder = encoder_for(&source.codec)
        .ok_or_else(|| Error::Failed(format!("No encoder for {}", source.codec)))?;
    let first_key = source
        .keyframes
        .iter()
        .copied()
        .find(|k| *k >= edit.start - EPSILON)
        .ok_or_else(|| Error::Failed("No keyframe after the start".into()))?;
    let last_key = source
        .keyframes
        .iter()
        .copied()
        .rfind(|k| *k <= edit.end + EPSILON)
        .ok_or_else(|| Error::Failed("No keyframe before the end".into()))?;
    if last_key <= first_key + EPSILON {
        return Err(Error::Failed("The range is shorter than a group of pictures".into()));
    }

    let dir = temp_dir()?;
    let input_s = input.to_string_lossy().to_string();
    let total = edit.end - edit.start;
    let mut done = 0.;
    let mut pieces: Vec<PathBuf> = Vec::new();

    let encode_piece = |from: f64, to: f64, name: &str| {
        let mut args = vec![
            "-ss".to_string(),
            seconds(from),
            "-to".into(),
            seconds(to),
            "-i".into(),
            input_s.clone(),
            "-map".into(),
            "0:v:0".into(),
            "-an".into(),
        ];
        args.extend(encoder.iter().map(|x| x.to_string()));
        if !source.pixel_format.is_empty() {
            args.extend(["-pix_fmt".into(), source.pixel_format.clone()]);
        }
        if source.codec == "h264" && !source.profile.is_empty() {
            args.extend(["-profile:v".into(), source.profile.to_lowercase().replace(' ', "")]);
        }
        let path = dir.join(name);
        args.extend(["-f".into(), "mpegts".into(), path.to_string_lossy().to_string()]);
        (args, path)
    };

    // Start, from the cut to the first keyframe
    if first_key > edit.start + EPSILON {
        let (args, path) = encode_piece(edit.start, first_key, "start.ts");
        ffmpeg(&args, total, done, progress, cancellable).await?;
        done += first_key - edit.start;
        pieces.push(path);
    }

    // Middle, copied; counted in frames, as a copy cut by time can keep
    // frames past the keyframe
    let middle = dir.join("middle.ts");
    let mut args = vec![
        "-ss".to_string(),
        seconds(first_key),
        "-i".into(),
        input_s.clone(),
        "-map".into(),
        "0:v:0".into(),
        "-an".into(),
        "-c".into(),
        "copy".into(),
    ];
    if source.frame_rate > 0. {
        let frames = ((last_key - first_key) * source.frame_rate).round() as u64;
        args.extend(["-frames:v".into(), frames.to_string()]);
    } else {
        args.extend(["-t".into(), seconds(last_key - first_key)]);
    }
    if matches!(source.codec.as_str(), "h264" | "hevc") {
        let filter = if source.codec == "h264" {
            "h264_mp4toannexb"
        } else {
            "hevc_mp4toannexb"
        };
        args.extend(["-bsf:v".into(), filter.into()]);
    }
    args.extend(["-f".into(), "mpegts".into(), middle.to_string_lossy().to_string()]);
    ffmpeg(&args, total, done, progress, cancellable).await?;
    done += last_key - first_key;
    pieces.push(middle);

    // End, from the last keyframe to the cut
    if edit.end > last_key + EPSILON {
        let (args, path) = encode_piece(last_key, edit.end, "end.ts");
        ffmpeg(&args, total, done, progress, cancellable).await?;
        pieces.push(path);
    }

    // Join the pieces, then add the sound of the kept range and metadata
    let list = dir.join("pieces.txt");
    let list_text: String = pieces
        .iter()
        .map(|p| format!("file '{}'\n", p.to_string_lossy().replace('\'', "'\\''")))
        .collect();
    std::fs::write(&list, list_text).map_err(|err| Error::Failed(err.to_string()))?;

    let mut args = display_options(edit, source);
    args.extend([
        "-f".into(),
        "concat".into(),
        "-safe".into(),
        "0".into(),
        "-i".into(),
        list.to_string_lossy().to_string(),
    ]);
    if !edit.mute && source.has_audio {
        args.extend([
            "-ss".into(),
            seconds(edit.start),
            "-to".into(),
            seconds(edit.end),
            "-i".into(),
            input_s.clone(),
            "-map".into(),
            "0:v".into(),
            "-map".into(),
            "1:a?".into(),
        ]);
    } else {
        args.extend(["-map".into(), "0:v".into()]);
    }
    args.extend(["-c".into(), "copy".into()]);
    args.push(output_path.to_string_lossy().to_string());
    let result = ffmpeg(&args, total, total, progress, cancellable).await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Encodes the whole kept range again, in the source codec
async fn reencode(
    input: &Path,
    output_path: &Path,
    edit: &VideoEdit,
    source: &SourceInfo,
    progress: &Rc<dyn Fn(f64)>,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    let encoder = encoder_for(&source.codec)
        .unwrap_or_else(|| vec!["-c:v", "libx264", "-crf", "17", "-preset", "medium"]);
    let mut args = display_options(edit, source);
    args.extend([
        "-ss".into(),
        seconds(edit.start),
        "-to".into(),
        seconds(edit.end),
        "-i".into(),
        input.to_string_lossy().to_string(),
        "-map".into(),
        "0:v:0".into(),
    ]);
    args.extend(encoder.iter().map(|x| x.to_string()));
    if edit.mute {
        args.push("-an".into());
    } else {
        args.extend(["-map".into(), "0:a?".into(), "-c:a".into(), "copy".into()]);
    }
    args.push(output_path.to_string_lossy().to_string());
    ffmpeg(&args, edit.end - edit.start, 0., progress, cancellable).await
}

/// Whether ffmpeg can encode H.264; otherwise smaller copies use VP9
async fn has_h264(cancellable: &gio::Cancellable) -> bool {
    output("ffmpeg", &["-hide_banner", "-encoders"], cancellable)
        .await
        .is_ok_and(|encoders| encoders.contains(" libx264 "))
}

/// File extension of smaller copies made on this system
pub async fn smaller_extension() -> &'static str {
    if has_h264(&gio::Cancellable::new()).await {
        "mp4"
    } else {
        "webm"
    }
}

/// Roughly how many bytes a smaller copy of `seconds` will take
pub fn estimate_size(quality: Quality, seconds: f64, source_bitrate: Option<u64>, mute: bool) -> u64 {
    let audio = if mute { 0 } else { 128_000 };
    ((quality.bitrate(source_bitrate) + audio) as f64 / 8. * seconds) as u64
}

/// Encodes `edit` of `input` to H.264 and AAC in MP4, or VP9 and Opus in
/// WebM without H.264
pub async fn save_smaller(
    input: &Path,
    output_path: &Path,
    edit: &VideoEdit,
    quality: Quality,
    progress: impl Fn(f64) + 'static,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    let progress: Rc<dyn Fn(f64)> = Rc::new(progress);
    let source = probe(input, cancellable).await?;
    let h264 = has_h264(cancellable).await;

    let mut args = display_options(edit, &source);
    args.extend([
        "-ss".into(),
        seconds(edit.start),
        "-to".into(),
        seconds(edit.end),
        "-i".into(),
        input.to_string_lossy().to_string(),
        "-map".into(),
        "0:v:0".into(),
    ]);
    if let Some(max) = quality.max_size() {
        // Only ever smaller, keeping the aspect ratio and even sizes
        args.extend([
            "-vf".into(),
            format!(
                "scale='if(gt(iw,ih),min({max},iw),-2)':'if(gt(iw,ih),-2,min({max},ih))'"
            ),
        ]);
    }
    if h264 {
        args.extend([
            "-c:v".into(),
            "libx264".into(),
            "-crf".into(),
            quality.h264_crf().to_string(),
            "-preset".into(),
            "medium".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
        ]);
    } else {
        args.extend([
            "-c:v".into(),
            "libvpx-vp9".into(),
            "-crf".into(),
            quality.vp9_crf().to_string(),
            "-b:v".into(),
            "0".into(),
        ]);
    }
    if edit.mute || !source.has_audio {
        args.push("-an".into());
    } else {
        let (codec, bitrate) = if h264 { ("aac", "128k") } else { ("libopus", "96k") };
        args.extend([
            "-map".into(),
            "0:a:0?".into(),
            "-c:a".into(),
            codec.into(),
            "-b:a".into(),
            bitrate.into(),
        ]);
    }
    if h264 {
        args.extend(["-movflags".into(), "+faststart".into()]);
    }
    args.push(output_path.to_string_lossy().to_string());
    ffmpeg(&args, edit.end - edit.start, 0., &progress, cancellable).await
}

fn temp_dir() -> Result<PathBuf, Error> {
    thread_local! {
        static COUNTER: Cell<u32> = const { Cell::new(0) };
    }
    let n = COUNTER.get();
    COUNTER.set(n + 1);
    let dir = glib::user_cache_dir()
        .join("blink")
        .join(format!("export-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|err| Error::Failed(err.to_string()))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_metadata_adds_to_source_rotation() {
        let source = SourceInfo {
            rotation: 90,
            ..Default::default()
        };
        let mut edit = VideoEdit::new(10.);
        assert!(display_options(&edit, &source).is_empty());
        edit.rotation = 1;
        assert_eq!(display_options(&edit, &source), ["-display_rotation:v:0", "180"]);
        edit.flip = true;
        assert_eq!(
            display_options(&edit, &source),
            ["-display_rotation:v:0", "180", "-display_hflip:v:0"]
        );
    }

    #[test]
    fn trimmed_and_changed() {
        let mut edit = VideoEdit::new(10.);
        assert!(!edit.is_changed(10.));
        edit.end = 8.;
        assert!(edit.is_trimmed(10.));
        edit.end = 10.;
        edit.mute = true;
        assert!(edit.is_changed(10.) && !edit.is_trimmed(10.));
    }

    /// Trims a generated video off its keyframes and checks the result;
    /// skipped without ffmpeg
    #[test]
    fn smart_cut_is_frame_accurate() {
        if glib::find_program_in_path("ffmpeg").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("blink-cut-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440", "-t", "12"])
            .args(["-c:v", "libx264", "-g", "50", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success());

        let output = dir.join("out.mp4");
        let edit = VideoEdit {
            start: 1.24,
            end: 9.6,
            ..VideoEdit::new(12.)
        };
        let context = glib::MainContext::default();
        let result = context.block_on(save(&input, &output, &edit, |_| {}, &gio::Cancellable::new()));
        assert!(result.is_ok(), "{result:?}");

        let info = context.block_on(probe(&output, &gio::Cancellable::new())).unwrap();
        assert!((info.duration - 8.36).abs() < 0.1, "duration {}", info.duration);
        assert_eq!(info.codec, "h264");
        assert!(info.has_audio);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn smaller_copies_estimate() {
        let small = estimate_size(Quality::Small, 10., None, true);
        let original = estimate_size(Quality::Original, 10., None, true);
        assert!(small < original);
        // Never above the source's own bitrate
        assert_eq!(estimate_size(Quality::Original, 10., Some(800_000), true), 1_000_000);
    }
}
