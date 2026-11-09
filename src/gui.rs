//! The window: a wry webview showing ui/index.html (the Python app's page,
//! reused as-is) with a small IPC shim that keeps its pywebview.api.* calls
//! working. Port of app/__main__.py + app/api.py. See PORT_SPEC.md "gui".
use crate::ai;
use crate::cdp;
use crate::config::{self, AppConfig};
use crate::notify::{self, Notifier};
use crate::watcher::{EventQueue, WatcherService};
use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value, json};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

const APP_TITLE: &str = "Poll Everywhere Abuse";
const INDEX_HTML: &str = include_str!("../ui/index.html");
const LEAFLET_CSS: &str = include_str!("../ui/leaflet.css");
const LEAFLET_JS: &str = include_str!("../ui/leaflet.js");
const CSS_TAG: &str = r#"<link rel="stylesheet" href="{static}/leaflet.css" />"#;
const JS_TAG: &str = r#"<script src="{static}/leaflet.js"></script>"#;

/// Defines window.pywebview.api as a Proxy so the page's pywebview.api.foo(...)
/// calls keep working: any method name becomes a function returning a Promise,
/// settled later by __ipcResolve (called from Rust on the main thread).
const BRIDGE_JS: &str = r#"
(() => {
  let nextId = 1;
  const pending = new Map();
  window.__ipcResolve = (id, ok, value) => {
    const p = pending.get(id);
    if (!p) return;
    pending.delete(id);
    if (ok) p.resolve(value); else p.reject(new Error(value));
  };
  const call = (name, args) => new Promise((resolve, reject) => {
    const id = nextId++;
    pending.set(id, { resolve, reject });
    window.ipc.postMessage(JSON.stringify({ id, name, args }));
  });
  window.pywebview = {
    api: new Proxy({}, {
      // "then" must stay undefined or awaiting the proxy would hang.
      get: (_, name) => (typeof name === 'string' && name !== 'then') ? (...args) => call(name, args) : undefined,
    }),
  };
})();
"#;

/// POLLEV_SELFTEST=1: once the page has loaded, exercise the real bridge and
/// report back through the same ipc channel.
const SELFTEST_JS: &str = r#"
window.addEventListener('load', async () => {
  const post = (r) => window.ipc.postMessage(JSON.stringify({ selftest: r }));
  try {
    const config = await pywebview.api.get_config();
    const state = await pywebview.api.get_state();
    const events = await pywebview.api.get_events();
    const chrome = await pywebview.api.check_playwright();
    let unknownRejected = false;
    try { await pywebview.api.no_such_method(); } catch (e) { unknownRejected = true; }
    const ok = typeof config === 'object' && 'poll_url' in config
      && typeof state.state === 'string' && Array.isArray(events)
      && typeof chrome.ok === 'boolean' && unknownRejected;
    post({ ok, config_keys: Object.keys(config).length, state: state.state, events: events.length, chrome });
  } catch (e) {
    post({ ok: false, error: String(e && e.message || e) });
  }
});
"#;

enum UserEvent {
    /// JS to run on the main thread (resolves a pending api promise).
    Eval(String),
    Exit(i32),
}

/// ui/index.html with leaflet inlined (a file or http reference would need
/// base-URL and sandbox wrinkles; inlining works the same everywhere).
fn page_html() -> String {
    INDEX_HTML
        .replace(CSS_TAG, &format!("<style>{LEAFLET_CSS}</style>"))
        .replace(JS_TAG, &format!("<script>{LEAFLET_JS}</script>"))
}

/// Open the window and run the event loop until it closes.
pub fn run() {
    if let Err(e) = run_inner() {
        eprintln!("{APP_TITLE}: {e:#}");
        std::process::exit(1);
    }
}

