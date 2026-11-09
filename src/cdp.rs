//! Minimal Chrome DevTools Protocol client: launch a Chrome-family browser
//! with a persistent profile and drive one page over a websocket with
//! untyped JSON. Replaces Playwright. See PORT_SPEC.md "cdp".
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tungstenite::Message;
use tungstenite::WebSocket;
use tungstenite::client::IntoClientRequest;
use tungstenite::protocol::WebSocketConfig;

/// A hung Chrome must not block a caller forever.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

pub struct LaunchOptions {
    pub headless: bool,
    pub profile_dir: PathBuf,
    /// None = find_chrome().
    pub chrome_path: Option<PathBuf>,
}

/// Executable of the first Chrome-family browser found (see PORT_SPEC.md).
pub fn find_chrome() -> Option<PathBuf> {
    // An unset HOME must not turn "~/Applications" into a path relative to the cwd.
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let have_home = !home.as_os_str().is_empty();
    let names = [
        "Google Chrome",
        "Chromium",
        "Brave Browser",
        "Microsoft Edge",
    ];
    let mut apps: Vec<(PathBuf, String)> = Vec::new();
    let mut app_dirs = vec![PathBuf::from("/Applications")];
    if have_home {
        app_dirs.push(home.join("Applications"));
    }
    for dir in app_dirs {
        for n in names {
            apps.push((dir.join(format!("{n}.app")), n.to_string()));
        }
    }
    for (app, name) in apps {
        let exe = app.join("Contents/MacOS").join(name);
        if exe.is_file() {
            return Some(exe);
        }
    }
    // Playwright's cache: newest chromium-* first (revision numbers sort numerically).
    if !have_home {
        return None;
    }
    let cache = home.join("Library/Caches/ms-playwright");
    let mut dirs: Vec<(u64, PathBuf)> = std::fs::read_dir(cache)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let rev = name.strip_prefix("chromium-")?.parse::<u64>().ok()?;
            Some((rev, e.path()))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, d) in dirs {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut subs: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("chrome-mac"))
            })
            .collect();
        subs.sort();
        for sub in subs.into_iter().rev() {
            let Ok(rd) = std::fs::read_dir(&sub) else {
                continue;
            };
            for e in rd.flatten() {
                let app = e.path();
                if app.extension().is_some_and(|x| x == "app")
                    && let Some(stem) = app.file_stem()
                {
                    let exe = app.join("Contents/MacOS").join(stem);
                    if exe.is_file() {
                        return Some(exe);
                    }
                }
            }
        }
    }
    None
}

/// One websocket plus the request-id counter. The Mutex serializes callers
/// (send then read until our reply), so concurrent threads never steal each
/// other's replies.
struct Conn {
    ws: Mutex<WebSocket<TcpStream>>,
    next_id: AtomicU64,
}

impl Conn {
    fn connect(ws_url: &str) -> Result<Conn> {
        let authority = ws_url
            .strip_prefix("ws://")
            .and_then(|r| r.split('/').next())
            .ok_or_else(|| anyhow!("unexpected websocket url: {ws_url}"))?;
        let stream =
            TcpStream::connect(authority).with_context(|| format!("connecting to {authority}"))?;
        // The timeouts also bound the handshake, which is a blocking read.
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        stream.set_write_timeout(Some(READ_TIMEOUT))?;
        let _ = stream.set_nodelay(true);
        let req = ws_url.into_client_request()?;
        // Local, trusted peer: a big screenshot or page dump must not trip the
        // default 16MiB frame cap (which would kill the connection for good).
        let cfg = WebSocketConfig::default()
            .max_frame_size(None)
            .max_message_size(None);
        let (ws, _) = tungstenite::client::client_with_config(req, stream, Some(cfg))
            .map_err(|e| anyhow!("websocket handshake failed: {e}"))?;
        Ok(Conn {
            ws: Mutex::new(ws),
            next_id: AtomicU64::new(0),
        })
    }

    fn set_read_timeout(&self, d: Duration) {
        let ws = self.ws.lock().unwrap_or_else(|e| e.into_inner());
        let _ = ws.get_ref().set_read_timeout(Some(d));
    }

    fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let msg = json!({"id": id, "method": method, "params": params}).to_string();
        let mut ws = self.ws.lock().unwrap_or_else(|e| e.into_inner());
        ws.send(Message::text(msg))
            .map_err(|e| anyhow!("{method}: send failed: {e}"))?;
        loop {
            let m = match ws.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e))
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    bail!("{method}: timed out waiting for Chrome")
                }
                Err(e) => bail!("{method}: connection lost: {e}"),
            };
            let text = match m {
                Message::Text(t) => t,
                Message::Close(_) => bail!("{method}: connection closed by Chrome"),
                _ => continue,
            };
            let Ok(v) = serde_json::from_str::<Value>(text.as_str()) else {
                continue;
            };
            // Events and stale replies (after an earlier timeout) have another id or none.
            if v.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(err) = v.get("error") {
                let msg = err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                bail!("{method}: {msg}");
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }
}

fn http_agent() -> ureq::Agent {
    // No proxy: this only talks to 127.0.0.1, and an env proxy would break it.
    ureq::Agent::config_builder()
        .proxy(None)
        .timeout_global(Some(Duration::from_secs(3)))
        .build()
        .into()
}

fn http_json(agent: &ureq::Agent, url: &str) -> Result<Value> {
    let text = agent.get(url).call()?.body_mut().read_to_string()?;
    Ok(serde_json::from_str(&text)?)
}

