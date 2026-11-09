# Rust port spec

Port of the Python app at `/Users/logan/Documents/dev/cornell/fall2026/pollevapp`
(public repo `loganprosser/poll-everywhere-abuse`). The Python source is the
behavior reference: read the named functions before porting them, and keep
the intent of every "confirmed live" comment (those encode real PollEv quirks).

The public signatures in `src/*.rs` are the contract between modules. Keep
them. Add private helpers freely. If a contract is truly wrong, say so in your
report instead of changing another module.

## Global rules

- Errors: `anyhow::Result`. No `unwrap`/`expect`/`panic!` on runtime paths
  (fine in tests). A bad page, dead browser or failed notification must never
  crash the app.
- Dependencies: only what is in Cargo.toml (wry, tao, serde, serde_json,
  anyhow, regex, chrono, base64, tungstenite, ureq 3, rusqlite). Read crate
  source under `~/.cargo/registry/src/` when unsure of an API; do not guess.
- Comments: explain why, not what, like the Python code. Keep them short.
- Tests: unit tests in the module (`#[cfg(test)]`). Tests that need a browser
  call `cdp::find_chrome()` and return early with an `eprintln!` if it is
  None. Tests NEVER contact pollev.com, never run a real AI CLI, never send a
  notification/iMessage/ntfy, and never use the real App Support directory
  (use `std::env::temp_dir()` subfolders).
- Timestamps: `chrono::Local`, format `%Y%m%d_%H%M%S`.

## Deliberate changes from the Python app

1. **No fixed grace timers.** Python waited 4s (cookie banner) + 5s (SSO wall)
   + up to 9s (screen name) on every start. Here `PageSetup::tick` runs every
   loop iteration and acts on whatever is on screen. Startup = time for the
   page to load.
2. **Faster detection:** loop interval 150ms (was 400ms).
3. **Persistent Chrome profile** (`config::chrome_profile_dir()`) instead of
   Playwright's storage_state file. Close Chrome gracefully (CDP
   `Browser.close`, then wait up to 5s, then kill) so cookies flush.
4. **Exact-text option matching first** (Python's substring `has_text`
   clicked "12" for answer "2"); substring only as a fallback.