fn run_inner() -> Result<()> {
    let selftest = std::env::var("POLLEV_SELFTEST").is_ok_and(|v| v == "1");
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(APP_TITLE)
        .with_inner_size(LogicalSize::new(1200.0, 780.0))
        .with_min_inner_size(LogicalSize::new(940.0, 600.0))
        .build(&event_loop)?;

    // Without a main menu macOS has no Cmd-C/V/X/A, Cmd-Q or Cmd-W, so the
    // poll URL could not even be pasted.
    install_menu();

    // Cheap by contract (no Chrome, no network), so the window is not held up.
    let api = Arc::new(Api::new());

    let ipc_api = api.clone();
    let ipc_proxy = proxy.clone();
    let mut init = BRIDGE_JS.to_string();
    if selftest {
        init.push_str(SELFTEST_JS);
    }
    // Not incognito (the default): the map tile cache should persist.
    let webview = WebViewBuilder::new()
        .with_html(page_html())
        .with_initialization_script(init)
        .with_incognito(false)
        // Leaflet's "Leaflet" attribution link would otherwise navigate the
        // whole window away from the app (and hand window.pywebview.api,
        // which can run brew and send messages, to that site).
        .with_navigation_handler(|url| {
            let internal = ["about:", "data:", "blob:"]
                .iter()
                .any(|p| url.starts_with(p));
            if !internal {
                open_external(&url);
            }
            internal
        })
        .with_new_window_req_handler(|url, _| {
            open_external(&url);
            wry::NewWindowResponse::Deny
        })
        .with_ipc_handler(move |req| handle_ipc(&ipc_api, &ipc_proxy, selftest, req.into_body()))
        .build(&window)?;

    if selftest {
        let p = proxy.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            println!(
                "{}",
                json!({"ok": false, "error": "selftest timed out after 30s"})
            );
            let _ = p.send_event(UserEvent::Exit(1));
        });
    }

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _keep_alive = &webview;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // Hide first: stopping the watcher (Chrome closes gracefully so
                // cookies flush) can take a few seconds.
                window.set_visible(false);
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::Eval(js)) => {
                if let Err(e) = webview.evaluate_script(&js) {
                    eprintln!("evaluate_script failed: {e}");
                }
            }
            Event::UserEvent(UserEvent::Exit(code)) => {
                *control_flow = if code == 0 {
                    ControlFlow::Exit
                } else {
                    ControlFlow::ExitWithCode(code)
                };
            }
            // Also covers Cmd-Q, which skips CloseRequested.
            Event::LoopDestroyed => {
                // If the watcher is stuck (launch, navigation, a hung CDP call),
                // process::exit follows and would orphan Chrome with its profile lock.
                if !api.watcher.stop(Duration::from_secs(10)) {
                    crate::cdp::kill_all_browsers();
                }
            }
            _ => {}
        }
    })
}

/// Open an http(s) link in the default browser; anything else is dropped.
fn is_http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

fn open_external(url: &str) {
    if !is_http(url) {
        return;
    }
    let url = url.to_string();
    std::thread::spawn(move || {
        let _ = Command::new("open").arg(url).status();
    });
}

