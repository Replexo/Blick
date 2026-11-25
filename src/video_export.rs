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

    // Errors are read at the same time as the progress: a full pipe
    // would make ffmpeg wait
    let stderr = process.stderr_pipe().map(|stderr| {
        glib::spawn_future_local(async move {
            let lines = gio::DataInputStream::new(&stderr);
            let mut last = String::new();
            while let Ok(Some(line)) = lines.read_line_utf8_future(glib::Priority::DEFAULT).await {
                if !line.trim().is_empty() {
                    last = line.to_string();
                }
            }
            last
        })
    });

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
    let stderr = match stderr {
        Some(task) => task.await.unwrap_or_default(),
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
        // Data tracks, like an iPhone's, are left out: many containers
        // refuse them
        "-dn".into(),
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
        // The pixels stay as stored, like in the copied middle; the
        // rotation is written again when the pieces are joined
        let mut args = vec![
            "-noautorotate".to_string(),
            "-ss".into(),
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

    // MPEG-TS has no rotation, so the source's comes back with the edit's
    let mut args = display_options(edit, source);
    if args.is_empty() && source.rotation.rem_euclid(360) != 0 {
        args = vec!["-display_rotation:v:0".into(), source.rotation.rem_euclid(360).to_string()];
    }
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
    ffmpeg(&args, total, total, progress, cancellable).await
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
#[allow(clippy::too_many_arguments)]
pub async fn save_smaller(
    input: &Path,
    output_path: &Path,
    edit: &VideoEdit,
    frames: &FrameEdits,
    quality: Quality,
    progress: impl Fn(f64) + 'static,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    let progress: Rc<dyn Fn(f64)> = Rc::new(progress);
    let source = probe(input, cancellable).await?;
    let h264 = has_h264(cancellable).await;
    // Only ever smaller, keeping the aspect ratio and even sizes
    let shrink = quality.max_size().map(|max| {
        format!("scale='if(gt(iw,ih),min({max},iw),-2)':'if(gt(iw,ih),-2,min({max},ih))'")
    });

    let dir = temp_dir()?;
    let mut args;
    if let Some((rotation, flip)) = frames.as_orientation() {
        let mut edit = *edit;
        edit.rotation = (edit.rotation + rotation).rem_euclid(4);
        edit.flip ^= flip;
        args = display_options(&edit, &source);
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
        if let Some(shrink) = shrink {
            args.extend(["-vf".into(), shrink]);
        }
    } else {
        // The photo editor's edits, then the smaller size
        let video_size = source_size(input, cancellable).await.unwrap_or(frames.frame_size);
        let (mut graph, layer) = frame_filters(frames, video_size, &dir)?;
        let mut last = "out";
        if let Some(shrink) = shrink {
            graph.push_str(&format!(";[out]{shrink}[small]"));
            last = "small";
        }
        args = vec![
            "-ss".into(),
            seconds(edit.start),
            "-to".into(),
            seconds(edit.end),
            "-i".into(),
            input.to_string_lossy().to_string(),
        ];
        if let Some(layer) = &layer {
            args.extend(["-loop".into(), "1".into(), "-i".into(), layer.to_string_lossy().to_string()]);
        }
        args.extend([
            "-filter_complex".into(),
            graph,
            "-map".into(),
            format!("[{last}]"),
            "-shortest".into(),
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

/// Edits made in the photo editor's tabs, applied to every frame
#[derive(Debug, Clone, Default)]
pub struct FrameEdits {
    pub operations: Option<glycin::Operations>,
    pub pixel: crate::editing::render::PixelEdits,
    /// Size of the frame the edits were made on
    pub frame_size: (u32, u32),
}

impl FrameEdits {
    /// Rotation in steps of 90° to the left and a horizontal flip, if the
    /// edits are nothing else, so they can stay display metadata
    pub fn as_orientation(&self) -> Option<(i32, bool)> {
        use glycin::Operation;
        use gufo::common::orientation::Rotation;

        if !self.pixel.is_empty() {
            return None;
        }
        // The edits as `rotate left r times` after `flip f times`
        let (mut r, mut f) = (0i32, 0i32);
        for operation in self.operations.iter().flat_map(|o| o.operations()) {
            match operation {
                Operation::Rotate(Rotation::_0) => {}
                Operation::Rotate(Rotation::_90) => r += 1,
                Operation::Rotate(Rotation::_180) => r += 2,
                Operation::Rotate(Rotation::_270) => r += 3,
                Operation::MirrorHorizontally => {
                    r = -r;
                    f += 1;
                }
                Operation::MirrorVertically => {
                    r = 2 - r;
                    f += 1;
                }
                _ => return None,
            }
        }
        Some((r.rem_euclid(4), f.rem_euclid(2) == 1))
    }
}

/// ffmpeg filters that give every frame the edits; writes the colour table
/// and the markup layer into `dir`
fn frame_filters(edits: &FrameEdits, video_size: (u32, u32), dir: &Path) -> Result<(String, Option<PathBuf>), Error> {
    use crate::editing::geometry;
    use glycin::Operation;
    use gufo::common::orientation::Rotation;

    let (fw, fh) = edits.frame_size;
    let mut chain: Vec<String> = Vec::new();
    // Edits were made on the frame as shown, which may be scaled
    if (fw, fh) != video_size && fw > 0 && fh > 0 {
        chain.push(format!("scale={fw}:{fh}"));
    }

    // Straighten and perspective: the input points of the output corners
    let pixel = &edits.pixel;
    if !pixel.geometry.is_identity() {
        let size = (fw as f64, fh as f64);
        if let Some(inverse) = geometry::invert(&pixel.geometry.matrix(size)) {
            let corners = [(0., 0.), (size.0, 0.), (0., size.1), (size.0, size.1)];
            let points: Vec<String> = corners
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let (x, y) = geometry::transform(&inverse, *c);
                    format!("x{i}={x:.3}:y{i}={y:.3}")
                })
                .collect();
            chain.push(format!("perspective={}:interpolation=cubic", points.join(":")));
        }
    }

    // Crop, rotate and flip, in order
    let (mut width, mut height) = (fw, fh);
    for operation in edits.operations.iter().flat_map(|o| o.operations()) {
        match operation {
            Operation::Clip((x, y, w, h)) => {
                chain.push(format!("crop={w}:{h}:{x}:{y}"));
                (width, height) = (*w, *h);
            }
            Operation::MirrorHorizontally => chain.push("hflip".into()),
            Operation::MirrorVertically => chain.push("vflip".into()),
            Operation::Rotate(Rotation::_90) => {
                chain.push("transpose=2".into());
                (width, height) = (height, width);
            }
            Operation::Rotate(Rotation::_180) => chain.push("hflip,vflip".into()),
            Operation::Rotate(Rotation::_270) => {
                chain.push("transpose=1".into());
                (width, height) = (height, width);
            }
            _ => {}
        }
    }

    // Colour adjustments as a 3D table, exactly as the photo editor does them
    if !pixel.adjustments.is_identity() {
        let table = dir.join("adjust.cube");
        write_color_table(&pixel.adjustments, &table)?;
        chain.push(format!("lut3d=file={}", filter_path(&table)));
    }

    // Markup as a transparent layer over the edited frame
    let mut layer = None;
    if !pixel.markup.is_empty() {
        let mut image = image::RgbaImage::new(width.max(1), height.max(1));
        let (matrix, _) = crate::editing::markup::operations_matrix(
            edits.operations.as_ref(),
            (fw as f64, fh as f64),
        );
        crate::editing::markup::flatten(&mut image, &pixel.markup, matrix)
            .map_err(|err| Error::Failed(err.to_string()))?;
        let path = dir.join("markup.png");
        use gdk::prelude::TextureExt;
        let texture = gdk::MemoryTexture::new(
            width as i32,
            height as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(image.into_raw()),
            width as usize * 4,
        );
        texture
            .save_to_png(&path)
            .map_err(|err| Error::Failed(err.to_string()))?;
        layer = Some(path);
    }

    let mut graph = format!(
        "[0:v]{}[edited]",
        if chain.is_empty() { "null".into() } else { chain.join(",") }
    );
    let mut last = "edited".to_string();
    if layer.is_some() {
        graph.push_str(";[edited][1:v]overlay=0:0[marked]");
        last = "marked".into();
    }
    // Resize, to even sizes for the encoders
    let (mut out_w, mut out_h) = (width, height);
    if let Some(scale) = pixel.resize {
        (out_w, out_h) = crate::editing::render::scaled_size((width, height), scale);
    }
    out_w -= out_w % 2;
    out_h -= out_h % 2;
    graph.push_str(&format!(
        ";[{last}]scale={}:{},format=yuv420p[out]",
        out_w.max(2),
        out_h.max(2)
    ));
    Ok((graph, layer))
}

/// A path inside a filter graph, with the characters ffmpeg treats as
/// special escaped
fn filter_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
        .replace(',', "\\,")
}

/// Writes `adjustments` as an `.cube` 3D colour table
fn write_color_table(adjustments: &crate::editing::adjust::Adjustments, path: &Path) -> Result<(), Error> {
    const SIZE: usize = 33;
    let step = |i: usize| ((i as f64 / (SIZE - 1) as f64) * 255.).round() as u8;
    let mut pixels = Vec::with_capacity(SIZE * SIZE * SIZE * 4);
    // Red changes fastest in .cube files
    for b in 0..SIZE {
        for g in 0..SIZE {
            for r in 0..SIZE {
                pixels.extend_from_slice(&[step(r), step(g), step(b), 255]);
            }
        }
    }
    adjustments.apply_rgba8(&mut pixels);
    let mut text = format!("LUT_3D_SIZE {SIZE}\n");
    for pixel in pixels.as_chunks::<4>().0 {
        text.push_str(&format!(
            "{:.6} {:.6} {:.6}\n",
            pixel[0] as f64 / 255.,
            pixel[1] as f64 / 255.,
            pixel[2] as f64 / 255.
        ));
    }
    std::fs::write(path, text).map_err(|err| Error::Failed(err.to_string()))
}

/// Saves `edit` with the photo editor's `frames` edits; rotation and flips
/// alone stay metadata, everything else is encoded again
pub async fn save_edited(
    input: &Path,
    output_path: &Path,
    edit: &VideoEdit,
    frames: &FrameEdits,
    progress: impl Fn(f64) + 'static,
    cancellable: &gio::Cancellable,
) -> Result<(), Error> {
    if let Some((rotation, flip)) = frames.as_orientation() {
        let mut edit = *edit;
        edit.rotation = (edit.rotation + rotation).rem_euclid(4);
        edit.flip ^= flip;
        return save(input, output_path, &edit, progress, cancellable).await;
    }

    let progress: Rc<dyn Fn(f64)> = Rc::new(progress);
    let source = probe(input, cancellable).await?;
    let dir = temp_dir()?;
    let video_size = source_size(input, cancellable).await.unwrap_or(frames.frame_size);
    let (graph, layer) = frame_filters(frames, video_size, &dir)?;

    let encoder = encoder_for(&source.codec)
        .unwrap_or_else(|| vec!["-c:v", "libx264", "-crf", "17", "-preset", "medium"]);
    let mut args = vec![
        "-ss".to_string(),
        seconds(edit.start),
        "-to".into(),
        seconds(edit.end),
        "-i".into(),
        input.to_string_lossy().to_string(),
    ];
    if let Some(layer) = &layer {
        args.extend(["-loop".into(), "1".into(), "-i".into(), layer.to_string_lossy().to_string()]);
    }
    args.extend([
        "-filter_complex".into(),
        graph,
        "-map".into(),
        "[out]".into(),
        "-shortest".into(),
    ]);
    args.extend(encoder.iter().map(|x| x.to_string()));
    if edit.mute || !source.has_audio {
        args.push("-an".into());
    } else {
        args.extend(["-map".into(), "0:a?".into(), "-c:a".into(), "copy".into()]);
    }
    args.push(output_path.to_string_lossy().to_string());
    ffmpeg(&args, edit.end - edit.start, 0., &progress, cancellable).await
}

/// Width and height as shown, after the rotation in the file
async fn source_size(input: &Path, cancellable: &gio::Cancellable) -> Option<(u32, u32)> {
    let input = input.to_string_lossy().to_string();
    let json = output(
        "ffprobe",
        &["-v", "error", "-select_streams", "v:0", "-print_format", "json", "-show_streams", &input],
        cancellable,
    )
    .await
    .ok()?;
    let json: serde_json::Value = serde_json::from_str(&json).ok()?;
    let stream = json["streams"].as_array()?.first()?.clone();
    let (w, h) = (stream["width"].as_u64()? as u32, stream["height"].as_u64()? as u32);
    let rotation = stream["side_data_list"]
        .as_array()
        .and_then(|list| list.iter().find_map(|d| d["rotation"].as_f64()))
        .unwrap_or_default() as i32;
    Some(if rotation.rem_euclid(180) == 90 { (h, w) } else { (w, h) })
}

/// A folder for the pieces of an export, removed when dropped, also when
/// the export fails or is cancelled
struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir() -> Result<TempDir, Error> {
    thread_local! {
        static COUNTER: Cell<u32> = const { Cell::new(0) };
    }
    let n = COUNTER.get();
    COUNTER.set(n + 1);
    let dir = glib::user_cache_dir()
        .join("blick")
        .join(format!("export-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|err| Error::Failed(err.to_string()))?;
    Ok(TempDir(dir))
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
        let dir = std::env::temp_dir().join(format!("blick-cut-{}", std::process::id()));
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
    fn orientation_only_edits_stay_metadata() {
        use glycin::{Operation, Operations};
        use gufo::common::orientation::Rotation;

        let mut edits = FrameEdits {
            operations: Some(Operations::new(vec![Operation::Rotate(Rotation::_90)])),
            ..Default::default()
        };
        assert_eq!(edits.as_orientation(), Some((1, false)));
        edits.operations = Some(Operations::new(vec![Operation::MirrorHorizontally]));
        assert_eq!(edits.as_orientation(), Some((0, true)));
        edits.operations = Some(Operations::new(vec![Operation::Clip((0, 0, 10, 10))]));
        assert_eq!(edits.as_orientation(), None);
    }

    /// Crops and brightens a generated video; skipped without ffmpeg
    #[test]
    fn frame_edits_crop_and_adjust() {
        use glycin::{Operation, Operations};
        if glib::find_program_in_path("ffmpeg").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("blick-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        let status = std::process::Command::new("ffmpeg")
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "color=c=gray:size=320x240:rate=25"])
            .args(["-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success());

        let mut pixel = crate::editing::render::PixelEdits::default();
        pixel.adjustments.brightness = 0.5;
        let frames = FrameEdits {
            operations: Some(Operations::new(vec![Operation::Clip((10, 20, 200, 100))])),
            pixel,
            frame_size: (320, 240),
        };
        let output = dir.join("out.mp4");
        let context = glib::MainContext::default();
        let result = context.block_on(save_edited(
            &input,
            &output,
            &VideoEdit::new(2.),
            &frames,
            |_| {},
            &gio::Cancellable::new(),
        ));
        assert!(result.is_ok(), "{result:?}");
        let size = context
            .block_on(source_size(&output, &gio::Cancellable::new()))
            .unwrap();
        assert_eq!(size, (200, 100));
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
