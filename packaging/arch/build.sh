#!/bin/sh
# Builds dist/blick-<version>-<rel>-<arch>.pkg.tar.zst inside the arch image
set -eu
version=$(sed -n "s/^  version: '\(.*\)',/\1/p" /src/meson.build)
work=/home/builder/pkg
rm -rf "$work"
mkdir -p "$work"
# makepkg builds from a git repository of the sources
cp -r /src "$work/blick-src"
rm -rf "$work/blick-src/dist"
git -C "$work/blick-src" init -q
git -C "$work/blick-src" add -A
git -C "$work/blick-src" -c user.name=build -c user.email=build@localhost commit -qm build
sed -e "s/^pkgver=.*/pkgver=$version/" \
    -e "s|^source=.*|source=(\"blick::git+file://$work/blick-src\")|" \
    /src/packaging/arch/PKGBUILD > "$work/PKGBUILD"
chown -R builder "$work"
cd "$work"
su builder -c "CARGO_HOME=/home/builder/.cargo makepkg -f"
cp blick-*.pkg.tar.zst /src/dist/
