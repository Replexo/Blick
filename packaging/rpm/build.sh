#!/bin/sh
# Builds dist/blick-<version>.<arch>.rpm inside the rpm image
set -eu
cd /src
version=$(sed -n "s/^  version: '\(.*\)',/\1/p" meson.build)
rpmbuild -bb packaging/rpm/blick.spec \
    --define "app_version $version" --define "src /src" \
    --define "_topdir /tmp/rpmbuild"
mkdir -p dist
cp /tmp/rpmbuild/RPMS/*/blick-*.rpm dist/