/// Minimal app/Edit/Window menus (what pywebview gave the Python app). tao has
/// no menu API and no extra crates are allowed, so this talks to AppKit through
/// the Objective-C runtime directly. Must run on the main thread.
#[cfg(target_os = "macos")]
fn install_menu() {
    use std::ffi::{CString, c_char, c_void};
    type Id = *mut c_void;
    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Id;
        fn sel_registerName(name: *const c_char) -> Id;
        fn objc_msgSend();
    }
    // objc_msgSend must be called through a pointer of the exact signature.
    unsafe fn f0(o: Id, s: Id) -> Id {
        unsafe {
            let f: unsafe extern "C" fn(Id, Id) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(o, s)
        }
    }
    unsafe fn f1(o: Id, s: Id, a: Id) -> Id {
        unsafe {
            let f: unsafe extern "C" fn(Id, Id, Id) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(o, s, a)
        }
    }
    unsafe fn f3(o: Id, s: Id, a: Id, b: Id, c: Id) -> Id {
        unsafe {
            let f: unsafe extern "C" fn(Id, Id, Id, Id, Id) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(o, s, a, b, c)
        }
    }
    unsafe fn fmask(o: Id, s: Id, m: u64) {
        unsafe {
            let f: unsafe extern "C" fn(Id, Id, u64) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            f(o, s, m)
        }
    }
    unsafe fn sel(n: &str) -> Id {
        let Ok(c) = CString::new(n) else {
            return std::ptr::null_mut();
        };
        unsafe { sel_registerName(c.as_ptr()) }
    }
    unsafe fn cls(n: &str) -> Id {
        let Ok(c) = CString::new(n) else {
            return std::ptr::null_mut();
        };
        unsafe { objc_getClass(c.as_ptr()) }
    }
    unsafe fn nsstring(s: &str) -> Id {
        let Ok(c) = CString::new(s) else {
            return std::ptr::null_mut();
        };
        unsafe {
            f1(
                cls("NSString"),
                sel("stringWithUTF8String:"),
                c.as_ptr() as Id,
            )
        }
    }
    // The few +1 objects below are leaked on purpose: the menu lives as long as the app.
    unsafe fn item(title: &str, action: Option<&str>, key: &str, mask: u64) -> Id {
        unsafe {
            let it = f0(cls("NSMenuItem"), sel("alloc"));
            let action = action.map_or(std::ptr::null_mut(), |a| sel(a));
            let it = f3(
                it,
                sel("initWithTitle:action:keyEquivalent:"),
                nsstring(title),
                action,
                nsstring(key),
            );
            if !key.is_empty() {
                fmask(it, sel("setKeyEquivalentModifierMask:"), mask);
            }
            it
        }
    }
    const CMD: u64 = 1 << 20;
    const SHIFT: u64 = 1 << 17;
    // (menu title, [(item title, action, key, modifiers)]); None = separator.
    type Entry = Option<(&'static str, &'static str, &'static str, u64)>;
    let menus: [(&str, Vec<Entry>); 3] = [
        (
            APP_TITLE,
            vec![
                Some(("Hide Poll Everywhere Abuse", "hide:", "h", CMD)),
                None,
                Some(("Quit Poll Everywhere Abuse", "terminate:", "q", CMD)),
            ],
        ),
        (
            "Edit",
            vec![
                Some(("Undo", "undo:", "z", CMD)),
                Some(("Redo", "redo:", "z", CMD | SHIFT)),
                None,
                Some(("Cut", "cut:", "x", CMD)),
                Some(("Copy", "copy:", "c", CMD)),
                Some(("Paste", "paste:", "v", CMD)),
                Some(("Select All", "selectAll:", "a", CMD)),
            ],
        ),
        (
            "Window",
            vec![
                Some(("Minimize", "performMiniaturize:", "m", CMD)),
                Some(("Close Window", "performClose:", "w", CMD)),
            ],
        ),
    ];
    unsafe {
        let app = f0(cls("NSApplication"), sel("sharedApplication"));
        if app.is_null() {
            return;
        }
        let bar = f0(f0(cls("NSMenu"), sel("alloc")), sel("init"));
        for (title, entries) in menus {
            let sub = f1(
                f0(cls("NSMenu"), sel("alloc")),
                sel("initWithTitle:"),
                nsstring(title),
            );
            for e in entries {
                let it = match e {
                    Some((t, a, k, m)) => item(t, Some(a), k, m),
                    None => f0(cls("NSMenuItem"), sel("separatorItem")),
                };
                f1(sub, sel("addItem:"), it);
            }
            let top = item(title, None, "", 0);
            f1(top, sel("setSubmenu:"), sub);
            f1(bar, sel("addItem:"), top);
        }
        f1(app, sel("setMainMenu:"), bar);
    }
}

#[cfg(not(target_os = "macos"))]
fn install_menu() {}

