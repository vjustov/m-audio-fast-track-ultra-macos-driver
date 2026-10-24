#!/bin/bash
# Builds the control panel and wraps it in an app bundle: panel/build/Fast Track Ultra.app
set -euo pipefail
cd "$(dirname "$0")"
swift build -c release
APP="build/Fast Track Ultra.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp .build/release/FastTrackUltraPanel "$APP/Contents/MacOS/"
# Icon source: icon/make_icon.swift draws icon/icon-1024.png; AppIcon.icns is built from it.
cp icon/AppIcon.icns "$APP/Contents/Resources/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>FastTrackUltraPanel</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIdentifier</key><string>local.ftu.panel</string>
  <key>CFBundleName</key><string>Fast Track Ultra</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
codesign -s - --force "$APP"
echo "Built $APP"
