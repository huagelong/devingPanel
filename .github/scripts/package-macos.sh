#!/usr/bin/env bash
set -euo pipefail

arch=$1
case "$arch" in
  x64) expected=x86_64 ;;
  arm64) expected=arm64 ;;
  *) echo "Unsupported architecture: $arch" >&2; exit 1 ;;
esac
test "$(uname -m)" = "$expected"
test "$(lipo -archs target/release/deving-panel)" = "$expected"

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "deving-panel") | .version')
app_name=devingPanel
app="$app_name.app"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/$app/Contents/MacOS" "$work/$app/Contents/Resources" dist
cp target/release/deving-panel "$work/$app/Contents/MacOS/deving-panel"
cp LICENSE "$work/$app/Contents/Resources/LICENSE"
cp .github/packaging/deving-panel.icns "$work/$app/Contents/Resources/deving-panel.icns"
cat > "$work/$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>devingPanel</string>
  <key>CFBundleDisplayName</key><string>devingPanel</string>
  <key>CFBundleIdentifier</key><string>com.devingpanel.desktop</string>
  <key>CFBundleExecutable</key><string>deving-panel</string>
  <key>CFBundleIconFile</key><string>deving-panel.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
plutil -lint "$work/$app/Contents/Info.plist"
codesign --force --deep --sign - "$work/$app"
codesign --verify --deep --strict --verbose=2 "$work/$app"
hdiutil create -volname "$app_name" -srcfolder "$work/$app" \
  -format UDZO -ov "dist/deving-panel_${version}_macos_${arch}.dmg"