/// One message from the page: {id, name, args} (an api call) or {selftest}.
/// Each call gets its own thread, like pywebview, so a slow call (model list,
/// geocode) never freezes the UI.
fn handle_ipc(
    api: &Arc<Api>,
    proxy: &tao::event_loop::EventLoopProxy<UserEvent>,
    selftest: bool,
    body: String,
) {
    let Ok(msg) = serde_json::from_str::<Value>(&body) else {
        eprintln!("ignoring malformed ipc message");
        return;
    };
    if selftest && let Some(result) = msg.get("selftest") {
        println!("{result}");
        let code = if result.get("ok") == Some(&Value::Bool(true)) {
            0
        } else {
            1
        };
        let _ = proxy.send_event(UserEvent::Exit(code));
        return;
    }
    let Some(id) = msg.get("id").and_then(Value::as_u64) else {
        eprintln!("ignoring ipc message without an id");
        return;
    };
    let name = msg
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let args = msg
        .get("args")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let (api, proxy) = (api.clone(), proxy.clone());
    std::thread::spawn(move || {
        // A panic must still settle the promise, or the page waits forever.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| api.call(&name, &args)))
                .unwrap_or_else(|_| Err(anyhow!("{name} panicked")));
        let js = match result {
            Ok(v) => format!("window.__ipcResolve({id}, true, {v})"),
            Err(e) => format!(
                "window.__ipcResolve({id}, false, {})",
                Value::String(format!("{e:#}"))
            ),
        };
        let _ = proxy.send_event(UserEvent::Eval(js));
    });
}

/// The JS-callable methods, by name with positional args; same names and
/// return shapes as app/api.py's Api.
pub struct Api {
    settings_path: PathBuf,
    profile_dir: PathBuf,
    config: Mutex<AppConfig>,
    events: Arc<EventQueue>,
    watcher: WatcherService,
}

impl Api {
    pub fn new() -> Self {
        Self::with_paths(config::settings_path(), config::chrome_profile_dir())
    }

    /// Tests point these at temp dirs so they never touch App Support.
    fn with_paths(settings_path: PathBuf, profile_dir: PathBuf) -> Self {
        let events = Arc::new(EventQueue::new(500));
        Self {
            config: Mutex::new(AppConfig::load(&settings_path)),
            settings_path,
            profile_dir,
            watcher: WatcherService::new(events.clone()),
            events,
        }
    }

    pub fn call(&self, name: &str, args: &[Value]) -> Result<Value> {
        let s = |i: usize| args.get(i).and_then(Value::as_str).unwrap_or("");
        Ok(match name {
            "get_config" => self.get_config()?,
            "save_config" => self.save_config(args.first().and_then(Value::as_object))?,
            "start_watch" => self.start_watch(),
            "stop_watch" => self.stop_watch(),
            "get_state" => self.get_state(),
            "get_events" => Value::Array(self.events.get_events(100)),
            "decide" => self.watcher.decide(s(0), s(1)),
            "list_models" => self.list_models(),
            "test_model" => self.test_model(s(0)),
            "resolve_place" => resolve_place(s(0)),
            "approx_current_location" => approx_current_location(),
            "test_channel" => self.test_channel(s(0)),
            "check_playwright" => check_playwright(),
            "install_playwright" => install_playwright(),
            _ => bail!("unknown method {name:?}"),
        })
    }

    fn cfg(&self) -> MutexGuard<'_, AppConfig> {
        // A panicked holder cannot leave the config half-written (it is
        // replaced whole), so a poisoned lock is safe to reuse.
        self.config.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn get_config(&self) -> Result<Value> {
        Ok(serde_json::to_value(&*self.cfg())?)
    }

    /// Merge + persist. The UI sends the full settings form; unknown keys are
    /// dropped, missing keys keep their current values.
    fn save_config(&self, input: Option<&Map<String, Value>>) -> Result<Value> {
        let input = input.ok_or_else(|| anyhow!("save_config expects an object"))?;
        let mut cfg = self.cfg();
        let Value::Object(mut map) = serde_json::to_value(&*cfg)? else {
            bail!("config did not serialize to an object");
        };
        for (k, v) in input {
            let Some(cur) = map.get(k) else { continue };
            let mut candidate = map.clone();
            candidate.insert(k.clone(), coerce(cur, v.clone()));
            // A value of the wrong type for its field is ignored (the old
            // value stays) rather than poisoning the whole save.
            if serde_json::from_value::<AppConfig>(Value::Object(candidate.clone())).is_ok() {
                map = candidate;
            }
        }
        *cfg = serde_json::from_value(Value::Object(map))?;
        cfg.save(&self.settings_path)?;
        Ok(json!({"ok": true, "config": serde_json::to_value(&*cfg)?}))
    }

