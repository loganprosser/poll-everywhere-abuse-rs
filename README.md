# Poll Everywhere Abuse (Rust)

A Rust port of [poll-everywhere-abuse](https://github.com/loganprosser/poll-everywhere-abuse).
It watches a PollEv presenter's page and, when they open a live question,
notifies you (ntfy, macOS notification and sound, iMessage) and saves the
question as structured JSON. With AI on, it also reasons about and submits
answers.

Same window, same settings file, same features as the Python app, but it
answers a live question in about a tenth of a second and the app is 8.6 MB
instead of 179 MB. [Numbers below](#speed-vs-the-python-version).

## Install

### Apple Silicon Mac (M1 or newer): one line

Nothing needs to be installed first: no Homebrew, no Xcode, no Rust. Open
Terminal (Cmd-Space, type "Terminal") and paste:

```bash
curl -fsSL https://raw.githubusercontent.com/loganprosser/poll-everywhere-abuse-rs/main/install.sh | bash
```

It downloads the latest app into Applications, installs Google Chrome if you
have no Chrome-family browser, installs the Claude command-line tool if it's
missing, and opens the app. Then sign in to Claude once: run the `claude`
command it prints, log in, and type `/exit`. AI answering needs a Claude
subscription; without one the app still watches and notifies you. To update
later, paste the same line again: your settings and presets are kept.

### Or: the DMG

Download `Poll-Everywhere-Abuse.dmg` from the
[latest release](https://github.com/loganprosser/poll-everywhere-abuse-rs/releases/latest),
open it, and drag the app into Applications. The app isn't notarized by Apple
(that needs a paid developer account), so the **first** open is blocked:
click Done, then go to System Settings > Privacy & Security, scroll down, and
click **Open Anyway** next to "Poll Everywhere Abuse". After that it opens
normally. The DMG doesn't install Chrome or Claude for you; the one-liner
above does.

### Intel Mac, or build from source

You need macOS 13+. No Apple Developer account, no signing. The build is for
the Mac it runs on, so an Intel Mac builds its own (an app built on an M-series
Mac won't open on Intel).

1. **Install Homebrew**: open Terminal and paste the one-line command from
   [brew.sh](https://brew.sh). It also installs git and make. If it prints
   "Next steps", run those lines too.
2. **Download and build**: paste this into Terminal:

   ```bash
   git clone https://github.com/loganprosser/poll-everywhere-abuse-rs.git
   cd poll-everywhere-abuse-rs
   make app
   ```

   This installs Rust through Homebrew if needed, checks for a Chrome-family
   browser (it tells you to run `brew install --cask google-chrome` if there
   is none), then builds `dist/Poll Everywhere Abuse.app`. The first build
   takes a few minutes. An app built on your own Mac opens normally; no
   right-click-to-open dance.
3. **Open it**: `open "dist/Poll Everywhere Abuse.app"`, or drag it into
   Applications first.
4. **(Optional) AI answering** needs one AI command-line tool, logged in once:
   - Claude: `brew install --cask claude-code`, then run `claude` and log in
   - ChatGPT: `brew install --cask codex`, then run `codex` and log in

Other commands: `make run` launches from source without building the .app,
`make test` runs the offline tests (no network, no AI, nothing sent), and
`make help` lists everything. To update later: `git pull && make app`.

## How it all works

The app is a small window for settings plus a background watcher. When you
press **Start**:

1. **It opens its own Chrome window** on your poll link. This Chrome uses its
   own separate profile (not your normal Chrome), so a class login you finish
   there once is remembered next time.
2. **It answers PollEv's location request with your fake location**, never
   your real one (or denies it, if you turn that off).
3. **It checks the page about 7 times a second** for a live question, by
   reading the poll data PollEv embeds in the page.
4. **When a question opens**, it notifies you (Mac notification and sound,
   ntfy, iMessage: whichever you turned on) and saves the question to disk.
5. **With AI on**, it asks your AI (Claude, ChatGPT through Codex, or NRP)
   for an answer, then **waits** a random 14 to 20 seconds from when the
   question appeared, so it's never the first vote in. It goes sooner if it
   sees someone else answer first.
6. **It clicks the answer** in the poll, tells you what it picked, and goes
   back to watching for the next question.

Press **Stop** (or quit the app) and the Chrome window closes. Details on
each step are further down: [login](#if-the-poll-requires-login),
[location](#geolocation), [detection](#how-detection-works),
[answering](#how-answering-works), [AI backends](#ai-backends).

## Using it

1. **Set up a preset per class**: a preset is a poll link + screen name +
   fake location, saved together so switching classes is one click.
   - Paste the class's `pollev.com/...` link in **Poll link** and type the
     screen name you want to appear as.
   - Set the fake location on the **Location** page: click the map, or search
     a place or type `lat,lon`. The Watch page shows the current location
     under **Location**.
   - Click **Save as preset**. It appears as a button under **Presets**.

   Click a preset to switch to its link, screen name and location all at
   once; its × removes it. A preset lights up while the form matches it
   exactly, so if one goes dark you've changed something since saving: click
   **Save as preset** again to update it (presets are matched by link). A new
   link you press Start with is saved as a preset automatically, but an
   existing preset only changes when you click Save. Presets live in your own
   settings file, not in this repo, so pulling or reinstalling keeps them and
   nobody else sees them.
2. **Notifications** (Settings): turn on whichever you want. macOS and Sound
   work instantly. ntfy needs a topic (any unique word: install the ntfy app on
   your phone and subscribe to the same word; anyone who knows the word can
   read it, so pick something unguessable). iMessage needs your own phone
   number or iMessage email: it texts you, and if you reply, that's your
   confirmation. Each has a **Test** button: click it and check that you got
   it.
3. **AI answering** (Settings): three backends:
   - **Claude** (your own `claude` login)
   - **Codex** (your own ChatGPT login through `codex`)
   - **NRP gateway** (Cornell/Nautilus research models; needs a separate
     `nrp-claude` launcher that isn't part of this repo)

   Pick one, pick a model, click **Test model** (one real call, so you know it
   works before class). **Not sure? Uncheck "AI auto-answer"**: the app still
   watches and notifies you of every question, but never submits anything.
   That's a good way to try it during a real class.
4. **Start**: click **Start**. A Chrome window opens (keep it open; if the
   class requires login, you finish it there once and it's remembered).
   Status shows **Watching**. Every question now notifies you; with AI on and
   confirmation off, it answers them.

**What macOS will ask you (one time each, per Mac):**
- An **Automation** prompt for Messages: click OK (first iMessage only).
- **Full Disk Access**: only needed for reply-by-text confirmations (System
  Settings > Privacy & Security > Full Disk Access > add the app). Skip it if
  you confirm in the app instead.

**Where your settings live**: `~/Library/Application Support/pollevbot/`.
This is the same file the Python app uses, so your settings carry over in both
directions. Chrome's profile (cookies, so a login is remembered) is in
`chrome-profile/` there, and question records are in `pollev_logs/`.

**If something looks broken**: the Activity panel in the app logs every step
in plain text, and `pollev_logs/ai_answer.log` keeps the same log on disk. If
the map stays blank, check your network: the map tiles load from the internet
(they're cached after that).

## Speed vs the Python version

Both versions ran against the same fake poll page on one Mac, with a stand-in
AI that answers instantly (the AI's own thinking time is the same for both
and left out). Medians of 3 to 8 runs, re-run independently.

![Python vs Rust. Start to first answer: 10.5 s vs 0.47 s (~22x faster). Question to vote clicked: 0.33 s vs 0.10 s (~3x). Start to Watching: 10.0 s vs 5.5 s. Stop after a vote: 0.36 s vs 0.23 s. App size: 179 MB vs 8.6 MB (~21x smaller). Fresh build: 53 s vs ~24 s. Worse in Rust: stop while idle 0.08 s vs 0.20 s, idle CPU 2.4% vs 5.8% of a core, memory 515 MB vs 868 MB.](docs/speedup.png)

Most of the gain is design, not the language:

- **No fixed startup waits.** The Python app slept for the cookie banner, the
  login wall and the screen-name prompt on every start. Here the page setup
  runs every loop and reacts to whatever is on screen, so a question that is
  already live gets answered right after the page loads. (On an idle page the
  status still waits 5 s before saying "Watching", so a login wall that renders
  late isn't mistaken for a mid-session logout. Detection runs the whole time.)
- **150 ms detection** (was 400 ms), and a 200 ms post-submit wait (was 500 ms).
- **Chrome flags that stop background throttling**, so detection stays fast
  with the window hidden. The cost: checking that often keeps Chrome busier,
  so idle CPU is higher than the Python app's.
- **Drives your installed Chrome directly** over the DevTools protocol: no
  Python, no Playwright, no bundled browser.

Other changes from the Python version:

- **Exact option matching first.** Answer "2" no longer clicks the option
  "12"; substring matching is only a fallback.
- **A persistent Chrome profile** instead of a saved session file. Logins and
  cookies survive restarts, and Chrome is closed gracefully so they're saved.
  Don't run two copies of the app at once: they'd share that profile.
- **Stop is quicker**: it kills a running AI call right away instead of
  waiting for it, and quitting (even with Cmd-Q) closes Chrome.

## If the poll requires login

Some classes require a registered, logged-in participant instead of the
anonymous screen-name flow (PollEv shows a "Pre-registration required" wall).
When the app sees that wall, it clicks the poll's own **Log in** button and
walks the identifier-only screens: it enters `<screen name>@cornell.edu`,
picks "Log in with Cornell", and fills your NetID (the screen name box doubles
as your NetID). **It never types a password and never touches Duo.** At the
password screen it stops and waits for you to finish the login yourself in
the Chrome window, then resumes automatically once the wall is gone. The
identifier steps are Cornell-specific; at another school, just log in
yourself when the window shows the login page.

The login lasts across runs: it lives in the app's own Chrome profile, so
you're only asked again when the session really expires on the server.

If the wall comes back mid-session (you got logged out), you get one
notification, it tries the same login steps again, and the app shows when
the wall has cleared.

## Geolocation

PollEv can ask your browser for its real location even when the poll doesn't
need it, and there's no reason to hand that over to answer a question. By
default the app grants the request with a fixed fake position instead, using
Chrome's own DevTools geolocation override (the same mechanism as the
DevTools "Sensors" panel, not a JavaScript injection). Set the position on the
app's Location page (it starts at 0, 0 until you pick one). Turn the setting
off to deny the location permission outright, so PollEv gets nothing at all.
There is deliberately no "use my real location" mode.

Both paths are covered by tests: a page calling the real
`navigator.geolocation.getCurrentPosition()` gets back exactly the configured
coordinates, or a permission error when denied.

(Out of scope: this does not fake location to beat a poll that restricts
responses by geography. Faking where you are to pass a check designed to
confirm it is not something this project does.)

## How detection works

PollEv's participant page embeds the live poll's full data as JSON
(`activity: {...}`), and that object is absent when no question is live. The
app reads that JSON instead of scraping the rendered page:

| PollEv `type`          | `max_votes`   | our `type`              |
|------------------------|---------------|-------------------------|
| `multiple_choice_poll` | `1`           | `multiple_choice`       |
| `multiple_choice_poll` | anything else | `select_multiple`       |
| `free_text_poll`       | (n/a)         | `short_answer`          |
| `clickable_image`      | `1`           | `clickable_image`       |
| `clickable_image`      | anything else | `clickable_image_multi` |

`max_votes` is PollEv's per-participant cap: `1` means one pick, anything else
allows several. (`responses_per_option` sounds related but is an audience-wide
cap on one option's total votes, not a multi-select switch.)

For clickable images, the AI reads the downloaded image and returns click
positions as fractions of its width and height; iMessage gets a copy of the
image with the chosen spots marked. Any other type (word cloud, survey,
competition, ...) is saved as `"unknown"` with PollEv's raw JSON attached and
sent to you as a notification, but the AI is never asked to answer it.

## Output

Each detected question is saved to `pollev_logs/questions/poll_<timestamp>.json`
(same format as the Python app):

```json
{
  "question_id": 91775683,
  "poll_key": "oGmbbwD4t9aWM9DXmheDy",
  "timestamp": "20260825_190203",
  "url": "https://pollev.com/yourclass",
  "type": "select_multiple",
  "question": "Which fruits do you like? (pick any)",
  "options": [
    {"id": 186205036, "text": "Apple"},
    {"id": 186205037, "text": "Banana"}
  ],
  "image_url": null,
  "answer": null,
  "source": "dom"
}
```

`question_id` and `poll_key` are PollEv's own stable IDs for the question, and
each option keeps its real PollEv option ID, so an answer can always be tied
to exactly which option it was. `pollev_logs/questions/index.jsonl` gets one
line per question (timestamp, id, type, question, filename): a timeline of a
whole session without opening every file.

## How answering works

For each new question (each question ID is answered once, even if it stays
live):

1. It saves the JSON above and notifies you: "we have a question".
2. It strips known prompt-injection phrasing ("ignore all previous
   instructions", "you are now", "jailbreak", ...) out of the question and
   options. If it found any, it tells you and **forces confirmation** for that
   question, whatever your settings say.
3. It asks the model, framing the question as untrusted data. The CLI itself
   enforces the reply shape: `{"answer": ..., "confidence": 0-100, "reasoning": "..."}`.
4. It notifies you with the question, the answer and the confidence, then:
   - **Multiple choice, select multiple, clickable image**: submits on its own,
     or, if confirmation is on, waits for you to click **Accept** in the app or
     reply to the text.
   - **Short answer**: always waits for you. Reply "lgtm" (or click Accept) to
     submit it as-is, or reply with your own answer, or type one in the app, to
     submit that instead. The AI is told to keep these to 1 to 4 words with no
     hyphens or dashes.
5. It submits and notifies you of the outcome, then goes back to watching.
   It never votes the instant a poll opens: it waits a random 13.8 to 20.4
   seconds (0.23 to 0.34 min) from when the poll appeared, or less if the
   response count goes up first. The page only refreshes that count now and
   then, so usually the timer decides. Change the range with
   `answer_delay_secs` in `app_settings.json`; `[0, 0]` turns it off.

**Confirmations: in the app or by text, first one wins.** Clicking Accept after
you already replied by text does nothing, and the other way around, so an
answer can never be submitted twice. Pressing Stop while a confirmation is
pending counts as "no".

**Answer shapes** (what the model returns, and what `poll::submit_answer` takes):

| type                                       | answer                                          |
|--------------------------------------------|-------------------------------------------------|
| `multiple_choice`                          | one option id (number) or exact option text     |
| `select_multiple`                          | a list of option ids or texts; each gets clicked |
| `short_answer`                             | a non-empty string                              |
| `clickable_image`, `clickable_image_multi` | a list of `{"x": .., "y": ..}` fractions, 0 to 1 |

A click on an option *is* the vote (no separate submit step); short answers
fill the real text box and click Submit. Submitting fails safely with
`SubmitError::Stale` if the presenter moved on since the question was read, or
`SubmitError::Answer` if the answer doesn't match a real current option. To
change an answer, submit again while the question is live: if the plain click
doesn't take, it uses PollEv's own undo button and clicks again. That only
works when the presenter allows changing answers, and a second short answer
adds another response rather than replacing the first.

**Why a prompt injection can't make it submit garbage**: even if a question
talks the model into answering "I HAVE BEEN HACKED", the submit step only
accepts one of the poll's real current options. A filtered option no longer
matches its original text either, so an injected option fails closed. The
phrase filter and the schema are earlier layers; the option match is the
backstop.

## AI backends

| Backend | Runs | Models |
|---------|------|--------|
| Claude | `claude -p --json-schema ...` with your normal login | fable, opus, sonnet, haiku, opusplan |
| Codex (ChatGPT) | `codex exec` with a strict output schema, answer read from `-o <file>` | gpt-6-sol (the only one a plain ChatGPT account allows) |
| NRP gateway | the `nrp-claude` launcher (not included; set `NRP_CLAUDE=/path/to/nrp-claude` if it isn't in the default spot) | NRP's live model list |

Effort (low to max) is optional; not every model supports it. Each call has a
300 s limit, runs with stdin closed, and runs in its own process group, so
Stop (or the timeout) kills it and anything it started at once.

## Build the .app

```bash
make app          # -> dist/Poll Everywhere Abuse.app (about 8.6 MB)
```

Built locally and ad-hoc signed on purpose: an app built on the Mac that runs
it carries no quarantine flag, so Gatekeeper never complains. No Apple
Developer account, no notarization.

To publish a new version for the one-line installer and the DMG, commit and
push, then run `make release` (needs the `gh` command, logged in). It builds
the app, packages a zip and a DMG, and creates a GitHub release; the
installer always downloads the latest one. Releases built on an M-series Mac
are Apple Silicon only.

What the bundle does *not* include:
- **A browser.** It uses your installed Chrome-family browser (Google Chrome,
  Chromium, Brave or Edge). The one-line installer installs Google Chrome if
  there is none; otherwise the app offers to run
  `brew install --cask google-chrome` for you.
- **Your AI login.** Each user brings their own `claude` or `codex` login (or
  NRP launcher); nothing is bundled.

## What's been tested

Offline, with `make test` (97 tests, nothing sent anywhere): every question
type end to end in real Chrome against local fake PollEv pages, including:
- exact option matching ("2" never clicks "12"), select multiple, short answer
  accept and skip, confirm accept and skip, clickable images
- the cookie banner and screen-name steps, a login wall before the poll, a wall
  that renders late, a mid-session logout that notifies once and clears, and a
  question whose text contains wall-like wording
- Stop while a confirmation is pending (counts as "no"), Stop during the
  startup model test, Stop during an auto-submit notification (nothing is
  submitted), the same question ID answered only once
- Chrome killed mid-run (reports a crash cleanly), a crash sending a
  notification, and quitting never leaving Chrome running

The built app also passes a self-test of the window-to-Rust bridge
(`POLLEV_SELFTEST=1`), and it reads an existing Python-app settings file
unchanged.

**Not yet verified with this port**: a live pollev.com class, real AI calls
end to end, real iMessage and ntfy delivery, a real Cornell login, and
drag-and-drop of the poll link. The Python app was verified live on all of
those, and this port follows the same logic, but treat the first real class
as a test: leave "AI auto-answer" off or confirmation on.

## Not ported

Snapshot and websocket debug logging, the MCP server, the OpenAI classifier
for unknown question types, the CLI scripts, and the model-switching helper.
The GUI app is the product.

## Code layout

| File | What it does |
|------|--------------|
| `src/cdp.rs` | Launches Chrome and talks to it over the DevTools protocol |
| `src/poll.rs` | Reads the live question, submits answers, handles the cookie/login/screen-name steps |
| `src/ai.rs` | Prompts, injection filter, response parsing, and the three AI backends |
| `src/notify.rs` | macOS notifications and sound, ntfy, iMessage, and waiting for a reply |
| `src/watcher.rs` | The watch loop and the start/stop service the window talks to |
| `src/gui.rs` | The window: the Python app's page (`ui/index.html`), unchanged, wired to Rust |
| `src/config.rs` | Settings, in the same file and format as the Python app |

`PORT_SPEC.md` is the module-by-module spec the port was built from, including
every deliberate change from the Python app.

## License

MIT. Python original: https://github.com/loganprosser/poll-everywhere-abuse
