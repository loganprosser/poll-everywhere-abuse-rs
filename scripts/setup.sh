#!/bin/bash
# One-time setup: Rust (via Homebrew), browser check, AI CLI check, release
# build. `make app` / `make run` run this automatically the first time, so a
# fresh Mac only needs Homebrew installed first.
set -euo pipefail
cd "$(dirname "$0")/.."

say() { printf '\n==> %s\n' "$*"; }

say "checking for Homebrew"
# The Homebrew installer only puts brew on PATH if you ran the lines it
# printed at the end, so check the standard locations too.
BREW="$(command -v brew || ls /opt/homebrew/bin/brew /usr/local/bin/brew 2>/dev/null | head -1 || true)"
if [ -z "$BREW" ]; then
  echo "Homebrew isn't installed. Install it first (paste into Terminal, from https://brew.sh):" >&2
  echo '  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"' >&2
  exit 1
fi
BREW_BIN="$("$BREW" --prefix)/bin"
export PATH="$BREW_BIN:$HOME/.cargo/bin:$PATH"

say "checking for Rust (cargo)"
if ! command -v cargo >/dev/null; then
  "$BREW" install rust
fi
cargo --version

say "checking for a Chrome-family browser"
FOUND=""
for app in "Google Chrome" "Chromium" "Brave Browser" "Microsoft Edge"; do
  for dir in /Applications "$HOME/Applications"; do
    [ -d "$dir/$app.app" ] && FOUND="$dir/$app.app" && break 2
  done
done
if [ -z "$FOUND" ] && ls -d "$HOME"/Library/Caches/ms-playwright/chromium-*/chrome-mac*/*.app >/dev/null 2>&1; then
  FOUND="a Playwright Chromium"
fi
if [ -n "$FOUND" ]; then
  echo "  browser:  found ($FOUND)"
else
  echo "  browser:  none found -- install one: brew install --cask google-chrome"
fi

say "checking AI backends (optional -- any ONE is enough; pick it in Settings)"
have() { command -v "$1" >/dev/null || [ -x "$BREW_BIN/$1" ]; }
if have claude; then
  echo "  claude:   found -- 'Claude' backend available"
else
  echo "  claude:   not found -- to use Claude: brew install --cask claude-code, then run: claude"
fi
if have codex; then
  echo "  codex:    found -- 'Codex' backend available"
else
  echo "  codex:    not found -- to use ChatGPT: brew install --cask codex, then run: codex"
fi

say "building (release, a few minutes the first time)"
cargo build --release

cat <<'EOT'

Setup done.

  make app     build dist/Poll Everywhere Abuse.app, double-clickable
  make run     or launch it right now from source
  make test    offline tests (no network, no AI, nothing sent)

First launch: macOS asks for Messages automation on the first text, and
Full Disk Access only if you want reply-by-iMessage confirmations.
The AI is optional: with "AI auto-answer" off in Settings, the app only
detects questions and notifies you; nothing is ever submitted.
EOT
