#!/bin/sh
# Builds dist/Blick-<version>-<arch>.AppImage inside the arch image
#
# The AppImage carries everything it runs on, the C library included, so it
# starts on any Linux system, also older ones. quick-sharun from the Anylinux
# AppImages project finds and bundles the libraries:
# https://github.com/pkgforge-dev/Anylinux-AppImages
#
# Needs the Arch package of the same version in dist/, from arch/build.sh.
set -eu
cd /src
version=$(sed -n "s/^  version: '\(.*\)',/\1/p" meson.build)
arch=$(uname -m)

pacman -Syu --noconfirm --needed wget patchelf binutils strace xorg-server-xvfb librsvg gst-libav gst-plugins-bad-libs \
    tesseract-data-spa tesseract-data-fra tesseract-data-deu tesseract-data-por \
    tesseract-data-rus tesseract-data-pol tesseract-data-tur
pacman -U --noconfirm "$(ls /src/dist/blick-"$version"-*-"$arch".pkg.tar.zst | head -1)"

work=/tmp/appimage
rm -rf "$work"
mkdir -p "$work"
cd "$work"

# File managers and the AppImage catalog need a PNG icon
rsvg-convert -w 256 -h 256 /usr/share/icons/hicolor/scalable/apps/io.github.Replexo.Blick.svg \
    -o "$work/io.github.Replexo.Blick.png"

wget -q https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/refs/heads/main/useful-tools/quick-sharun.sh \
    -O quick-sharun
chmod +x quick-sharun

export ARCH="$arch"
export ICON="$work/io.github.Replexo.Blick.png"
export DESKTOP=/usr/share/applications/io.github.Replexo.Blick.desktop
export OUTNAME="Blick-$version-$arch.AppImage"
export OUTPATH=/src/dist
export UPINFO="gh-releases-zsync|Replexo|Blick|latest|Blick-*$arch.AppImage.zsync"
export DEPLOY_GTK=1 DEPLOY_GSTREAMER=1 DEPLOY_GLYCIN=1
./quick-sharun /usr/bin/blick /usr/bin/bwrap

# Language data for text recognition, found through TESSDATA_PREFIX
mkdir -p AppDir/share/tessdata
cp /usr/share/tessdata/*.traineddata AppDir/share/tessdata/
echo 'TESSDATA_PREFIX=${SHARUN_DIR}/share/tessdata' >> AppDir/.env

./quick-sharun --make-appimage
