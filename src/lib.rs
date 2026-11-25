// Copyright (c) 2024-2026 Sophie Herold
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

#![allow(clippy::new_without_default)]

/*!
# Blick Image Viewer

Code documentation

# Widget Structure

Simplified widget arrangement including the most important widgets.

-   [`LpWindow`]
    -   [`LpImageWindow`]
        -   [`LpPropertiesView`]
        -   [`LpImageView`]
            -   [`LpSlidingView`]
                -   [`LpImagePage`]
                    -   [`GtkScrolledWindow`]
                        -   [`LpImage`]
                -   …
    -   [`LpEditWindow`]
        - [`LpEditCrop`]


[`GtkScrolledWindow`]: gtk::ScrolledWindow
[`LpEditCrop`]: widgets::edit::LpEditCrop
[`LpEditWindow`]: widgets::LpEditWindow
[`LpImagePage`]: widgets::LpImagePage
[`LpImageView`]: widgets::LpImageView
[`LpImageWindow`]: widgets::LpImageWindow
[`LpImage`]: widgets::LpImage
[`LpPropertiesView`]: widgets::LpPropertiesView
[`LpSlidingView`]: widgets::LpSlidingView
[`LpWindow`]: widgets::LpWindow
*/

use gettextrs::*;
use gtk::gio::prelude::*;
use gtk::gio::{self};

pub mod about;
pub mod application;
pub mod config;
pub mod default_app;
pub mod decoder;
pub mod editing;
pub mod file_model;
pub mod metadata;
pub mod mpris;
pub mod ocr;
pub mod updates;
pub mod util;
pub mod video_export;
pub mod widgets;

mod deps {
    pub use gtk::{cairo, gdk, gio, glib, graphene, gsk};
}

use application::LpApplication;
use deps::*;
use tracing_subscriber::layer::*;
use tracing_subscriber::util::*;

static GRESOURCE_BYTES: &[u8] =
    gvdb_macros::include_gresource_from_dir!("/io/github/Replexo/Blick", "data/resources");

pub fn main() -> glib::ExitCode {
    // Follow G_MESSAGES_DEBUG env variable
    let default_level =
        if !glib::log_writer_default_would_drop(glib::LogLevel::Debug, Some("blick")) {
            tracing_subscriber::filter::LevelFilter::DEBUG
        } else {
            tracing_subscriber::filter::LevelFilter::ERROR
        };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(default_level.into())
                .from_env_lossy(),
        )
        .with(
            tracing_subscriber::fmt::Layer::default()
                .compact()
                .with_line_number(true)
                .with_writer(std::io::stderr),
        )
        .init();

    tracing::debug!("Logger initialized");

    prefer_integrated_gpu();

    // Text recognition runs several Tesseract instances side by side. With
    // a Tesseract built with OpenMP, its own threads would compete with them
    // and keep spinning after each read. OpenMP reads its settings when it
    // is loaded, before `main`, so the app starts itself again with them
    // when it is loaded.
    if std::env::var_os("OMP_THREAD_LIMIT").is_none()
        && std::fs::read_to_string("/proc/self/maps").is_ok_and(|maps| maps.contains("libgomp"))
    {
        use std::os::unix::process::CommandExt;

        if let Ok(exe) = std::env::current_exe() {
            let err = std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .env("OMP_THREAD_LIMIT", "1")
                .env("OMP_WAIT_POLICY", "passive")
                .exec();
            tracing::debug!("Could not restart with OpenMP settings: {err}");
        }
    }

    unsafe {
        setlocale(LocaleCategory::LcAll, "");
    }
    bindtextdomain("blick", config::LOCALEDIR).unwrap();
    textdomain("blick").unwrap();

    tracing::trace!("gettext initialized");

    gio::resources_register(
        &gio::Resource::from_data(&glib::Bytes::from_static(GRESOURCE_BYTES)).unwrap(),
    );

    tracing::trace!("Gio resources registered");

    // Video playback
    if let Err(err) = gst::init() {
        tracing::warn!("Could not initialize GStreamer: {err}");
    }

    LpApplication::new().run()
}

/// With NVIDIA's drivers and a built-in GPU, draws with the built-in one
///
/// It starts in half the time, and decodes videos itself, so frames do not
/// have to move between the GPUs. Skipped when the user picked a GPU.
fn prefer_integrated_gpu() {
    const CHOSEN: &[&str] = &[
        "VK_LOADER_DRIVERS_SELECT",
        "VK_DRIVER_FILES",
        "VK_ICD_FILENAMES",
        "__EGL_VENDOR_LIBRARY_FILENAMES",
        "__NV_PRIME_RENDER_OFFLOAD",
        "__GLX_VENDOR_LIBRARY_NAME",
        "DRI_PRIME",
        "GSK_RENDERER",
    ];
    if CHOSEN.iter().any(|name| std::env::var_os(name).is_some()) {
        return;
    }

    let vendors: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("device/vendor")).ok())
        .map(|vendor| vendor.trim().to_lowercase())
        .collect();
    // Its drivers come first even when the GPU itself is asleep
    let has_nvidia = vendors.iter().any(|vendor| vendor == "0x10de")
        || [
            "/usr/share/glvnd/egl_vendor.d/10_nvidia.json",
            "/usr/share/vulkan/icd.d/nvidia_icd.json",
            "/etc/vulkan/icd.d/nvidia_icd.json",
        ]
        .iter()
        .any(|path| std::path::Path::new(path).exists());
    // Intel or AMD
    let has_integrated = vendors.iter().any(|vendor| vendor == "0x8086" || vendor == "0x1002");
    let mesa_egl = ["/usr/share/glvnd/egl_vendor.d/50_mesa.json"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists());
    let mesa_vulkan = ["/usr/share/vulkan/icd.d", "/etc/vulkan/icd.d"]
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .any(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            name.starts_with("intel_icd") || name.starts_with("radeon_icd")
        });

    if has_nvidia
        && has_integrated
        && mesa_vulkan
        && let Some(mesa_egl) = mesa_egl
    {
        tracing::debug!("Drawing with the built-in GPU");
        // SAFETY: Before any other thread is started
        unsafe {
            std::env::set_var("VK_LOADER_DRIVERS_SELECT", "*intel*,*radeon*");
            std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa_egl);
        }
    }
}