    fn start_watch(&self) -> Value {
        let cfg = self.cfg().clone();
        let problems = cfg.validate();
        if !problems.is_empty() && !self.watcher.is_alive() {
            return json!({"ok": false, "problems": problems});
        }
        match self.watcher.start(&cfg) {
            Ok(()) => json!({"ok": true}),
            // Already running, or the AI backend could not be set up.
            Err(e) => json!({"ok": false, "problems": [format!("{e:#}")]}),
        }
    }

    fn stop_watch(&self) -> Value {
        let stopped = self.watcher.stop(Duration::from_secs(15));
        json!({"ok": stopped, "state": self.watcher.state()})
    }

    fn get_state(&self) -> Value {
        json!({
            "state": self.watcher.state(),
            "running": self.watcher.is_alive(),
            "last_error": self.watcher.last_error(),
            "session_exists": self.profile_dir.exists(),
            "pending": self.watcher.pending(),
        })
    }

    fn list_models(&self) -> Value {
        let backend = self.cfg().ai_backend.clone();
        let source = match backend.as_str() {
            "subscription" => "aliases",
            "codex" => "codex-static",
            _ => "nrp",
        };
        match ai::list_models(&backend) {
            Ok(models) => json!({"ok": true, "models": models, "source": source}),
            Err(e) => json!({"ok": false, "error": truncate(&format!("{e:#}"), 500)}),
        }
    }

    /// One real call through the selected model, the same path a live
    /// question takes.
    fn test_model(&self, model: &str) -> Value {
        let cfg = self.cfg().clone();
        // Only the configured model is known to go with the configured effort.
        let effort =
            (model == cfg.ai_model && !cfg.ai_effort.is_empty()).then(|| cfg.ai_effort.clone());
        let model = (!model.is_empty()).then(|| model.to_string());
        let result = ai::make_ai_fn(
            &cfg.ai_backend,
            model,
            effort,
            ai::answer_schema(),
            std::env::temp_dir(),
        )
        .map(|f| ai::test_backend(&f, &AtomicBool::new(false)));
        match result {
            Ok((ok, detail)) => json!({"ok": ok, "message": detail}),
            Err(e) => json!({"ok": false, "message": format!("{e:#}")}),
        }
    }

    fn test_channel(&self, channel: &str) -> Value {
        let cfg = self.cfg().clone();
        match channel {
            "mac" => {
                notify::mac_notify("pollevbot test", "macOS notification works");
                json!({"ok": true})
            }
            "sound" => {
                notify::mac_sound();
                json!({"ok": true})
            }
            "ntfy" => {
                let topic = cfg.ntfy_topic.trim();
                if topic.is_empty() {
                    return json!({"ok": false, "error": "no ntfy topic set"});
                }
                notify::ntfy(topic, "pollevbot test", "ntfy push works", "");
                json!({"ok": true})
            }
            "imessage" => {
                let recipient = cfg.imessage_recipient.trim();
                if recipient.is_empty() {
                    return json!({"ok": false, "error": "no iMessage recipient set"});
                }
                let n = Notifier {
                    imessage: Some(recipient.to_string()),
                    ..Default::default()
                };
                n.send_imessage("pollevbot test: iMessage notifications work");
                json!({"ok": true})
            }
            _ => json!({"ok": false, "error": format!("unknown channel '{channel}'")}),
        }
    }
}

impl Default for Api {
    fn default() -> Self {
        Self::new()
    }
}

/// Coerce a JSON value back to the declared type of the field it is for. The
/// form sends real bools, but other paths can send strings, and "false" must
/// not become true.
fn coerce(cur: &Value, v: Value) -> Value {
    match (cur, v) {
        (Value::Bool(_), Value::String(s)) => Value::Bool(!matches!(
            s.trim().to_lowercase().as_str(),
            "" | "false" | "0" | "no" | "off"
        )),
        (Value::Bool(_), Value::Null) => Value::Bool(false),
        (Value::Bool(_), Value::Number(n)) => Value::Bool(n.as_f64() != Some(0.0)),
        (Value::Bool(_), Value::Array(a)) => Value::Bool(!a.is_empty()),
        (Value::Bool(_), Value::Object(o)) => Value::Bool(!o.is_empty()),
        (Value::Number(_), Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::String(s), Value::Number),
        (Value::String(_), Value::Null) => Value::String(String::new()),
        (_, v) => v,
    }
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into()
}