/// Poll /json/list until the target `want` (a specific id, or the first
/// "page" when None) shows up; returns (id, websocket url).
fn find_page_target(
    agent: &ureq::Agent,
    port: u16,
    want: Option<&str>,
) -> Result<(String, String)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(list) = http_json(agent, &format!("http://127.0.0.1:{port}/json/list"))
            && let Some(items) = list.as_array()
        {
            for t in items {
                let id = t.get("id").and_then(Value::as_str).unwrap_or("");
                let is_page = t.get("type").and_then(Value::as_str) == Some("page");
                let matches = match want {
                    Some(w) => id == w,
                    None => is_page,
                };
                if matches && let Some(ws) = t.get("webSocketDebuggerUrl").and_then(Value::as_str) {
                    return Ok((id.to_string(), ws.to_string()));
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("no page target appeared in Chrome");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Pids of Chrome processes we launched and have not yet reaped, so a Quit
/// that can't wait for the watcher thread can still avoid orphaning them.
static LIVE_PIDS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

fn live_pids() -> std::sync::MutexGuard<'static, Vec<u32>> {
    LIVE_PIDS.lock().unwrap_or_else(|e| e.into_inner())
}

fn forget_pid(pid: u32) {
    live_pids().retain(|p| *p != pid);
}

/// SIGKILL every Chrome we launched that is still running. Last resort for
/// app exit: Chrome's helpers die with the browser process and the profile
/// lock it leaves behind names a dead pid, which the next Chrome ignores.
pub fn kill_all_browsers() {
    let pids: Vec<u32> = std::mem::take(&mut *live_pids());
    for pid in pids {
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .stderr(Stdio::null())
            .status();
    }
}

/// A running browser. Dropping it closes Chrome (gracefully first, so the
/// profile flushes cookies, then kill).
pub struct Browser {
    child: Mutex<Child>,
    port: u16,
    conn: Conn,
    page: Page,
}

/// One page target. All methods take &self (the websocket sits behind a
/// Mutex), and Page is Send + Sync.
pub struct Page {
    conn: Conn,
    target_id: String,
}

impl Browser {
    pub fn launch(opts: &LaunchOptions) -> Result<Browser> {
        let exe = opts
            .chrome_path
            .clone()
            .or_else(find_chrome)
            .ok_or_else(|| anyhow!("no Chrome-family browser found"))?;
        std::fs::create_dir_all(&opts.profile_dir)
            .with_context(|| format!("creating profile dir {}", opts.profile_dir.display()))?;
        // Port 0: Chrome picks a free port itself and reports it in the profile's
        // DevToolsActivePort file. Bind-and-drop here raced with other sockets on
        // the machine (Chrome kept running with no debugging port at all).
        let started = std::time::SystemTime::now();
        let mut cmd = Command::new(&exe);
        cmd.arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", opts.profile_dir.display()))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--use-mock-keychain",
                // Keep detection fast when the window is hidden or covered.
                "--disable-background-timer-throttling",
                "--disable-backgrounding-occluded-windows",
                "--disable-renderer-backgrounding",
            ]);
        if opts.headless {
            cmd.arg("--headless=new");
        }
        cmd.arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        live_pids().push(child.id());
        match Self::connect(&mut child, &opts.profile_dir, started) {
            Ok((port, conn, page)) => Ok(Browser {
                child: Mutex::new(child),
                port,
                conn,
                page,
            }),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                forget_pid(child.id());
                Err(e)
            }
        }
    }

    /// Port Chrome wrote to DevToolsActivePort, ignoring a stale file from an
    /// earlier run (it is only rewritten once Chrome is listening).
    fn read_active_port(profile: &Path, started: std::time::SystemTime) -> Option<u16> {
        let file = profile.join("DevToolsActivePort");
        let fresh = std::fs::metadata(&file)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t >= started);
        if !fresh {
            return None;
        }
        std::fs::read_to_string(file)
            .ok()?
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }

    fn connect(
        child: &mut Child,
        profile: &Path,
        started: std::time::SystemTime,
    ) -> Result<(u16, Conn, Page)> {
        let agent = http_agent();
        let deadline = Instant::now() + Duration::from_secs(20);
        let (port, version) = loop {
            if let Some(status) = child.try_wait()? {
                // Chrome hands off to a running instance that owns the profile, then exits.
                bail!(
                    "Chrome exited right after launch ({status}); the profile is probably already in use by another Chrome window"
                );
            }
            if let Some(port) = Self::read_active_port(profile, started)
                && let Ok(v) = http_json(&agent, &format!("http://127.0.0.1:{port}/json/version"))
            {
                break (port, v);
            }
            if Instant::now() >= deadline {
                bail!("Chrome did not start its debugging port within 20s");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let browser_ws = version
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Chrome did not report a browser websocket url"))?;
        let conn = Conn::connect(browser_ws)?;
        let (target_id, page_ws) = find_page_target(&agent, port, None)?;
        let page = Page {
            conn: Conn::connect(&page_ws)?,
            target_id,
        };
        Ok((port, conn, page))
    }

    /// The page Chrome opened at launch.
    pub fn page(&self) -> &Page {
        &self.page
    }

    /// Some((lat, lon, accuracy_m)) = grant geolocation and override the
    /// position; None = deny the permission outright.
    pub fn set_geolocation(&self, geo: Option<(f64, f64, f64)>) -> Result<()> {
        match geo {
            Some((latitude, longitude, accuracy)) => {
                self.conn.call(
                    "Browser.grantPermissions",
                    json!({"permissions": ["geolocation"]}),
                )?;
                self.page.call(
                    "Emulation.setGeolocationOverride",
                    json!({"latitude": latitude, "longitude": longitude, "accuracy": accuracy}),
                )?;
            }
            None => {
                self.conn.call(
                    "Browser.setPermission",
                    json!({"permission": {"name": "geolocation"}, "setting": "denied"}),
                )?;
            }
        }
        Ok(())
    }

    /// A second, throwaway page (rendering the marked click image).
    pub fn new_page(&self) -> Result<Page> {
        let r = self
            .conn
            .call("Target.createTarget", json!({"url": "about:blank"}))?;
        let id = r
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Target.createTarget returned no targetId"))?;
        let (target_id, ws) = find_page_target(&http_agent(), self.port, Some(id))?;
        Ok(Page {
            conn: Conn::connect(&ws)?,
            target_id,
        })
    }

    /// Chrome process still running and the page connection usable.
    pub fn is_alive(&self) -> bool {
        {
            let mut child = self.child.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(child.try_wait(), Ok(None)) {
                return false;
            }
        }
        // On macOS Chrome outlives its last window, so also check our page still exists.
        // Asking the browser connection (not the page's) never waits on a busy page.
        self.conn
            .call("Target.getTargets", json!({}))
            .map(|r| {
                r["targetInfos"].as_array().is_some_and(|ts| {
                    ts.iter().any(|t| {
                        t.get("targetId").and_then(Value::as_str)
                            == Some(self.page.target_id.as_str())
                    })
                })
            })
            .unwrap_or(false)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap_or_else(|e| e.into_inner());
        if matches!(child.try_wait(), Ok(None)) {
            // Chrome may drop the socket before answering; that is success here.
            self.conn.set_read_timeout(Duration::from_secs(5));
            let _ = self.conn.call("Browser.close", json!({}));
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(Duration::from_millis(50));
            }
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
            }
        }
        let _ = child.wait();
        forget_pid(child.id());
    }
}

