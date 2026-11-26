#!/bin/sh
# Builds dist/Blick-<version>-<arch>.AppImage inside the deb image
#
# Image loading uses the host's glycin loaders, which sandbox each image.
# Videos play with the host's GStreamer and its plugins, so the GStreamer
# libraries are left out of the AppImage to match the host's plugins.
# Editing videos uses the host's ffmpeg.
set -eu
cd /src
version=$(sed -n "s/^  version: '\(.*\)',/\1/p" meson.build)
appdir=/tmp/AppDir
rm -rf /tmp/build-appimage "$appdir"
meson setup /tmp/build-appimage --prefix=/usr --buildtype=release -Dupdate_check=true
meson compile -C /tmp/build-appimage
DESTDIR="$appdir" meson install -C /tmp/build-appimage

# Language data for text recognition, found through TESSDATA_PREFIX
mkdir -p "$appdir/usr/share/tessdata"
cp /opt/tessdata/*.traineddata "$appdir/usr/share/tessdata/"
mkdir -p "$appdir/apprun-hooks"
cat > "$appdir/apprun-hooks/blick.sh" <<'HOOK'
export TESSDATA_PREFIX="$APPDIR/usr/share/tessdata"
HOOK

# The GTK plugin copies GTK's module folder, which Ubuntu only has with
# optional modules installed
mkdir -p "$(pkg-config --variable=libdir gtk4)/gtk-4.0/4.0.0"

export DEPLOY_GTK_VERSION=4 OUTPUT="/src/dist/Blick-$version-$(uname -m).AppImage"
export APPIMAGE_EXTRACT_AND_RUN=1 LDAI_RUNTIME_FILE=/opt/linuxdeploy/runtime-x86_64 LDAI_UPDATE_INFORMATION="gh-releases-zsync|Replexo|Blick|latest|Blick-*$(uname -m).AppImage.zsync"
mkdir -p /src/dist
cd /opt/linuxdeploy
# File managers and the AppImage catalog need a PNG icon as .DirIcon
icon="$appdir/usr/share/icons/hicolor/256x256/apps/io.github.Replexo.Blick.png"
mkdir -p "$(dirname "$icon")"
rsvg-convert -w 256 -h 256 "$appdir/usr/share/icons/hicolor/scalable/apps/io.github.Replexo.Blick.svg" -o "$icon"
./linuxdeploy-x86_64.AppImage --appdir "$appdir" \
    --desktop-file "$appdir/usr/share/applications/io.github.Replexo.Blick.desktop" \
    --icon-file "$icon" \
    --icon-file "$appdir/usr/share/icons/hicolor/scalable/apps/io.github.Replexo.Blick.svg" \
    --exclude-library 'libgst*' \
    --plugin gtk
rm -f "$appdir/io.github.Replexo.Blick.svg" "$appdir/.DirIcon"
cp "$icon" "$appdir/io.github.Replexo.Blick.png"
cp "$icon" "$appdir/.DirIcon"
./linuxdeploy-x86_64.AppImage --appdir "$appdir" --output appimage
mv Blick*.zsync /src/dist/ 2>/dev/null || true
