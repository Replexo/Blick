#!/bin/sh
# Builds dist/blink-<version>-<rel>-<arch>.pkg.tar.zst inside the arch image
set -eu
version=$(sed -n "s/^  version: '\(.*\)',/\1/p" /src/meson.build)
work=/home/builder/pkg
rm -rf "$work"
mkdir -p "$work"
# makepkg builds from a git repository of the sources
cp -r /src "$work/blink-src"
rm -rf "$work/blink-src/dist"
git -C "$work/blink-src" init -q
git -C "$work/blink-src" add -A
git -C "$work/blink-src" -c user.name=build -c user.email=build@localhost commit -qm build
sed -e "s/^pkgver=.*/pkgver=$version/" \
    -e "s|^source=.*|source=(\"blink::git+file://$work/blink-src\")|" \
    /src/packaging/arch/PKGBUILD > "$work/PKGBUILD"
chown -R builder "$work"
cd "$work"
su builder -c "CARGO_HOME=/home/builder/.cargo makepkg -f"
cp blink-*.pkg.tar.zst /src/dist/
