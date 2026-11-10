#!/usr/bin/env bash
# Build a .deb from the release binaries in src-tauri/target/release.
#
#   packaging/deb/build.sh <version> [arch] [outdir]
#
# Run it inside an Ubuntu/Debian container (or any host with dpkg-deb) after
# `cargo build --release --bins`; CI does exactly that.
set -euo pipefail

version="${1:?usage: build.sh <version> [arch] [outdir]}"
arch="${2:-amd64}"
outdir="${3:-dist}"

root="$(cd "$(dirname "$0")/../.." && pwd)"
bindir="$root/src-tauri/target/release"
stage="$outdir/bluetooth-input-bridge_${version}_${arch}"

for binary in bluetooth-input-desktop bluetooth-input-bridge-service; do
  if [ ! -x "$bindir/$binary" ]; then
    echo "missing $bindir/$binary; run 'cargo build --release --bins' first" >&2
    exit 1
  fi
done

rm -rf "$stage"
mkdir -p "$stage/DEBIAN" \
         "$stage/usr/bin" \
         "$stage/usr/share/applications" \
         "$stage/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "$bindir/bluetooth-input-desktop" "$stage/usr/bin/"
install -m 0755 "$bindir/bluetooth-input-bridge-service" "$stage/usr/bin/"

# Debian 打包惯例是去掉符号表；strip 不存在时保持原样。
if command -v strip >/dev/null 2>&1; then
  strip --strip-unneeded "$stage/usr/bin/"*
fi
install -m 0644 "$root/packaging/common/bluetooth-input-bridge.desktop" "$stage/usr/share/applications/"
install -m 0644 "$root/packaging/common/bluetooth-input-bridge.svg" \
  "$stage/usr/share/icons/hicolor/scalable/apps/"

installed_size=$(du -sk "$stage/usr" | cut -f1)
cat > "$stage/DEBIAN/control" <<CONTROL
Package: bluetooth-input-bridge
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: chao <chao@users.noreply.github.com>
Installed-Size: $installed_size
Depends: libwebkit2gtk-4.1-0, libgtk-3-0, libsoup-3.0-0, librsvg2-2, libevdev2, libssl3 | libssl3t64
Homepage: https://github.com/chaoes/BlueBridgeIme
Description: Type on your phone, get the text on this computer over Bluetooth
 BlueBridgeIme sends text typed on an Android phone over classic Bluetooth
 RFCOMM and injects it into the focused window on a Linux desktop. The UI is
 a thin Tauri shell; a headless Rust service owns the Bluetooth side.
CONTROL

mkdir -p "$outdir"
dpkg-deb --root-owner-group --build "$stage" "$outdir/bluetooth-input-bridge_${version}_${arch}.deb"
echo "built $outdir/bluetooth-input-bridge_${version}_${arch}.deb"
