#!/usr/bin/env bash
set -euo pipefail

arch=$1
case "$arch" in
  x64) deb_arch=amd64; image_arch=x86_64 ;;
  arm64) deb_arch=arm64; image_arch=aarch64 ;;
  *) echo "Unsupported architecture: $arch" >&2; exit 1 ;;
esac
test "$(uname -m)" = "$image_arch"

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "deving-panel") | .version')
binary=target/release/deving-panel
icon=.github/packaging/deving-panel.svg
mkdir -p dist

deb_root=$(mktemp -d)
rpm_top=$(mktemp -d)
appdir=$(mktemp -d)/deving-panel.AppDir
tools_dir=$(mktemp -d)
trap 'rm -rf "$deb_root" "$rpm_top" "$(dirname "$appdir")" "$tools_dir"' EXIT

install -Dm755 "$binary" "$deb_root/usr/bin/deving-panel"
install -Dm644 "$icon" "$deb_root/usr/share/icons/hicolor/scalable/apps/deving-panel.svg"
install -Dm644 .github/packaging/deving-panel.desktop "$deb_root/usr/share/applications/deving-panel.desktop"
install -Dm644 LICENSE "$deb_root/usr/share/doc/deving-panel/LICENSE"
mkdir -p "$deb_root/DEBIAN"
cat > "$deb_root/DEBIAN/control" <<EOF
Package: deving-panel
Version: $version
Section: utils
Priority: optional
Architecture: $deb_arch
Maintainer: devingPanel contributors
Depends: libasound2t64, libfontconfig1, libsecret-1-0, libssl3t64, libvulkan1, libwayland-client0, libx11-6, libxkbcommon-x11-0
Description: Desktop client for Docker deployment over SSH
EOF
dpkg-deb --build --root-owner-group "$deb_root" "dist/deving-panel_${version}_${deb_arch}.deb"

rpmbuild -bb --target "$image_arch" \
  --define "_topdir $rpm_top" \
  --define "_sourcedir $deb_root" \
  --define "app_version $version" \
  .github/packaging/deving-panel.spec
install -m644 "$rpm_top/RPMS/$image_arch/deving-panel-$version-1.$image_arch.rpm" \
  "dist/deving-panel_${version}_linux_${arch}.rpm"

curl -fL --retry 3 -o "$tools_dir/linuxdeploy.AppImage" \
  "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${image_arch}.AppImage"
curl -fL --retry 3 -o "$tools_dir/appimagetool.AppImage" \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${image_arch}.AppImage"
chmod +x "$tools_dir"/*.AppImage
APPIMAGE_EXTRACT_AND_RUN=1 "$tools_dir/linuxdeploy.AppImage" \
  --appdir "$appdir" --executable "$binary" \
  --desktop-file .github/packaging/deving-panel.desktop --icon-file "$icon"
APPIMAGE_EXTRACT_AND_RUN=1 ARCH="$image_arch" "$tools_dir/appimagetool.AppImage" \
  "$appdir" "dist/deving-panel_${version}_linux_${arch}.AppImage"
