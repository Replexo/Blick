<p align="center">
  <img src="data/icons/hicolor/scalable/apps/io.github.Replexo.Blick.svg" width="80" alt="Blick icon">
</p>

<div align="center">

# Blick

**Image viewer, video player and photo editor for Linux**

*Previously Glance*

[![Latest release](https://img.shields.io/github/v/release/Replexo/Blick?label=release)](https://github.com/Replexo/Blick/releases/latest)
[![License: GPL-3.0-or-later](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)](COPYING.md)
[![GTK 4 and libadwaita](https://img.shields.io/badge/GTK%204-libadwaita-4a86cf)](https://gtk.org)

<img src="data/screenshots/promo.webp" width="800" alt="Blick marking up, adjusting and cropping a photo, then trimming a video, on Linux">

</div>

Blick is a fast image viewer, video player and photo and video editor for Linux and GNOME. Open a picture, annotate it, adjust its colours, crop it, copy the text in it, and save, all in the same window. Videos play in the same window and can be trimmed, muted, rotated and saved smaller. It is a fork of [Loupe](https://gitlab.gnome.org/GNOME/loupe), the GNOME image viewer, so browsing stays fast, sandboxed and touch-friendly.

Blick was called **Glance** up to version 1.2. Installing Blick replaces Glance: the files Glance opened open in Blick, and the Flatpak takes Glance's settings along.

Blick runs on any Linux desktop and comes as a Flatpak, an AppImage, and packages for Debian and Ubuntu, Fedora and Arch Linux.

## Features

### Viewing

Everything you know from Loupe: fast and sandboxed image loading, smooth zoom and gestures, image metadata, printing and setting a picture as the background.

Small thumbnails float over the bottom of the photo, on a soft shade, to jump to the other photos in the folder. They show when you move the pointer to the bottom and fade away after a few seconds. The arrow keys and swipes move between photos, and between videos when a video is open.

Windows open centred, at most 60% of the screen, and keep their size as you move between files.

### Videos

Videos play like in GNOME's video player: edge to edge, under a see-through header that fades away with the controls. Seek with a frame preview, skip ten seconds, step through single frames, change the speed, repeat, and pick up where you left off. Playing and pausing is for the play button and <kbd>Space</kbd>, so a click on the picture never stops it by accident. When paused, zoom in, copy the frame, or select its text with live text.

Subtitles with the same name as the video load by themselves and stay sharp at any size. The **CC** button picks subtitle and audio tracks, the subtitle size and delay, or other subtitle files.

Motion photos from Google, Samsung and iPhone Live Photos get a small play button for their moving part.

### Video editing

Trim with a filmstrip like on the iPhone: drag the handles, and hold one still to zoom in until single frames show. Remove the sound, rotate and flip. Cuts are frame accurate and only the pieces at the cuts are encoded again, so the rest keeps its quality. **Save Smaller Copy** makes an H.264 or VP9 copy in three sizes, and **Save Frame as Image** opens the current frame in the photo editor. Saving videos needs ffmpeg.

### Markup

- Brush, highlighter, line, arrow, rectangle, ellipse and text tools.
- An eraser that rubs out markup where you drag.
- Preset and custom colours, an eyedropper to pick any colour from the photo, four line widths, and outlined or filled shapes.
- Select, move, resize and rotate anything you have drawn.
- Type text straight onto the picture, choose its font, size and background, and edit it again later.

### Adjust

Brightness, contrast, highlights, shadows and saturation, each on its own slider. Hold the compare button to see the original.

### Crop

Free or fixed aspect ratios, rotate, flip and resize. Straighten tilted photos, fix vertical and horizontal perspective, and zoom in while you crop.

### Live text (OCR)

Select and copy text straight from any image: screenshots, scanned pages, photos of signs, screens and documents. Blick finds the text by itself with [Tesseract](https://github.com/tesseract-ocr/tesseract), also when it is turned, tilted or slightly curved, and everything stays on your computer.

It reads English, Spanish, French, German, Portuguese, Russian, Polish and Turkish, using your device language together with English.

<img src="data/screenshots/live-text.webp" width="640" alt="Copying text from a photo of a recipe with Blick live text OCR">

### And more

- Undo and redo across every kind of edit.
- Zoom inside the editor for detailed work.
- **Save a Copy**, or **Replace Original** and move the old file to the trash.
- Blick opens all your images and videos after its first start. **Make Default Image Viewer** and **Make Default Video Player** in the main menu set it again later.
- The Flatpak and AppImage tell you about a new version once a day, with **Update** and **Later**.

### Languages

Blick is fully translated into Spanish, German, French, Brazilian Portuguese, Russian, Polish and Turkish. The viewer is also translated into the other 52 languages of Loupe.

## Install

Download the file for your system from the [latest release](https://github.com/Replexo/Blick/releases/latest):

| System | File | Install |
|---|---|---|
| Any Linux (Flatpak) | [`Blick-1.3-x86_64.flatpak`](https://github.com/Replexo/Blick/releases/latest/download/Blick-1.3-x86_64.flatpak) | `flatpak install --user Blick-1.3-x86_64.flatpak` |
| Any Linux (AppImage) | [`Blick-1.3-x86_64.AppImage`](https://github.com/Replexo/Blick/releases/latest/download/Blick-1.3-x86_64.AppImage) | `chmod +x Blick-1.3-x86_64.AppImage`, then run it |
| Ubuntu 25.10 and newer | [`blick_1.3_amd64.deb`](https://github.com/Replexo/Blick/releases/latest/download/blick_1.3_amd64.deb) | `sudo apt install ./blick_1.3_amd64.deb` |
| Fedora 43 and newer | [`blick-1.3-1.fc43.x86_64.rpm`](https://github.com/Replexo/Blick/releases/latest/download/blick-1.3-1.fc43.x86_64.rpm) | `sudo dnf install ./blick-1.3-1.fc43.x86_64.rpm` |
| Arch Linux | [`blick-1.3-1-x86_64.pkg.tar.zst`](https://github.com/Replexo/Blick/releases/latest/download/blick-1.3-1-x86_64.pkg.tar.zst) | `sudo pacman -U blick-1.3-1-x86_64.pkg.tar.zst` |

The Flatpak uses the GNOME 51 runtime, which Flatpak downloads from [Flathub](https://flathub.org/setup) if you don't have it yet. Blick is coming to Flathub itself soon, with automatic updates. You can check your download against `SHA256SUMS` on the release page.

To build it yourself instead, see [Build from source](#build-from-source).

### Coming from Glance

Install Blick, start it once, and it takes over the files Glance opened. Remove Glance afterwards, for example with `flatpak uninstall io.github.Replexo.Glance` or your package manager.

### Privacy and network access

Blick only goes online to check GitHub for updates. Your photos and videos never leave your computer, and there are no accounts or analytics. To keep the Flatpak offline, run `flatpak override --user --unshare=network io.github.Replexo.Blick`. Once Blick is on Flathub, which handles updates itself, the update check and the network access will be removed.

## Keyboard shortcuts

| Key | Action |
|---|---|
| <kbd>E</kbd> | Open the editor |
| <kbd>C</kbd> | Crop |
| <kbd>A</kbd> | Select, move and resize markup |
| <kbd>B</kbd> | Brush |
| <kbd>L</kbd> / <kbd>Shift</kbd>+<kbd>L</kbd> | Line / arrow |
| <kbd>R</kbd> / <kbd>O</kbd> | Rectangle / ellipse |
| <kbd>T</kbd> | Text |
| <kbd>E</kbd> (in the editor) | Eraser |
| <kbd>Shift</kbd> while drawing | Squares, circles and lines at 45° steps |
| <kbd>Ctrl</kbd>+<kbd>Z</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>Z</kbd> | Undo / redo |
| <kbd>Ctrl</kbd>+<kbd>S</kbd> / <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd> | Replace original / save a copy |
| <kbd>Space</kbd> | Play or pause a video |
| <kbd>←</kbd> / <kbd>→</kbd> | Skip a video back or forward ten seconds |
| <kbd>,</kbd> / <kbd>.</kbd> | Previous / next frame of a video |
| <kbd>M</kbd> / <kbd>F</kbd> | Mute / fullscreen a video |

All shortcuts are listed under **Keyboard Shortcuts** in the main menu.

## Build from source

You need GTK 4.20, libadwaita 1.8, libgweather 4, libseccomp, the glycin loaders, Tesseract 5 with Leptonica, and Rust 1.93 or newer.

```sh
meson setup builddir
ninja -C builddir
ninja -C builddir install
```

To build and install the Flatpak:

```sh
flatpak-builder --user --install --install-deps-from=flathub build-flatpak build-aux/flatpak/io.github.Replexo.Blick.json
```

To build a development version with its own app ID, so it can sit next to an installed release:

```sh
meson setup builddir -Dprofile=dev -Dapp_id_suffix=.Devel
```


## Contributing

Bug reports and ideas are welcome in the [issue tracker](https://github.com/Replexo/Blick/issues). See [CONTRIBUTING.md](CONTRIBUTING.md) before opening a pull request.

## Credits

- Based on [Loupe](https://gitlab.gnome.org/GNOME/loupe) by Christopher Davis, Sophie Herold and the GNOME contributors.
- Icons from [MingCute Icons](https://www.mingcute.com/) (Apache-2.0) and [coolicons](https://github.com/krystonschwarze/coolicons) (MIT). See `data/COPYING.mingcute` and `data/COPYING.coolicons`.
- The set of markup tools follows [Gradia](https://github.com/AlexanderVanhee/Gradia).
- Text recognition by [Tesseract](https://github.com/tesseract-ocr/tesseract) (Apache-2.0).
- Translations of the viewer come from the Loupe translators.

## License

Blick is free software, released under the [GNU General Public License, version 3 or later](COPYING.md).
