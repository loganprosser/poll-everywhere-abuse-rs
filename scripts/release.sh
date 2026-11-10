#!/bin/bash
# Build the app and publish it as a GitHub release: a zip (what install.sh
# downloads) and a DMG (drag to Applications). Apple Silicon only, since the
# build is for the Mac that runs this.
#
# Usage: ./scripts/release.sh   (or: make release; needs gh, logged in)
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/opt/homebrew/bin:/usr/local/bin:$HOME/.cargo/bin:$PATH"

if ! git diff --quiet HEAD; then
  echo "commit your changes first: a release should match a pushed commit" >&2
  exit 1
fi
if [ "$(git rev-parse HEAD)" != "$(git rev-parse @{u} 2>/dev/null)" ]; then
  echo "push first: a release should match a pushed commit" >&2
  exit 1
fi

./scripts/build-app.sh

NAME="Poll Everywhere Abuse"
APP="dist/$NAME.app"
ZIP="dist/Poll-Everywhere-Abuse.zip"
DMG="dist/Poll-Everywhere-Abuse.dmg"
rm -f "$ZIP" "$DMG"

echo "==> packaging"
ditto -c -k --keepParent "$APP" "$ZIP"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
ditto "$APP" "$STAGE/$NAME.app"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "$NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG"

TAG="v$(date +%Y.%m.%d-%H%M)"
echo "==> publishing release $TAG"
gh release create "$TAG" "$ZIP" "$DMG" --title "$TAG" \
  --notes "Apple Silicon build of $(git rev-parse --short HEAD). Install with the one-liner in the README, or open the DMG (see the README for the one-time Open Anyway step)."