impl Page {
    /// Raw CDP command; protocol errors become Err.
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.conn.call(method, params)
    }

    /// Runtime.evaluate with returnByValue + awaitPromise. JS exceptions
    /// become Err. Returns the value (Null for undefined).
    pub fn eval(&self, js: &str) -> Result<Value> {
        let r = self.call(
            "Runtime.evaluate",
            json!({"expression": js, "returnByValue": true, "awaitPromise": true}),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex
                .pointer("/exception/description")
                .and_then(Value::as_str)
                // `throw "str"` has no description, only a value.
                .or_else(|| ex.pointer("/exception/value").and_then(Value::as_str))
                .or_else(|| ex.get("text").and_then(Value::as_str))
                .unwrap_or("JavaScript exception");
            bail!("{msg}");
        }
        Ok(r["result"]["value"].clone())
    }

    /// Navigate and wait (up to 30s) for document.readyState == "complete".
    pub fn navigate(&self, url: &str) -> Result<()> {
        let r = self.call("Page.navigate", json!({"url": url}))?;
        if let Some(err) = r.get("errorText").and_then(Value::as_str) {
            bail!("navigation to {url} failed: {err}");
        }
        // "context destroyed" errors while the old document goes away are expected.
        if self.wait_for(
            "document.readyState === 'complete'",
            Duration::from_secs(30),
        )? {
            Ok(())
        } else {
            bail!("timed out loading {url}")
        }
    }

    /// Replace the document with `html` (Page.setDocumentContent).
    pub fn set_content(&self, html: &str) -> Result<()> {
        let tree = self.call("Page.getFrameTree", json!({}))?;
        let frame_id = tree
            .pointer("/frameTree/frame/id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Page.getFrameTree returned no main frame"))?;
        self.call(
            "Page.setDocumentContent",
            json!({"frameId": frame_id, "html": html}),
        )?;
        Ok(())
    }

    pub fn url(&self) -> Result<String> {
        Ok(self
            .eval("location.href")?
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// Real mouse click (Input.dispatchMouseEvent) at viewport CSS pixels.
    pub fn click_at(&self, x: f64, y: f64) -> Result<()> {
        self.call(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y}),
        )?;
        for kind in ["mousePressed", "mouseReleased"] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({"type": kind, "x": x, "y": y, "button": "left", "buttons": if kind == "mousePressed" { 1 } else { 0 }, "clickCount": 1}),
            )?;
        }
        Ok(())
    }

    /// Retry until `finder_js` (a JS expression evaluating to an Element or
    /// null) is attached, visible and not disabled, scroll it to the
    /// viewport center, then click its center with click_at. Err after
    /// `timeout`. This replaces Playwright's auto-waiting click.
    pub fn click_element(&self, finder_js: &str, timeout: Duration) -> Result<()> {
        self.click_element_at(finder_js, 0.5, 0.5, timeout)
    }

    /// Like click_element, but at a fraction (0..1) of the element's box.
    pub fn click_element_at(
        &self,
        finder_js: &str,
        fx: f64,
        fy: f64,
        timeout: Duration,
    ) -> Result<()> {
        // PollEv disables every option for ~300-500ms while a vote is in flight,
        // so "not clickable yet" is retried rather than reported.
        let fx = if fx.is_finite() {
            fx.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let fy = if fy.is_finite() {
            fy.clamp(0.0, 1.0)
        } else {
            0.5
        };
        // Newlines around the finder (and no trailing `;`) so a `//` comment or
        // a statement terminator cannot break the wrapping parens.
        let finder_js = finder_js.trim().trim_end_matches(';');
        let probe = format!(
            r#"(() => {{
  const el = (
{finder_js}
  );
  if (!el || !el.isConnected) return null;
  const cs = getComputedStyle(el);
  if (cs.display === 'none' || cs.visibility === 'hidden') return null;
  if (el.disabled || el.matches(':disabled') || el.getAttribute('aria-disabled') === 'true') return null;
  let r = el.getBoundingClientRect();
  if (r.width === 0 || r.height === 0) return null;
  el.scrollIntoView({{block: 'center', inline: 'center', behavior: 'instant'}});
  r = el.getBoundingClientRect();
  if (r.width === 0 || r.height === 0) return null;
  let x = r.left + r.width * {fx}, y = r.top + r.height * {fy};
  // A target point outside the viewport (big image) would swallow the click.
  // innerWidth/Height: documentElement.clientHeight is the whole page in quirks mode.
  const vw = window.innerWidth, vh = window.innerHeight;
  if (x < 0 || x >= vw || y < 0 || y >= vh) {{
    window.scrollBy({{left: x - vw / 2, top: y - vh / 2, behavior: 'instant'}});
    r = el.getBoundingClientRect();
    x = r.left + r.width * {fx}; y = r.top + r.height * {fy};
  }}
  // Playwright's "receives events" check: something else covering the point
  // (cookie banner, spinner overlay) means not clickable yet.
  if (x >= 0 && x < vw && y >= 0 && y < vh) {{
    const hit = document.elementFromPoint(x, y);
    if (hit && hit !== el && !el.contains(hit)) {{
      let n = el, ok = false;
      while (n && !ok) {{ ok = n === hit; n = n.parentNode || n.host; }}
      if (!ok) return null;
    }}
  }}
  return [x, y];
}})()"#
        );
        let deadline = Instant::now() + timeout;
        let mut last_err = String::from("element not found, not visible or disabled");
        loop {
            match self.eval(&probe) {
                Ok(v) => {
                    if let (Some(x), Some(y)) = (
                        v.get(0).and_then(Value::as_f64),
                        v.get(1).and_then(Value::as_f64),
                    ) {
                        return self.click_at(x, y);
                    }
                }
                Err(e) => last_err = e.to_string(),
            }
            if Instant::now() >= deadline {
                bail!(
                    "click timed out after {}ms: {last_err}",
                    timeout.as_millis()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Type into the focused element (Input.insertText: trusted input events,
    /// so React-controlled fields see it).
    pub fn insert_text(&self, text: &str) -> Result<()> {
        self.call("Input.insertText", json!({"text": text}))?;
        Ok(())
    }

    pub fn press_enter(&self) -> Result<()> {
        let key = |kind: &str, text: Option<&str>| {
            let mut p = json!({"type": kind, "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "nativeVirtualKeyCode": 13});
            if let Some(t) = text {
                p["text"] = json!(t);
            }
            p
        };
        self.call("Input.dispatchKeyEvent", key("keyDown", Some("\r")))?;
        self.call("Input.dispatchKeyEvent", key("keyUp", None))?;
        Ok(())
    }

    /// Poll `js_bool` every 50ms until true (Ok(true)) or `timeout`
    /// (Ok(false)). Eval errors count as false (pages navigate mid-poll).
    pub fn wait_for(&self, js_bool: &str, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.eval(js_bool).is_ok_and(|v| v.as_bool() == Some(true)) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// PNG bytes; clip = (x, y, width, height) in CSS pixels.
    pub fn screenshot_png(&self, clip: Option<(f64, f64, f64, f64)>) -> Result<Vec<u8>> {
        let mut p = json!({"format": "png", "captureBeyondViewport": true});
        if let Some((x, y, width, height)) = clip {
            p["clip"] = json!({"x": x, "y": y, "width": width, "height": height, "scale": 1});
        }
        let r = self.call("Page.captureScreenshot", p)?;
        let data = r
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Page.captureScreenshot returned no data"))?;
        Ok(base64::engine::general_purpose::STANDARD.decode(data)?)
    }

    pub fn close(&self) -> Result<()> {
        self.call("Page.close", json!({}))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    struct Fixture {
        browser: Browser,
        profile: PathBuf,
    }

    fn launch(name: &str) -> Option<Fixture> {
        if find_chrome().is_none() {
            eprintln!("skipping {name}: no Chrome-family browser found");
            return None;
        }
        let profile =
            std::env::temp_dir().join(format!("pollev-cdp-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        let browser = Browser::launch(&LaunchOptions {
            headless: true,
            profile_dir: profile.clone(),
            chrome_path: None,
        })
        .unwrap();
        Some(Fixture { browser, profile })
    }

    impl Fixture {
        /// Quit Chrome gracefully, then delete the temp profile.
        fn finish(self) {
            let Fixture { browser, profile } = self;
            drop(browser);
            let _ = std::fs::remove_dir_all(profile);
        }
    }

    /// Tiny one-shot-per-connection HTTP server: a secure context (127.0.0.1)
    /// for geolocation and a real navigation target.
    fn serve(body: &'static str) -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let mut s = s;
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        port
    }

    #[test]
    fn eval_arithmetic_and_errors() {
        let Some(f) = launch("eval") else { return };
        let page = f.browser.page();
        assert_eq!(page.eval("1+1").unwrap(), json!(2));
        assert_eq!(page.eval("undefined").unwrap(), Value::Null);
        assert_eq!(page.eval("Promise.resolve('x')").unwrap(), json!("x"));
        let err = page
            .eval("throw new Error('boom')")
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom"), "{err}");
        assert!(page.call("No.suchMethod", json!({})).is_err());
        assert!(f.browser.is_alive());
        f.finish();
    }

    #[test]
    fn click_element_records_value() {
        let Some(f) = launch("click") else { return };
        let page = f.browser.page();
        page.set_content(
            "<body style='margin:0'><div style='height:3000px'></div>\
             <button id=b onclick=\"window.v='clicked'\">go</button></body>",
        )
        .unwrap();
        page.click_element("document.getElementById('b')", Duration::from_secs(3))
            .unwrap();
        assert_eq!(page.eval("window.v").unwrap(), json!("clicked"));
        // A finder that never resolves times out with Err.
        assert!(
            page.click_element("null", Duration::from_millis(200))
                .is_err()
        );
        f.finish();
    }

    #[test]
    fn click_waits_for_disabled_button() {
        let Some(f) = launch("disabled") else { return };
        let page = f.browser.page();
        page.set_content("<button id=b disabled onclick=\"window.v='late'\">go</button>")
            .unwrap();
        page.eval("setTimeout(() => { document.getElementById('b').disabled = false; }, 300)")
            .unwrap();
        page.click_element("document.getElementById('b')", Duration::from_secs(5))
            .unwrap();
        assert_eq!(page.eval("window.v").unwrap(), json!("late"));
        f.finish();
    }

    #[test]
    fn click_waits_for_overlay_and_ignores_trailing_comment() {
        let Some(f) = launch("overlay") else { return };
        let page = f.browser.page();
        page.set_content(
            "<body style='margin:0'><button id=b style='margin:20px' onclick=\"window.v='btn'\">go</button>\
             <div id=o style='position:fixed;left:0;top:0;width:100%;height:100%' onclick=\"window.v='overlay'\"></div></body>",
        )
        .unwrap();
        page.eval("setTimeout(() => document.getElementById('o').remove(), 300)")
            .unwrap();
        page.click_element(
            "document.getElementById('b') // trailing comment;",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(page.eval("window.v").unwrap(), json!("btn"));
        f.finish();
    }

    #[test]
    fn click_skips_aria_disabled_and_scrolls_big_target_point_into_view() {
        let Some(f) = launch("aria") else { return };
        let page = f.browser.page();
        page.set_content("<button id=b aria-disabled='true' onclick=\"window.v='x'\">go</button>")
            .unwrap();
        assert!(
            page.click_element("document.getElementById('b')", Duration::from_millis(300))
                .is_err()
        );
        assert_eq!(page.eval("window.v").unwrap(), Value::Null);
        // Taller than the viewport: the 90% point is off screen after centering.
        page.set_content(
            "<body style='margin:0'><div id=t style='height:4000px' \
             onclick=\"window.v=Math.round(event.offsetY)\"></div></body>",
        )
        .unwrap();
        page.click_element_at(
            "document.getElementById('t')",
            0.5,
            0.9,
            Duration::from_secs(3),
        )
        .unwrap();
        let v = page.eval("window.v").unwrap().as_f64().unwrap();
        assert!((v - 3600.0).abs() <= 2.0, "{v}");
        f.finish();
    }

    #[test]
    fn eval_reports_thrown_string() {
        let Some(f) = launch("thrown") else { return };
        let err = f.browser.page().eval("throw 'plain string'").unwrap_err();
        assert!(err.to_string().contains("plain string"), "{err}");
        f.finish();
    }

    #[test]
    fn typing_and_enter() {
        let Some(f) = launch("typing") else { return };
        let page = f.browser.page();
        page.set_content(
            "<input id=i onkeydown=\"if(event.key==='Enter')window.entered=this.value\"><textarea id=t></textarea>",
        )
        .unwrap();
        page.eval("document.getElementById('i').focus()").unwrap();
        page.insert_text("hello").unwrap();
        page.press_enter().unwrap();
        assert_eq!(page.eval("window.entered").unwrap(), json!("hello"));
        f.finish();
    }

    #[test]
    fn navigate_geolocation_and_screenshot() {
        let Some(f) = launch("geo") else { return };
        let port = serve("<html><body><h1 id=h>hi</h1></body></html>");
        let page = f.browser.page();
        f.browser
            .set_geolocation(Some((42.5, -76.25, 33.0)))
            .unwrap();
        page.navigate(&format!("http://127.0.0.1:{port}/")).unwrap();
        assert_eq!(page.url().unwrap(), format!("http://127.0.0.1:{port}/"));
        let v = page
            .eval(
                "new Promise((res, rej) => navigator.geolocation.getCurrentPosition(\
                 p => res([p.coords.latitude, p.coords.longitude, p.coords.accuracy]), \
                 e => rej(new Error('geo error ' + e.code)), {timeout: 5000}))",
            )
            .unwrap();
        let got: Vec<f64> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_f64().unwrap())
            .collect();
        assert_eq!(got, vec![42.5, -76.25, 33.0]);
        let png = page.screenshot_png(Some((0.0, 0.0, 50.0, 40.0))).unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
        assert!(page.navigate("http://127.0.0.1:1/").is_err());
        f.finish();
    }

    #[test]
    fn geolocation_denied() {
        let Some(f) = launch("geodeny") else { return };
        let port = serve("<html><body>x</body></html>");
        f.browser.set_geolocation(None).unwrap();
        let page = f.browser.page();
        page.navigate(&format!("http://127.0.0.1:{port}/")).unwrap();
        let code = page
            .eval(
                "new Promise(res => navigator.geolocation.getCurrentPosition(\
                 () => res(0), e => res(e.code), {timeout: 5000}))",
            )
            .unwrap();
        assert_eq!(code, json!(1)); // PERMISSION_DENIED
        f.finish();
    }

    #[test]
    fn second_page_and_close() {
        let Some(f) = launch("pages") else { return };
        let p2 = f.browser.new_page().unwrap();
        p2.set_content("<p id=a>two</p>").unwrap();
        assert_eq!(
            p2.eval("document.getElementById('a').textContent").unwrap(),
            json!("two")
        );
        p2.close().unwrap();
        // The original page is unaffected.
        assert_eq!(f.browser.page().eval("2*3").unwrap(), json!(6));
        assert!(f.browser.is_alive());
        // wait_for: true case and timeout case.
        assert!(
            f.browser
                .page()
                .wait_for("true", Duration::from_millis(100))
                .unwrap()
        );
        assert!(
            !f.browser
                .page()
                .wait_for("false", Duration::from_millis(150))
                .unwrap()
        );
        f.finish();
    }

    #[test]
    fn closing_page_makes_browser_not_alive_and_drop_quits_chrome() {
        let Some(f) = launch("dead") else { return };
        let pid = f.browser.child.lock().unwrap().id();
        f.browser.page().close().unwrap();
        let t = Instant::now();
        while f.browser.is_alive() && t.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!f.browser.is_alive());
        f.finish();
        // After drop the Chrome process is gone (kill -0 fails).
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "chrome pid {pid} still running after drop");
    }

    #[test]
    fn profile_in_use_fails_fast() {
        let Some(f) = launch("inuse") else { return };
        let t = Instant::now();
        let second = Browser::launch(&LaunchOptions {
            headless: true,
            profile_dir: f.profile.clone(),
            chrome_path: None,
        });
        assert!(second.is_err());
        assert!(t.elapsed() < Duration::from_secs(20));
        f.finish();
    }

    #[test]
    fn stale_devtools_port_file_is_ignored() {
        if find_chrome().is_none() {
            return;
        }
        let profile =
            std::env::temp_dir().join(format!("pollev-cdp-test-{}-stale", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).unwrap();
        // A crashed earlier Chrome leaves this behind; its port is dead.
        std::fs::write(profile.join("DevToolsActivePort"), "1\n/devtools/browser/x").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let browser = Browser::launch(&LaunchOptions {
            headless: true,
            profile_dir: profile.clone(),
            chrome_path: None,
        })
        .unwrap();
        assert_eq!(browser.page().eval("1+1").unwrap(), json!(2));
        drop(browser);
        let _ = std::fs::remove_dir_all(profile);
    }

    #[test]
    fn find_chrome_returns_existing_file() {
        if let Some(p) = find_chrome() {
            assert!(p.is_file(), "{}", p.display());
        }
    }
}