/// Free keyless geocoding (Nominatim). Bare "lat,lon" skips the network.
fn resolve_place(text: &str) -> Value {
    let text = text.trim();
    if text.is_empty() {
        return json!({"ok": false, "error": "enter a place name or coordinates"});
    }
    if let Some((a, b)) = text.split_once(',') {
        // Only the first two comma-separated parts count, like Python.
        let b = b.split(',').next().unwrap_or("");
        if let (Ok(lat), Ok(lon)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>())
            && lat.is_finite()
            && lon.is_finite()
        {
            return json!({"ok": true, "lat": lat, "lon": lon, "name": format!("{lat:.5}, {lon:.5}")});
        }
    }
    let lookup = || -> Result<Value> {
        let mut resp = http_agent(Duration::from_secs(15))
            .get("https://nominatim.openstreetmap.org/search")
            .query("q", text)
            .query("format", "json")
            .query("limit", "1")
            .header("User-Agent", "pollevbot-gui/1.0")
            .call()?;
        let results: Value = resp.body_mut().read_json()?;
        let Some(r) = results.get(0) else {
            return Ok(json!({"ok": false, "error": format!("no results for '{text}'")}));
        };
        let coord = |k: &str| -> Result<f64> {
            let v = r.get(k).ok_or_else(|| anyhow!("result has no {k}"))?;
            match v.as_str() {
                Some(s) => Ok(s.parse()?),
                None => v.as_f64().ok_or_else(|| anyhow!("bad {k}")),
            }
        };
        let name = r
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(text);
        Ok(json!({"ok": true, "lat": coord("lat")?, "lon": coord("lon")?, "name": name}))
    };
    lookup().unwrap_or_else(|e| json!({"ok": false, "error": format!("{e:#}")}))
}

/// City-level IP geolocation (keyless) so the map can show where you really
/// are next to the fake pin.
fn approx_current_location() -> Value {
    let lookup = || -> Result<Value> {
        let mut resp = http_agent(Duration::from_secs(10))
            .get("https://ipapi.co/json/")
            .header("User-Agent", "pollevbot-gui/1.0")
            .call()?;
        let data: Value = resp.body_mut().read_json()?;
        // A rate-limit reply can carry null coordinates; never hand those to the map.
        match (
            data.get("latitude").and_then(Value::as_f64),
            data.get("longitude").and_then(Value::as_f64),
        ) {
            (Some(lat), Some(lon)) => Ok(json!({"ok": true, "lat": lat, "lon": lon})),
            _ => bail!("response had no coordinates"),
        }
    };
    lookup().unwrap_or_else(|e| json!({"ok": false, "error": format!("{e:#}")}))
}

/// Is a Chrome-family browser installed? (The name is kept: the page calls it.)
fn check_playwright() -> Value {
    match cdp::find_chrome() {
        Some(path) => json!({"ok": true, "path": path.to_string_lossy()}),
        None => json!({"ok": false, "error": "no Chrome-family browser found"}),
    }
}

/// `brew install --cask google-chrome`; {ok, output: last 2000 chars}.
fn install_playwright() -> Value {
    const BREW_DIRS: [&str; 2] = ["/opt/homebrew/bin", "/usr/local/bin"];
    let Some(brew) = BREW_DIRS
        .iter()
        .map(|d| PathBuf::from(d).join("brew"))
        .find(|p| p.exists())
    else {
        return json!({"ok": false, "output": "Homebrew not found (looked in /opt/homebrew/bin and /usr/local/bin). Install it from https://brew.sh, or install Google Chrome by hand."});
    };
    let mut cmd = Command::new(brew);
    cmd.args(["install", "--cask", "google-chrome"]);
    let (ok, output) = run_capture(cmd, Duration::from_secs(600));
    json!({"ok": ok, "output": tail_chars(&output, 2000)})
}

fn tail_chars(s: &str, n: usize) -> String {
    let skip = s.chars().count().saturating_sub(n);
    s.chars().skip(skip).collect()
}

