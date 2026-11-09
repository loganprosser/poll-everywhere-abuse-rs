#!/bin/bash
# Build "Poll Everywhere Abuse.app" in dist/, locally. Ad-hoc signed, no
# notarization: a locally-built app has no quarantine attribute, so
# Gatekeeper never complains (friends clone the repo and run this script).
#
# Usage: ./scripts/build-app.sh   (or: make app)
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.cargo/bin:$PATH"

NAME="Poll Everywhere Abuse"
BIN="poll-everywhere-abuse"
APP="dist/$NAME.app"

if ! command -v cargo >/dev/null; then
  echo "cargo not found -- run: make setup" >&2
  exit 1
fi

echo "==> building release binary"
cargo build --release

echo "==> assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/release/$BIN" "$APP/Contents/MacOS/$BIN"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>io.github.loganprosser.poll-everywhere-abuse</string>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleExecutable</key><string>$BIN</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "==> ad-hoc signing"
codesign --force --sign - "$APP"

echo
echo "Done: $APP"
echo "Run it:   open \"$APP\""
echo "Install:  drag it to /Applications (optional -- it runs fine from dist/)"
