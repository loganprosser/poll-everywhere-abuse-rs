# Poll Everywhere Abuse (Rust)

A Rust port of [poll-everywhere-abuse](https://github.com/loganprosser/poll-everywhere-abuse).
It watches a PollEv presenter's page and, when they open a live question,
notifies you (ntfy, macOS notification and sound, iMessage) and saves the
question as structured JSON. With AI on, it also reasons about and submits
answers.

## Install (fresh Mac)

You need a Mac (macOS 13+). No Apple Developer account, no signing.

1. **Install Homebrew**: open Terminal and paste the one-line command from
   [brew.sh](https://brew.sh). It also installs git and make. If it prints
   "Next steps", run those lines too.
2. **Download and build**:

   ```bash
   git clone https://github.com/loganprosser/poll-everywhere-abuse-rs.git
   cd poll-everywhere-abuse-rs
   make app
   ```

   This installs Rust through Homebrew if needed, checks for a Chrome-family
   browser (it tells you to run `brew install --cask google-chrome` if there
   is none), then builds `dist/Poll Everywhere Abuse.app`. The first build
   takes a few minutes. An app built on your own Mac opens normally.
3. **Open it**: `open "dist/Poll Everywhere Abuse.app"`, or drag it into
   Applications first.

**Optional AI answering** needs one AI command-line tool, logged in once:

- Claude: `brew install --cask claude-code`, then run `claude` and log in
- ChatGPT: `brew install --cask codex`, then run `codex` and log in

Other commands: `make run` launches from source, `make test` runs the offline
tests (no network, no AI, nothing sent), `make help` lists everything.
To update later: `git pull && make app`.

## Using it

1. **Poll link**: paste the `pollev.com/...` link and type the screen name you
   want to appear as.
2. **Notifications** (Settings): turn on what you want. Each has a **Test**
   button.
3. **AI answering** (Settings): pick a backend and model and click
   **Test model**. Not sure? Uncheck "AI auto-answer": the app still notifies
   you of every question but never submits anything.
4. **Start**: a Chrome window opens (keep it open). If the class requires
   login, do it there once; the profile is remembered. Status shows
   **Watching**.

macOS may ask once for Messages automation (first iMessage). Full Disk Access
is only needed for reply-by-text confirmations.

## What differs from the Python version, and why it is faster

- **No fixed startup waits.** The Python app slept for the cookie banner, the
  login wall and the screen-name prompt on every start. Here the page setup
  runs every loop and reacts to whatever is on screen, so startup is just the
  page load.
- **150ms detection** (was 400ms).
- **Persistent Chrome profile** instead of a saved storage-state file. Logins
  and cookies survive restarts, and Chrome is closed gracefully so they flush.
  Quit other Chrome windows using that profile before starting.
- **Exact option matching first.** Answer "2" no longer clicks the option
  "12"; substring matching is only a fallback.
- **Quicker submit** (200ms post-submit wait, was 500ms) and Chrome flags that
  stop background throttling, so detection stays fast with the window hidden.
- Stop now denies a pending confirmation, and AI child processes are killed on
  Stop so it responds quickly even mid-call.
- It drives your installed Chrome-family browser directly over the DevTools
  protocol: no Python, no Playwright download.

## Not ported

Snapshot and websocket debug logging, the MCP server, the OpenAI classifier
for unknown question types, the CLI scripts, and the model-switching helper.
The GUI app is the product.

## License

MIT. Python original: https://github.com/loganprosser/poll-everywhere-abuse