5. **Stop denies a pending confirmation** (Python's bug resolved it truthy).
6. **AI child processes are killed when stop is requested**, so Stop is quick
   even mid-call.
7. **Post-submit wait 200ms** (was 500ms).
8. Chrome flags that stop background throttling, so detection stays fast when
   the window is hidden.

## Skipped (not ported)

Snapshot/websocket debug logging (textwatcher's SnapshotManager and ws
frames), the MCP server, the OpenAI classifier for unknown types, the CLI
scripts, and switch-nrp-model. The GUI app is the product.

## cdp

- `find_chrome`: first existing of `Google Chrome`, `Chromium`,
  `Brave Browser`, `Microsoft Edge` `.app` in `/Applications` then
  `~/Applications`, then Playwright's cache
  (`~/Library/Caches/ms-playwright/chromium-*/chrome-mac*/*.app`, newest
  first). Return the executable inside: `X.app/Contents/MacOS/X`.
- `launch`: free port by binding `127.0.0.1:0` and dropping it. Args:
  `--remote-debugging-port=PORT --user-data-dir=PROFILE --no-first-run
  --no-default-browser-check --use-mock-keychain
  --disable-background-timer-throttling --disable-backgrounding-occluded-windows
  --disable-renderer-backgrounding about:blank`, plus `--headless=new` when
  headless. stdout/stderr to null. Poll `http://127.0.0.1:PORT/json/version`
  (ureq) until it answers (20s timeout, and fail early if the child exits:
  a profile already in use by another Chrome is the common cause; say so).
  Connect a websocket to the browser endpoint (`webSocketDebuggerUrl`) and to
  the first `"type": "page"` target from `/json/list`.
- Message loop: send `{"id","method","params"}`, read until the reply with
  that id, ignore events. `error` -> Err(message). Set a read timeout on the
  TCP stream (30s) so a hung Chrome cannot block forever.
- `eval`: `Runtime.evaluate {expression, returnByValue: true, awaitPromise: true}`;
  `exceptionDetails` -> Err with its text/description.
- `navigate`: `Page.navigate`, then wait for readyState complete, tolerating
  "context destroyed" errors mid-navigation.
- `set_content`: `Page.getFrameTree` for the main frame id, then
  `Page.setDocumentContent`.
- `click_at`: `Input.dispatchMouseEvent` mouseMoved, mousePressed,
  mouseReleased (button "left", clickCount 1).
- `click_element`/`click_element_at`: one eval per attempt that resolves the
  finder, checks `isConnected`, non-zero rect, visible (computed style), and
  not `disabled`, calls `scrollIntoView({block: "center", inline: "center"})`,
  and returns the target point; then `click_at`. Retry every 50ms until the
  timeout. This must ride out PollEv's ~300-500ms "every option disabled
  while a vote is in flight" window, like Playwright did.
- `insert_text`: `Input.insertText`. `press_enter`: `Input.dispatchKeyEvent`
  keyDown + keyUp with key/code "Enter", windowsVirtualKeyCode 13, text "\r".
- `set_geolocation`: Some -> browser `Browser.grantPermissions
  {permissions: ["geolocation"]}` and page `Emulation.setGeolocationOverride
  {latitude, longitude, accuracy}`; None -> browser `Browser.setPermission
  {permission: {name: "geolocation"}, setting: "denied"}`.
- `screenshot_png`: `Page.captureScreenshot {format: "png", clip: {x, y,
  width, height, scale: 1}, captureBeyondViewport: true}`, base64-decode.
- `new_page`: browser `Target.createTarget {url: "about:blank"}`, find that
  target's websocket URL in `/json/list`, connect. `Page::close`: `Page.close`.
- Tests: headless launch with a temp profile, `eval("1+1")`, set_content a
  page with a button whose onclick records a value, `click_element` it and
  read the value back; a button that starts disabled and enables after 300ms
  must still get clicked; geolocation override seen by
  `navigator.geolocation.getCurrentPosition`.

## poll

Port from `app/textwatcher.py`: `extract_first_json_object`,
`extract_activity_json`, `extract_poll_dom` (-> `classify`),
`build_question_record`, `get_current_question`, `_resolve_option`,
`_click_option`, `_click_image_point`, `submit_answer`, `download_image`,
`render_marked_image`, `save_question_json`, `SSO_LOGIN_WALL_PATTERN`.

- `get_current_question`: ONE eval per call returning null when the
  serialized document (`document.documentElement.outerHTML`) has no
  `activity: {` marker, else `{html: <slice from the marker, up to 500k
  chars>, url: location.href, text: document.body.innerText}`. Parse with
  `extract_activity_json`, `classify`, build the record (question falls back
  to the first 300 chars of the body text, one-lined, like `short_preview`).
- Option finder JS: buttons `button.component-response-multiple-choice__option__vote`;
  prefer the one whose `innerText.trim()` equals the option text exactly,
  else the first whose text contains it case-insensitively. Embed strings
  with `serde_json::to_string` (never string-concatenate raw text into JS).
- Undo fallback exactly as Python: on a click that still fails after 5s,
  if a visible `button.component-response-multiple-choice__option__undo`
  (or `button.component-response-clickable-image__undo`) exists, click it,
  wait 400ms, retry once (5s); keep Python's error messages.
- short_answer: focus `textarea[name='response']`, select its contents,
  `insert_text`, click `input[type='submit']`.
- clickable image: `img.component-response-clickable-image__image`,
  `click_element_at` with the fractions.
- Stale check first (re-read the live question, compare `question_id`), 300ms
  between multi clicks, 200ms after the last.
- `PageSetup::tick`: ONE eval returns a snapshot (url, body text, and
  visibility/emptiness of: password input, email input `input#email,
  input[type=email]`, the "Log in with Cornell" button, `input#user_identifier`,
  the wall's `a.mdc-button` whose text is exactly "Log in", a cookie button
  (button/[role=button]/a, text under 40 chars matching
  `\b(accept|agree|allow)\b`), the first visible text input
  (`input[type=text]`, `input[type=name]`, then any `input` that is not
  hidden/password/checkbox/radio/submit), and whether `activity: {` is
  present). Then at most ONE action per tick, in this order:
  1. Cookie button visible and not done yet -> click it, mark done.
  2. On pollev.com and wall text showing -> `wall = true`; click the wall's
     Log in button (at most once per 5s).
  3. Off pollev.com (SSO pages): password visible -> `awaiting_human`, do
     nothing. Else, never repeating an action on the same URL within 2s:
     empty email input -> fill `{username}@cornell.edu`, click
     `input[type=submit], button[type=submit]`; "Log in with Cornell" button
     -> click; empty `#user_identifier` -> fill username, click the button
     whose text is exactly "Continue" (case-insensitive).
  4. On pollev.com, no wall, no activity, `question_seen` false, name not done
     yet, username non-empty, an empty visible text input -> fill username,
     click a button matching `continue|submit|enter|join|start` or press
     Enter; mark done.
  Fill = focus + select via JS, then `insert_text`. Never type a password.
  Read `advance_sso_identifier_steps`, `wait_for_manual_sso_login`,
  `dismiss_cookie_banner`, `handle_initial_pollev_steps` and
  `sso_flow_reference/cornell_sso_flow.md` in the Python repo first.
- Tests: pure-function tests (brace-in-string JSON, each classify mapping
  incl. max_votes, wall regex positives/negatives), plus browser tests on
  fake pages: MC with options "12" and "2" clicks "2"; select_multiple;
  short answer; clickable image fraction; stale question -> Stale.

## ai

Port from `app/ai_answer.py`: PROMPT_TEMPLATE, ANSWER_SHAPE_BY_TYPE,
build_prompt, CLICK_PROMPT_TEMPLATE, build_click_prompt, CLICK_JSON_SCHEMA,
INJECTION_PHRASE_PATTERNS, filter_injection_attempts, _filtered_question,
parse_ai_response, parse_click_response, resolve_short_answer_reply,
ANSWER_JSON_SCHEMA, _codex_strict_schema, make_codex_ai_call_fn,
make_claude_code_ai_call_fn, test_ai_backend; and `list_models` from
`app/api.py`. Prompt text must be identical.

- Subprocesses: resolve `claude`/`codex` via PATH, then `/opt/homebrew/bin`,
  `/usr/local/bin` (Finder-launched apps get a bare PATH; prepend those dirs
  to the child's PATH too). Run in `cwd`. Kill the child if `stop` becomes
  true or after 300s (poll `try_wait` every 50ms); read stdout/stderr on
  threads so a full pipe cannot deadlock.
- nrp: run the launcher at `config::nrp_script()` with `python3` (it is a
  Python script) using the same args as Python (`--model`, `-p`,
  `--output-format json`, `--json-schema`, `--dangerously-skip-permissions`).
  subscription: `claude -p ...` with ANTHROPIC_API_KEY/AUTH_TOKEN/BASE_URL
  removed from the child env. Parse the JSON envelope even on non-zero exit,
  exactly as Python. codex: strict schema written to a temp file, stdin
  null, `-o <tempfile>`, same args.
- `list_models`: subscription -> fable, opus, sonnet, haiku, opusplan; codex
  -> gpt-6-sol; nrp -> `python3 <launcher> --list-models`, split on
  whitespace (30s timeout; launcher missing -> Err naming the path).
- Tests: prompt contains question/options and the data framing; injection
  phrases caught ("IGNORE ALL PREVIOUS INSTRUCTIONS", "forget your
  programming"), ordinary text ("pick any") untouched; parsers with fences
  and surrounding prose; short-answer resolution; strict schema nested.

## notify

Port from `app/textwatcher.py`: mac_notify, mac_sound, _applescript_escape,
imessage_notify, imessage_send_image, ntfy_notify, notify_all,
SENT_MESSAGE_TEXTS; and `ai_answer._wait_for_imessage_reply`.

- osascript/afplay via `std::process::Command`, never block more than ~15s.
  Escape every interpolated string, including the recipient.
- ntfy: POST `https://ntfy.sh/{topic}` with header `Title`, body
  `message` + `\n` + url when url is non-empty, 10s timeout, errors logged
  to stderr only. Skip when the topic is empty.
- chat.db: open read-only (`?mode=ro` URI or OpenFlags), same queries as
  Python; on open/query failure return Err mentioning Full Disk Access.
- Tests: escaping only (newlines, quotes, backslashes). Never send anything.

## watcher

Port `ai_answer.run` + `answer_with_ai` and `app/watcher_service.py`.

- `run`: emit state "starting"; launch Chrome (headless per opts, profile
  dir), `set_geolocation(opts.geo)`, navigate to poll_url. Loop every
  `poll_interval`: stop set -> break; browser dead -> fatal ("the Chrome
  window was closed"); `PageSetup::tick`; when on pollev.com with no wall,
  emit state "watching" once (and log), then `get_current_question`; a new
  `question_id` -> `answer_question`. Wall after "watching" -> emit
  `{"event":"sso_wall","phase":"mid_session"}` and notify "PollEv: login
  needed again" once per appearance, then `{"phase":"cleared"}` when gone. A
  wall before "watching" -> emit sso_wall phase "startup" once. Tick or
  detection errors are transient (pages navigate); 40 in a row is fatal.
  Fatal -> emit state "crashed" with error, notify "PollEv bot crashed" with
  Python's message, return Err. Stop -> emit state "stopped", Ok.
- `answer_question`: exact port of `answer_with_ai`, same statuses, same
  notification titles/bodies, same events. Click questions: image to
  `logs_dir/images/<question_id>.png`, then after SUBMITTED render the marked
  copy and call `send_image` (best effort). Save every new question with
  `poll::save_question_json`.
- `WatcherService`: port of WatcherService/EventQueue/PendingDecision with
  the first-wins confirm/reply functions (in-app decision vs iMessage reply
  via `notify::wait_for_imessage_reply` on a thread with its own per-question
  stop flag). `stop` resolves a pending confirm with **false** and a pending
  short answer with None. State stays in sync with run's state events.
  `start` builds AiFns with `ai::make_ai_fn` (cwd = logs_dir, created) for
  the text schema and the click schema, honors ai_enabled /
  ai_test_on_startup, and a Notifier from cfg; `start_with` is what tests
  use. Join with a deadline by polling `JoinHandle::is_finished`.
- Tests: EventQueue drop-oldest; PendingDecision first wins.

## gui

Port of `app/__main__.py` + `app/api.py`.

- tao window "Poll Everywhere Abuse", 1200x780, min 940x600. wry webview with
  the page: `include_str!("../ui/index.html")` with the
  `<link rel="stylesheet" href="{static}/leaflet.css" />` and
  `<script src="{static}/leaflet.js"></script>` tags replaced by inline
  `<style>`/`<script>` blocks of `ui/leaflet.css`/`ui/leaflet.js`. Not
  incognito (the map tile cache should persist).
- Initialization script defines `window.pywebview.api` as a Proxy: any
  method name -> `(...args) => Promise`, posting `{id, name, args}` via
  `window.ipc.postMessage`. Rust handles each call on its own thread
  (pywebview's model) and resolves via an EventLoopProxy user event that
  runs `window.__ipcResolve(id, ok, value)` on the main thread. Errors
  reject with `new Error(message)`.
- `Api::call` methods, same names, args and return shapes as `app/api.py`:
  get_config, save_config(cfg) (merge only known keys, same bool/float
  coercion, then save), start_watch (validate first), stop_watch, get_state
  (`session_exists` = Chrome profile dir exists), get_events, decide,
  list_models, test_model, resolve_place, approx_current_location,
  test_channel, check_playwright (-> find_chrome: `{ok, path}` or
  `{ok: false, error: "no Chrome-family browser found"}`), install_playwright
  (-> `brew install --cask google-chrome`, brew found at /opt/homebrew/bin or
  /usr/local/bin; `{ok, output}` with the last 2000 chars).
- `POLLEV_SELFTEST=1`: after the page loads, run JS that awaits
  get_config, get_state, get_events and check_playwright through the real
  bridge, posts the results back, prints one JSON line to stdout and exits
  0 (1 on any error). This is how the bridge is tested without clicking.
- Tests: `Api::call` for get_config/save_config against a temp settings path
  (make the path injectable on Api, e.g. a private constructor), unknown
  method -> Err.

## Packaging

- `Makefile`: setup, app, run, test, help (same style as the Python repo;
  app/run/test run setup first when needed via a stamp file).
- `scripts/setup.sh`: Homebrew check (same fallbacks as the Python repo),
  `brew install rust` if cargo is missing, check for a Chrome-family browser
  and print `brew install --cask google-chrome` if none, check AI CLIs
  (same hints), then `cargo build --release`.
- `scripts/build-app.sh`: release build, assemble
  `dist/Poll Everywhere Abuse.app` (Info.plist: bundle id
  `io.github.loganprosser.poll-everywhere-abuse`, name, version 0.1.0,
  LSMinimumSystemVersion 13.0, NSHighResolutionCapable), ad-hoc sign with
  `codesign --force --sign -`.
- README.md (install for a fresh Mac in the same 3 steps as the Python repo,
  plus what changed vs Python and why it is faster), MIT LICENSE
  (Copyright (c) 2026 loganprosser), .gitignore (target/, dist/).
