#!/bin/sh
# Builds dist/blink-<version>.<arch>.rpm inside the rpm image
set -eu
cd /src
version=$(sed -n "s/^  version: '\(.*\)',/\1/p" meson.build)
rpmbuild -bb packaging/rpm/blink.spec \
    --define "app_version $version" --define "src /src" \
    --define "_topdir /tmp/rpmbuild"
mkdir -p dist
cp /tmp/rpmbuild/RPMS/*/blink-*.rpm dist/
