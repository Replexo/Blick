# Release packages

```sh
./packaging/build.sh              # all of them
./packaging/build.sh deb appimage # some of them
```

Packages land in `dist/`, with a `SHA256SUMS` file that the update checker uses to verify downloads. Upload both to the GitHub release.

| Package | Built in | Recipe |
|---|---|---|
| `.deb` | Docker, Ubuntu 25.10 | `deb/` |
| `.rpm` | Docker, Fedora 43 | `rpm/` |
| `.pkg.tar.zst` | Docker, Arch Linux | `arch/` |
| `.AppImage` | Docker, Arch Linux, with quick-sharun | `appimage/` |

The Docker images are built on the first run and cached; Cargo downloads are kept in `packaging/.cargo-cache`. The deb, rpm and AppImage are built with the update checker on. The Arch package keeps it off, since pacman updates it.

The AppImage bundles every library it needs, the C library included, with [quick-sharun](https://github.com/pkgforge-dev/Anylinux-AppImages), so it runs on older systems too. It is built from the Arch package, so build `arch` before `appimage`.

The Flatpak bundle is built from `build-aux/flatpak/io.github.Replexo.Blick.json`. Keep the update checker off in the Flathub manifest: Flathub doesn't allow apps to update themselves.
