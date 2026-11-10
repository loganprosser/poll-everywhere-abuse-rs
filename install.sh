#!/bin/bash
# One-line install for Apple Silicon Macs. Needs nothing installed first: no
# Homebrew, no Xcode, no Rust. Paste into Terminal:
#
#   curl -fsSL https://raw.githubusercontent.com/loganprosser/poll-everywhere-abuse-rs/main/install.sh | bash
#
# Installs the prebuilt app from the latest GitHub release, Google Chrome if
# no Chrome-family browser is installed, and the Claude CLI if it's missing,
# then opens the app. Run it again any time to update.
set -euo pipefail

REPO="loganprosser/poll-everywhere-abuse-rs"
NAME="Poll Everywhere Abuse"
say() { printf '\n==> %s\n' "$*"; }

if [ "$(uname -m)" != "arm64" ]; then
  echo "This installer is for Apple Silicon Macs (M1 or newer)." >&2
  echo "On an Intel Mac, build it from source: https://github.com/$REPO#intel-mac-or-build-from-source" >&2
  exit 1
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# /Applications when this account can write there (admins can), else ~/Applications.
DEST="${POLLEV_DEST:-/Applications}"
if [ ! -w "$DEST" ]; then DEST="$HOME/Applications"; fi
mkdir -p "$DEST"

say "downloading $NAME"
curl -fL --progress-bar -o "$TMP/app.zip" "https://github.com/$REPO/releases/latest/download/Poll-Everywhere-Abuse.zip"
ditto -x -k "$TMP/app.zip" "$TMP"
osascript -e "quit app \"$NAME\"" >/dev/null 2>&1 || true
rm -rf "$DEST/$NAME.app"
ditto "$TMP/$NAME.app" "$DEST/$NAME.app"
# curl doesn't mark downloads as quarantined, but clear it in case, so the
# first open isn't blocked (the app is ad-hoc signed, not notarized).
xattr -dr com.apple.quarantine "$DEST/$NAME.app" 2>/dev/null || true
echo "  installed: $DEST/$NAME.app"

say "checking for a Chrome-family browser"
BROWSER=""
for dir in /Applications "$HOME/Applications"; do
  for b in "Google Chrome" Chromium "Brave Browser" "Microsoft Edge"; do
    if [ -z "$BROWSER" ] && [ -d "$dir/$b.app" ]; then BROWSER="$dir/$b.app"; fi
  done
done
if [ -n "$BROWSER" ]; then
  echo "  found: $BROWSER"
else
  echo "  none found, installing Google Chrome (about 280 MB)"
  curl -fL --progress-bar -o "$TMP/chrome.dmg" "https://dl.google.com/chrome/mac/universal/stable/GGRO/googlechrome.dmg"
  MNT="$TMP/chrome-mnt"
  hdiutil attach -quiet -nobrowse -readonly -mountpoint "$MNT" "$TMP/chrome.dmg"
  ditto --norsrc --noextattr "$MNT/Google Chrome.app" "$DEST/Google Chrome.app"
  hdiutil detach -quiet "$MNT"
  echo "  installed: $DEST/Google Chrome.app"
fi

say "checking for the Claude CLI (used for AI answering)"
CLAUDE=""
for c in "$(command -v claude || true)" "$HOME/.local/bin/claude" /opt/homebrew/bin/claude /usr/local/bin/claude; do
  if [ -z "$CLAUDE" ] && [ -n "$c" ] && [ -x "$c" ]; then CLAUDE="$c"; fi
done
if [ -n "$CLAUDE" ]; then
  echo "  found: $CLAUDE"
else
  echo "  not found, installing it with Anthropic's installer"
  curl -fsSL https://claude.ai/install.sh | bash
  CLAUDE="$HOME/.local/bin/claude"
fi

say "done, opening $NAME"
open "$DEST/$NAME.app"
cat <<EOF

Next:
  1. AI answering needs a Claude login (a Claude subscription). Sign in once by running:
       $CLAUDE
     then type /exit. Skip this to use the app for notifications only.
  2. In the app: fill in the poll link, screen name and location, click
     "Save as preset", then Start. The README explains the rest:
     https://github.com/$REPO#using-it
EOF