/// Run to completion (or kill after `timeout`), returning (success,
/// stdout+stderr). Readers run on threads so a full pipe cannot deadlock.
fn run_capture(mut cmd: Command, timeout: Duration) -> (bool, String) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, format!("could not start: {e}")),
    };
    // Output lands in a shared buffer as it arrives, so a reader that never
    // sees EOF (a grandchild still holds the pipe) cannot lose what was read
    // or block us.
    type Buf = Arc<Mutex<Vec<u8>>>;
    fn drain(r: Option<impl Read + Send + 'static>) -> (Buf, std::thread::JoinHandle<()>) {
        let buf: Buf = Arc::default();
        let b = buf.clone();
        let h = std::thread::spawn(move || {
            let Some(mut r) = r else { return };
            let mut chunk = [0u8; 4096];
            while let Ok(n) = r.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                b.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend_from_slice(&chunk[..n]);
            }
        });
        (buf, h)
    }
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let deadline = Instant::now() + timeout;
    let (ok, note) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.success(), String::new()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break (false, format!("\ntimed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => break (false, format!("\nwait failed: {e}")),
        }
    };
    // Give the readers a moment to flush, then move on even if a leftover
    // grandchild keeps the pipes open.
    let grace = Instant::now() + Duration::from_secs(2);
    while !(out.1.is_finished() && err.1.is_finished()) && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(20));
    }
    let take = |b: &Buf| {
        String::from_utf8_lossy(&b.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    };
    (ok, take(&out.0) + &take(&err.0) + &note)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_api(tag: &str) -> (Api, PathBuf) {
        let dir = std::env::temp_dir().join(format!("pea-gui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (
            Api::with_paths(
                dir.join("settings/app_settings.json"),
                dir.join("chrome-profile"),
            ),
            dir,
        )
    }

    #[test]
    fn get_and_save_config() {
        let (api, dir) = temp_api("config");
        let cfg = api.call("get_config", &[]).unwrap();
        assert_eq!(cfg["poll_url"], "");
        assert_eq!(cfg["geo_accuracy"], 50.0);

        let r = api
            .call(
                "save_config",
                &[json!({
                    "poll_url": "https://pollev.com/abc",
                    "username": null,
                    "headless": "yes",
                    "notify_sound": "false",
                    "geo_lat": "42.5",
                    "geo_lon": 7,
                    "geo_accuracy": "junk",
                    "ai_enabled": 0,
                    "not_a_key": 1,
                })],
            )
            .unwrap();
        assert_eq!(r["ok"], true);
        let c = &r["config"];
        assert_eq!(c["poll_url"], "https://pollev.com/abc");
        assert_eq!(c["username"], "");
        assert_eq!(c["headless"], true);
        assert_eq!(c["notify_sound"], false);
        assert_eq!(c["geo_lat"], 42.5);
        assert_eq!(c["geo_lon"], 7.0);
        assert_eq!(
            c["geo_accuracy"], 50.0,
            "unparseable float keeps the old value"
        );
        assert_eq!(c["ai_enabled"], false);
        assert!(c.get("not_a_key").is_none());

        // Persisted to the injected path, and a fresh Api reads it back.
        let path = dir.join("settings/app_settings.json");
        assert_eq!(AppConfig::load(&path).poll_url, "https://pollev.com/abc");
        let api2 = Api::with_paths(path, dir.join("chrome-profile"));
        assert_eq!(api2.call("get_config", &[]).unwrap()["geo_lat"], 42.5);

        // Missing keys keep their values on a later partial save.
        let r = api
            .call("save_config", &[json!({"username": "me"})])
            .unwrap();
        assert_eq!(r["config"]["poll_url"], "https://pollev.com/abc");
        assert_eq!(r["config"]["username"], "me");
        assert!(api.call("save_config", &[json!("nope")]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_method_is_err() {
        let (api, _) = temp_api("unknown");
        let e = api.call("no_such_method", &[]).unwrap_err().to_string();
        assert!(e.contains("no_such_method"), "{e}");
    }

    #[test]
    fn start_watch_validates_first() {
        let (api, dir) = temp_api("start");
        let r = api.call("start_watch", &[]).unwrap();
        assert_eq!(r["ok"], false);
        let problems = r["problems"].as_array().unwrap();
        assert!(
            problems.iter().any(|p| p == "Poll URL is empty"),
            "{problems:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_reports_session_dir() {
        let (api, dir) = temp_api("state");
        let s = api.call("get_state", &[]).unwrap();
        assert_eq!(s["session_exists"], false);
        assert_eq!(s["running"], false);
        assert!(s["pending"].is_null());
        std::fs::create_dir_all(dir.join("chrome-profile")).unwrap();
        assert_eq!(api.call("get_state", &[]).unwrap()["session_exists"], true);
        assert!(api.call("get_events", &[]).unwrap().is_array());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_place_offline_forms() {
        let (api, _) = temp_api("place");
        let r = api
            .call("resolve_place", &[json!(" 40.7128, -74.006 ")])
            .unwrap();
        assert_eq!(
            r,
            json!({"ok": true, "lat": 40.7128, "lon": -74.006, "name": "40.71280, -74.00600"})
        );
        let r = api.call("resolve_place", &[json!("   ")]).unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "enter a place name or coordinates");
        // Extra parts are ignored, like Python.
        assert_eq!(
            api.call("resolve_place", &[json!("1,2,3")]).unwrap()["lon"],
            2.0
        );
    }

    #[test]
    fn unknown_channel_and_missing_targets() {
        let (api, _) = temp_api("channel");
        assert_eq!(
            api.call("test_channel", &[json!("fax")]).unwrap()["error"],
            "unknown channel 'fax'"
        );
        assert_eq!(
            api.call("test_channel", &[json!("ntfy")]).unwrap()["error"],
            "no ntfy topic set"
        );
        assert_eq!(
            api.call("test_channel", &[json!("imessage")]).unwrap()["error"],
            "no iMessage recipient set"
        );
    }

    #[test]
    fn check_playwright_shape() {
        let (api, _) = temp_api("chrome");
        let r = api.call("check_playwright", &[]).unwrap();
        match r["ok"].as_bool() {
            Some(true) => assert!(r["path"].is_string()),
            Some(false) => assert_eq!(r["error"], "no Chrome-family browser found"),
            None => panic!("{r}"),
        }
    }

    #[test]
    fn page_inlines_leaflet() {
        let html = page_html();
        assert!(!html.contains("{static}"));
        assert!(html.contains("<style>") && html.contains("leaflet-container"));
        assert!(html.contains("Leaflet") && html.contains("pywebview.api.get_config"));
    }

    #[test]
    fn coerce_rules() {
        assert_eq!(coerce(&json!(true), json!("Off")), json!(false));
        assert_eq!(coerce(&json!(false), json!("1")), json!(true));
        assert_eq!(coerce(&json!(1.0), json!(" 3.5 ")), json!(3.5));
        assert_eq!(coerce(&json!(""), Value::Null), json!(""));
        assert_eq!(coerce(&json!(""), json!("x")), json!("x"));
    }

    #[test]
    fn tail_and_capture() {
        assert_eq!(tail_chars("héllo wörld", 5), "wörld");
        assert_eq!(tail_chars("ab", 5), "ab");
        let mut c = Command::new("sh");
        c.args(["-c", "echo out; echo err 1>&2; exit 3"]);
        let (ok, text) = run_capture(c, Duration::from_secs(5));
        assert!(
            !ok && text.contains("out") && text.contains("err"),
            "{text}"
        );
        let mut c = Command::new("sleep");
        c.arg("5");
        let (ok, text) = run_capture(c, Duration::from_millis(200));
        assert!(!ok && text.contains("timed out"), "{text}");
        // A leftover grandchild holding the pipe must not hang the call or
        // lose the output already read.
        let mut c = Command::new("sh");
        c.args(["-c", "echo out; sleep 6 &"]);
        let t = Instant::now();
        let (ok, text) = run_capture(c, Duration::from_secs(5));
        assert!(ok && text.contains("out"), "{text}");
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    }

    #[test]
    fn only_http_links_leave_the_app() {
        assert!(is_http("https://leafletjs.com") && is_http("http://x.test"));
        for u in [
            "file:///etc/passwd",
            "-a Calculator",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!is_http(u), "{u}");
        }
    }
}
