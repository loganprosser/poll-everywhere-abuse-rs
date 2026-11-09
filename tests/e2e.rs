//! End-to-end tests: WatcherService + headless Chrome + a fake PollEv server
//! on 127.0.0.1. Run with `cargo test --test e2e -- --test-threads=1`.
//!
//! The fake page polls the server for its content every 25ms, so a test can
//! switch the served page (idle -> question -> idle ...) like PollEv's SPA
//! does. Buttons fetch `/vote?..`, `/text?..`, `/click?..` etc.; every such
//! request is recorded with a timestamp. The poll URL contains "/pollev.com/"
//! because the app decides "on PollEv" by substring.
use anyhow::anyhow;
use poll_everywhere_abuse::ai::AiFn;
use poll_everywhere_abuse::cdp::find_chrome;
use poll_everywhere_abuse::config::AppConfig;
use poll_everywhere_abuse::notify::NotifyFn;
use poll_everywhere_abuse::watcher::{EventQueue, Paths, WatcherService};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- server

const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

struct St {
    html: String,
    ver: u64,
    hits: Vec<(Instant, String)>,
    /// (request path prefix, html to switch to when it is hit)
    rules: Vec<(String, String)>,
}

struct Srv {
    base: String,
    st: Arc<Mutex<St>>,
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn b64(s: &str) -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).unwrap()
}

impl Srv {
    fn start(html: &str) -> Srv {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let st = Arc::new(Mutex::new(St {
            html: html.to_string(),
            ver: 1,
            hits: vec![],
            rules: vec![],
        }));
        let shared = st.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let st = shared.clone();
                std::thread::spawn(move || {
                    let mut s = stream;
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 8192];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (ctype, body): (&str, Vec<u8>) = if path == "/content" {
                        let g = st.lock().unwrap();
                        (
                            "application/json",
                            json!({"ver": g.ver, "html": g.html}).to_string().into_bytes(),
                        )
                    } else if path == "/img.png" {
                        ("image/png", b64(PNG_1X1))
                    } else if path.starts_with("/pollev.com") {
                        let g = st.lock().unwrap();
                        let page = format!(
                            "<html><head><title>fake pollev</title></head><body>{}</body>\
                             <script>let cur={};(function tick(){{fetch('/content').then(r=>r.json()).then(j=>{{\
                             if(j.ver!==cur){{cur=j.ver;document.body.innerHTML=j.html;}}}}).catch(()=>{{}})\
                             .finally(()=>setTimeout(tick,25));}})();</script></html>",
                            g.html, g.ver
                        );
                        ("text/html", page.into_bytes())
                    } else if ["/vote", "/text", "/click", "/agree", "/cont", "/login"]
                        .iter()
                        .any(|p| path.starts_with(p))
                    {
                        let mut g = st.lock().unwrap();
                        g.hits.push((Instant::now(), percent_decode(&path)));
                        let hit = g
                            .rules
                            .iter()
                            .find(|(p, _)| path.starts_with(p.as_str()))
                            .map(|(_, h)| h.clone());
                        if let Some(h) = hit {
                            g.html = h;
                            g.ver += 1;
                        }
                        ("text/plain", vec![])
                    } else {
                        ("text/plain", vec![])
                    };
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(&body);
                });
            }
        });
        Srv { base, st }
    }

    fn url(&self) -> String {
        format!("{}/pollev.com/class", self.base)
    }

    fn set(&self, html: &str) -> Instant {
        let mut g = self.st.lock().unwrap();
        g.html = html.to_string();
        g.ver += 1;
        Instant::now()
    }

    fn after_hit(&self, prefix: &str, html: &str) {
        self.st
            .lock()
            .unwrap()
            .rules
            .push((prefix.to_string(), html.to_string()));
    }

    /// Recorded request paths (decoded) starting with `prefix`.
    fn hits(&self, prefix: &str) -> Vec<String> {
        self.hits_t(prefix).into_iter().map(|h| h.1).collect()
    }

    fn hits_t(&self, prefix: &str) -> Vec<(Instant, String)> {
        self.st
            .lock()
            .unwrap()
            .hits
            .iter()
            .filter(|(_, p)| p.starts_with(prefix))
            .cloned()
            .collect()
    }

    fn wait_hits(&self, prefix: &str, n: usize, secs: u64) -> Vec<String> {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if self.hits(prefix).len() >= n {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.hits(prefix)
    }
}

// ----------------------------------------------------------------- pages

const VOTE_CLASS: &str = "component-response-multiple-choice__option__vote";
const IDLE: &str = "<p>Waiting for the presenter to start the poll</p>";

fn activity_script(a: &Value) -> String {
    format!("<script>window.__setup = {{activity: {a}}};</script>")
}

fn mc_page(id: i64, max_votes: Value, opts: &[&str]) -> String {
    let a = json!({
        "id": id, "shared_key": "k", "type": "multiple_choice_poll", "title": format!("Pick {id}"),
        "max_votes": max_votes,
        "options": opts.iter().enumerate().map(|(i, t)| json!({"id": i + 1, "value": t})).collect::<Vec<_>>(),
    });
    let buttons: String = opts
        .iter()
        .map(|t| {
            format!(
                "<button class='{VOTE_CLASS}' onclick=\"fetch('/vote?'+encodeURIComponent(this.innerText.trim()))\">{t}</button>"
            )
        })
        .collect();
    format!("{}{buttons}", activity_script(&a))
}

fn short_page(id: i64) -> String {
    let a = json!({"id": id, "type": "free_text_poll", "title": format!("Why {id}?")});
    format!(
        "{}<textarea name='response'></textarea>\
         <input type='submit' value='Send' onclick=\"fetch('/text?'+encodeURIComponent(document.querySelector('textarea').value))\">",
        activity_script(&a)
    )
}

fn image_page(id: i64, base: &str) -> String {
    let a = json!({
        "id": id, "type": "clickable_image", "title": "Where?", "max_votes": 1,
        "type_options": {"imageUrl": format!("{base}/img.png")},
    });
    format!(
        "{}<img class='component-response-clickable-image__image' src='/img.png' \
         style='display:block;margin:20px 0 0 30px;width:400px;height:200px' \
         onclick=\"var r=this.getBoundingClientRect();fetch('/click?'+((event.clientX-r.left)/r.width)+','+((event.clientY-r.top)/r.height))\">",
        activity_script(&a)
    )
}

// ------------------------------------------------------------------ AI stubs

fn ai_text(answer: &str) -> AiFn {
    let raw = json!({"answer": answer, "confidence": 90, "reasoning": "stub"}).to_string();
    Arc::new(move |_p: &str, _s: &AtomicBool| Ok(raw.clone()))
}

fn ai_json(raw: Value) -> AiFn {
    let raw = raw.to_string();
    Arc::new(move |_p: &str, _s: &AtomicBool| Ok(raw.clone()))
}

fn ai_never() -> AiFn {
    Arc::new(|_p: &str, _s: &AtomicBool| Err(anyhow!("this AI must not be called")))
}

// ------------------------------------------------------------------- harness

struct Run {
    svc: WatcherService,
    queue: Arc<EventQueue>,
    events: Mutex<Vec<(Instant, Value)>>,
    notes: Arc<Mutex<Vec<(String, String)>>>,
    dir: PathBuf,
    t0: Instant,
}

fn chrome_or_skip() -> bool {
    if find_chrome().is_none() {
        eprintln!("skipping: no Chrome-family browser");
        return false;
    }
    true
}

impl Run {
    fn start(tag: &str, url: &str, cfg: impl FnOnce(&mut AppConfig), ai: Option<(AiFn, AiFn)>) -> Run {
        let dir = std::env::temp_dir().join(format!("pollev_e2e_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let queue = Arc::new(EventQueue::new(1000));
        let svc = WatcherService::new(queue.clone());
        let mut c = AppConfig {
            poll_url: url.to_string(),
            headless: true,
            geolocation_enabled: false,
            ai_enabled: true,
            ai_require_confirmation: false,
            ai_test_on_startup: false,
            notify_imessage: false,
            notify_ntfy: false,
            ..AppConfig::default()
        };
        cfg(&mut c);
        let notes = Arc::new(Mutex::new(Vec::new()));
        let n = notes.clone();
        let notify: NotifyFn = Arc::new(move |t: &str, m: &str, _u: &str| {
            n.lock().unwrap().push((t.to_string(), m.to_string()))
        });
        let paths = Paths {
            profile_dir: dir.join("profile"),
            logs_dir: dir.join("logs"),
        };
        let t0 = Instant::now();
        svc.start_with(&c, paths, ai, notify).unwrap();
        Run {
            svc,
            queue,
            events: Mutex::new(vec![]),
            notes,
            dir,
            t0,
        }
    }

    fn profile(&self) -> String {
        self.dir.join("profile").to_string_lossy().into_owned()
    }

    fn drain(&self) -> Vec<Value> {
        let now = Instant::now();
        let mut g = self.events.lock().unwrap();
        for e in self.queue.get_events(1000) {
            g.push((now, e));
        }
        g.iter().map(|(_, e)| e.clone()).collect()
    }

    fn timed(&self) -> Vec<(Instant, Value)> {
        self.drain();
        self.events.lock().unwrap().clone()
    }

    /// Wait until `f(all events so far)` holds; panics with the events on timeout.
    fn wait(&self, what: &str, secs: u64, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let evs = self.drain();
            if f(&evs) {
                return evs;
            }
            if Instant::now() >= end {
                panic!(
                    "timed out ({secs}s) waiting for {what}; state={} last_error={:?}\nevents: {evs:#?}\nnotes: {:?}",
                    self.svc.state(),
                    self.svc.last_error(),
                    self.notes.lock().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_state(&self, state: &str, secs: u64) -> Vec<Value> {
        let s = state.to_string();
        self.wait(&format!("state {state}"), secs, move |e| {
            e.iter().any(|x| x["event"] == "state" && x["state"] == s.as_str())
        })
    }

    /// Time from start() to the first "watching" event (10ms resolution).
    fn watching_after(&self) -> Duration {
        self.timed()
            .iter()
            .find(|(_, e)| e["event"] == "state" && e["state"] == "watching")
            .map(|(t, _)| t.duration_since(self.t0))
            .expect("no watching event")
    }

    fn results(&self) -> Vec<Value> {
        self.drain().into_iter().filter(|e| e["event"] == "result").collect()
    }

    fn wait_result(&self, n: usize, secs: u64) -> Vec<Value> {
        self.wait(&format!("{n} result event(s)"), secs, |e| {
            e.iter().filter(|x| x["event"] == "result").count() >= n
        });
        self.results()
    }

    /// Wait until the service has registered the pending decision for the
    /// question titled `title` (decide() before that answers "nothing
    /// pending"). Returns the awaiting_reply events seen so far.
    ///
    /// Note: the watch loop emits its own awaiting_reply event BEFORE the
    /// service registers the pending decision, and the service emits a second
    /// one (with "channels") after; the Python app did the same, so a test must
    /// wait on pending(), not on the first event.
    fn awaiting(&self, title: &str, secs: u64) -> Vec<Value> {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            if self.svc.pending().is_some_and(|p| p["question"] == title) {
                return self.drain().into_iter().filter(|e| e["event"] == "awaiting_reply").collect();
            }
            if Instant::now() >= end {
                panic!("timed out waiting for pending '{title}': state={} events={:#?}", self.svc.state(), self.drain());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn note_titles(&self) -> Vec<String> {
        self.notes.lock().unwrap().iter().map(|n| n.0.clone()).collect()
    }
}

fn chrome_pids(profile: &str) -> Vec<u32> {
    Command::new("pgrep")
        .args(["-f", profile])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

impl Drop for Run {
    fn drop(&mut self) {
        self.svc.stop(Duration::from_secs(15));
        let profile = self.profile();
        // Never leave a browser behind, even when a test failed half way.
        for pid in chrome_pids(&profile) {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn mc_ai(answer: &str) -> Option<(AiFn, AiFn)> {
    Some((ai_text(answer), ai_never()))
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

// --------------------------------------------------------------------- tests

#[test]
fn t1_multiple_choice_exact_option_and_timings() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let run = Run::start("mc", &srv.url(), |_| {}, mc_ai("2"));
    run.wait_state("watching", 30);
    let watching = run.watching_after();
    println!("MEASURE start -> watching: {}", ms(watching));
    // Let the loop settle on the idle page, then switch to a question.
    std::thread::sleep(Duration::from_millis(500));
    let t_switch = srv.set(&mc_page(1, json!(1), &["12", "2"]));
    let votes = srv.wait_hits("/vote", 1, 20);
    assert_eq!(votes, vec!["/vote?2".to_string()], "exactly vote 2");
    let t_vote = srv.hits_t("/vote")[0].0;
    println!(
        "MEASURE page switch -> vote request: {} (includes <=25ms for the fake page to refresh)",
        ms(t_vote.duration_since(t_switch))
    );
    let res = run.wait_result(1, 20);
    assert_eq!(res[0]["status"], json!("SUBMITTED"), "{res:?}");
    assert_eq!(res[0]["answer"], json!("2"));
    assert_eq!(res[0]["type"], json!("multiple_choice"));
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(srv.hits("/vote").len(), 1, "no second vote");
    let t_stop = Instant::now();
    assert!(run.svc.stop(Duration::from_secs(10)));
    println!("MEASURE stop() with idle watcher: {}", ms(t_stop.elapsed()));
    assert_eq!(run.svc.state(), "stopped");
}

#[test]
fn t2a_select_multiple() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let run = Run::start("sel", &srv.url(), |_| {}, Some((ai_json(json!({"answer": ["Red", "Blue"], "confidence": 80, "reasoning": "s"})), ai_never())));
    run.wait_state("watching", 30);
    srv.set(&mc_page(2, Value::Null, &["Red", "Green", "Blue"]));
    let votes = srv.wait_hits("/vote", 2, 20);
    assert_eq!(votes, vec!["/vote?Red".to_string(), "/vote?Blue".to_string()]);
    let res = run.wait_result(1, 20);
    assert_eq!(res[0]["status"], json!("SUBMITTED"), "{res:?}");
    assert_eq!(res[0]["type"], json!("select_multiple"));
}

#[test]
fn t2b_short_answer_send_accept_skip() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let run = Run::start("short", &srv.url(), |_| {}, mc_ai("ai suggestion"));
    run.wait_state("watching", 30);

    // 1: user types their own answer in the app.
    srv.set(&short_page(10));
    let a = run.awaiting("Why 10?", 20);
    assert_eq!(a[0]["mode"], json!("short_answer"));
    assert_eq!(a[0]["answer"], json!("ai suggestion"));
    assert_eq!(run.svc.pending().unwrap()["mode"], json!("short_answer"));
    assert!(srv.hits("/text").is_empty(), "nothing submitted before the user decides");
    assert_eq!(run.svc.decide("send", "my answer"), json!({"resolved": true}));
    assert_eq!(srv.wait_hits("/text", 1, 10), vec!["/text?my answer".to_string()]);
    let res = run.wait_result(1, 10);
    assert_eq!(res[0]["status"], json!("SUBMITTED"), "{res:?}");
    assert_eq!(res[0]["answer"], json!("my answer"));

    // 2: accept = the AI's suggestion as-is (second question in the same run,
    // which also proves the per-question reply plumbing is not left stale).
    srv.set(&short_page(11));
    run.awaiting("Why 11?", 20);
    assert_eq!(run.svc.decide("accept", ""), json!({"resolved": true}));
    assert_eq!(
        srv.wait_hits("/text", 2, 10),
        vec!["/text?my answer".to_string(), "/text?ai suggestion".to_string()]
    );
    run.wait_result(2, 10);

    // 3: skip submits nothing.
    srv.set(&short_page(12));
    run.awaiting("Why 12?", 20);
    assert_eq!(run.svc.decide("skip", ""), json!({"resolved": true}));
    let res = run.wait_result(3, 10);
    assert_eq!(res[2]["status"], json!("SKIPPED (no reply)"), "{res:?}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(srv.hits("/text").len(), 2);
    assert_eq!(run.svc.decide("accept", ""), json!({"resolved": false, "reason": "nothing pending"}));
}

#[test]
fn t2c_confirm_accept_and_skip() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let run = Run::start("confirm", &srv.url(), |c| c.ai_require_confirmation = true, mc_ai("2"));
    run.wait_state("watching", 30);
    srv.set(&mc_page(20, json!(1), &["12", "2"]));
    let a = run.awaiting("Pick 20", 20);
    assert_eq!(a[0]["mode"], json!("confirm"));
    std::thread::sleep(Duration::from_millis(400));
    assert!(srv.hits("/vote").is_empty());
    assert_eq!(run.svc.decide("accept", ""), json!({"resolved": true}));
    assert_eq!(srv.wait_hits("/vote", 1, 10), vec!["/vote?2".to_string()]);
    let res = run.wait_result(1, 10);
    assert_eq!(res[0]["status"], json!("SUBMITTED"), "{res:?}");

    srv.set(&mc_page(21, json!(1), &["12", "2"]));
    run.awaiting("Pick 21", 20);
    assert_eq!(run.svc.decide("skip", ""), json!({"resolved": true}));
    let res = run.wait_result(2, 10);
    assert_eq!(res[1]["status"], json!("SKIPPED (no reply / denied)"), "{res:?}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(srv.hits("/vote").len(), 1);
}

#[test]
fn t3_clickable_image() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let click = ai_json(json!({"clicks": [{"x": 0.25, "y": 0.75}], "confidence": 70, "reasoning": "stub"}));
    let run = Run::start("img", &srv.url(), |_| {}, Some((ai_never(), click)));
    run.wait_state("watching", 30);
    srv.set(&image_page(30, &srv.base));
    let hits = srv.wait_hits("/click", 1, 20);
    assert_eq!(hits.len(), 1, "{hits:?}");
    let (x, y) = hits[0].trim_start_matches("/click?").split_once(',').unwrap();
    let (x, y): (f64, f64) = (x.parse().unwrap(), y.parse().unwrap());
    assert!((x - 0.25).abs() <= 0.03 && (y - 0.75).abs() <= 0.03, "clicked at {x},{y}");
    let res = run.wait_result(1, 20);
    assert_eq!(res[0]["status"], json!("SUBMITTED"), "{res:?}");
    assert_eq!(res[0]["type"], json!("clickable_image"));
    // The downloaded image lands under logs_dir (the AI's cwd).
    assert!(run.dir.join("logs/images/30.png").exists());
}

#[test]
fn t4_cookie_banner_and_screen_name() {
    if !chrome_or_skip() {
        return;
    }
    let setup = "<div id='bar'>We use cookies <button onclick=\"this.parentNode.remove();fetch('/agree')\">Agree</button></div>\
        <h1>Join the poll</h1><input type='text' id='n' placeholder='Screen name'>\
        <button onclick=\"fetch('/cont?'+encodeURIComponent(document.getElementById('n').value))\">Continue</button>";
    let srv = Srv::start(setup);
    srv.after_hit("/cont", &mc_page(40, json!(1), &["12", "2"]));
    let run = Run::start("setup", &srv.url(), |c| c.username = "abc123".into(), mc_ai("2"));
    let votes = srv.wait_hits("/vote", 1, 30);
    assert_eq!(srv.hits("/agree").len(), 1, "cookie banner clicked once");
    assert_eq!(srv.hits("/cont"), vec!["/cont?abc123".to_string()], "screen name");
    assert_eq!(votes, vec!["/vote?2".to_string()]);
    let res = run.wait_result(1, 20);
    assert_eq!(res[0]["status"], json!("SUBMITTED"), "{res:?}");
    println!("MEASURE start -> vote with cookie+name setup: {} (vote hit)", ms(srv.hits_t("/vote")[0].0.duration_since(run.t0)));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(srv.hits("/cont").len(), 1, "screen name submitted once");
}

#[test]
fn t5_login_wall_then_poll() {
    if !chrome_or_skip() {
        return;
    }
    let wall = "<h1>Pre-registration required</h1><p>Log in to the Poll Everywhere account registered with Cornell to respond.</p>\
        <a class='mdc-button' href='#' onclick=\"fetch('/login');return false\">Log in</a>";
    let srv = Srv::start(wall);
    let run = Run::start("wall", &srv.url(), |c| c.username = "abc123".into(), mc_ai("2"));
    let evs = run.wait("the startup sso_wall event", 30, |e| {
        e.iter().any(|x| x["event"] == "sso_wall")
    });
    let walls: Vec<&Value> = evs.iter().filter(|e| e["event"] == "sso_wall").collect();
    assert_eq!(walls, vec![&json!({"event": "sso_wall", "phase": "startup"})]);
    assert!(
        !evs.iter().any(|e| e["event"] == "state" && e["state"] == "watching"),
        "must not report watching while the wall is up: {evs:?}"
    );
    // The wall's own Log in button is clicked (nav "Log in" links are not).
    let logins = srv.wait_hits("/login", 1, 10);
    println!("MEASURE wall Log in clicks within the first seconds: {}", logins.len());
    // Registration done: the real page now shows the poll.
    srv.set(&mc_page(50, json!(1), &["12", "2"]));
    run.wait_state("watching", 20);
    let votes = srv.wait_hits("/vote", 1, 20);
    assert_eq!(votes, vec!["/vote?2".to_string()]);
    let evs = run.drain();
    assert_eq!(
        evs.iter().filter(|e| e["event"] == "sso_wall").count(),
        1,
        "no mid_session/cleared events for a startup wall: {evs:?}"
    );
    assert_eq!(run.wait_result(1, 20)[0]["status"], json!("SUBMITTED"));
}

#[test]
fn t6_stop_while_awaiting_confirmation() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(&mc_page(60, json!(1), &["12", "2"]));
    let run = Run::start("stopconf", &srv.url(), |c| c.ai_require_confirmation = true, mc_ai("2"));
    run.awaiting("Pick 60", 30);
    let t = Instant::now();
    let stopped = run.svc.stop(Duration::from_secs(3));
    let took = t.elapsed();
    println!("MEASURE stop() while awaiting confirmation: {}", ms(took));
    assert!(stopped, "stop() did not finish within 3s (took {took:?})");
    assert!(took < Duration::from_secs(3));
    assert_eq!(run.svc.state(), "stopped");
    assert!(srv.hits("/vote").is_empty(), "stop must not approve: {:?}", srv.hits("/vote"));
    assert!(!run.svc.is_alive());
    assert!(run.svc.pending().is_none());
    let res = run.results();
    assert_eq!(res[0]["status"], json!("SKIPPED (no reply / denied)"), "{res:?}");
    assert!(
        chrome_pids(&run.profile()).is_empty(),
        "Chrome still running after stop: {:?}",
        chrome_pids(&run.profile())
    );
}

#[test]
fn t7_browser_killed_mid_run_crashes_cleanly() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let run = Run::start("kill", &srv.url(), |_| {}, mc_ai("2"));
    run.wait_state("watching", 30);
    let pids = chrome_pids(&run.profile());
    assert!(!pids.is_empty(), "couldn't find the Chrome pid via pgrep");
    let t = Instant::now();
    for pid in &pids {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    run.wait_state("crashed", 40);
    println!("MEASURE kill -9 -> crashed event: {}", ms(t.elapsed()));
    let end = Instant::now() + Duration::from_secs(10);
    while run.svc.is_alive() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!run.svc.is_alive(), "watcher thread still alive after crash");
    assert_eq!(run.svc.state(), "crashed");
    let err = run.svc.last_error().expect("an error message");
    println!("crash error: {err}");
    let evs = run.drain();
    let crashed = evs
        .iter()
        .find(|e| e["event"] == "state" && e["state"] == "crashed")
        .unwrap();
    assert!(crashed["error"].as_str().is_some_and(|s| !s.is_empty()), "{crashed}");
    assert!(run.note_titles().contains(&"PollEv bot crashed".to_string()), "{:?}", run.note_titles());
    let t = Instant::now();
    assert!(run.svc.stop(Duration::from_secs(3)));
    assert!(t.elapsed() < Duration::from_secs(3));
}

#[test]
fn t8_same_question_id_answered_once() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let ai: AiFn = Arc::new(move |_p: &str, _s: &AtomicBool| {
        c.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"answer": "2", "confidence": 90, "reasoning": "stub"}).to_string())
    });
    let run = Run::start("dup", &srv.url(), |_| {}, Some((ai, ai_never())));
    run.wait_state("watching", 30);
    srv.set(&mc_page(80, json!(1), &["12", "2"]));
    srv.wait_hits("/vote", 1, 20);
    run.wait_result(1, 20);
    // The page is re-rendered (idle in between, then the same id again).
    std::thread::sleep(Duration::from_millis(300));
    srv.set(&mc_page(80, json!(1), &["12", "2"]));
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(srv.hits("/vote").len(), 1, "same id re-shown must not be answered again: {:?}", srv.hits("/vote"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    srv.set(IDLE);
    std::thread::sleep(Duration::from_millis(400));
    srv.set(&mc_page(80, json!(1), &["12", "2"]));
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(srv.hits("/vote").len(), 1, "idle in between must not reset the id");
    // A new id is answered.
    srv.set(&mc_page(81, json!(1), &["12", "2"]));
    let votes = srv.wait_hits("/vote", 2, 20);
    assert_eq!(votes, vec!["/vote?2".to_string(), "/vote?2".to_string()]);
    let res = run.wait_result(2, 20);
    assert_eq!(res[1]["question_id"], json!(81));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn t9_mid_session_wall_notifies_once_and_clears() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let run = Run::start("midwall", &srv.url(), |c| c.username = "abc123".into(), mc_ai("2"));
    run.wait_state("watching", 30);
    srv.set("<h1>Pre-registration required</h1><p>Log in to the Poll Everywhere account registered with Cornell to respond.</p>");
    run.wait("mid_session wall event", 20, |e| {
        e.iter().any(|x| x["event"] == "sso_wall" && x["phase"] == "mid_session")
    });
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        run.note_titles().iter().filter(|t| *t == "PollEv: login needed again").count(),
        1,
        "one notification per appearance: {:?}",
        run.note_titles()
    );
    srv.set(&mc_page(90, json!(1), &["12", "2"]));
    run.wait("cleared event", 20, |e| {
        e.iter().any(|x| x["event"] == "sso_wall" && x["phase"] == "cleared")
    });
    assert_eq!(srv.wait_hits("/vote", 1, 20), vec!["/vote?2".to_string()]);
}

const WALL_HTML: &str = "<h1>Pre-registration required</h1><p>Log in to the Poll Everywhere account registered with Cornell to respond.</p>\
    <a class='mdc-button' href='#' onclick=\"fetch('/login');return false\">Log in</a>";

/// PollEv is an SPA: the wall can render a moment after the page loads. That is
/// still a startup wall, not "login needed again", and not "watching" yet.
#[test]
fn t10_late_login_wall_is_a_startup_wall() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let run = Run::start("latewall", &srv.url(), |c| c.username = "abc123".into(), mc_ai("2"));
    std::thread::sleep(Duration::from_millis(900));
    srv.set(WALL_HTML);
    run.wait("the startup sso_wall event", 30, |e| {
        e.iter().any(|x| x["event"] == "sso_wall")
    });
    std::thread::sleep(Duration::from_millis(500));
    let all = run.drain();
    let walls: Vec<&Value> = all.iter().filter(|e| e["event"] == "sso_wall").collect();
    assert_eq!(walls, vec![&json!({"event": "sso_wall", "phase": "startup"})], "{all:?}");
    assert!(
        !all.iter().any(|e| e["event"] == "state" && e["state"] == "watching"),
        "must not report watching while the wall is up: {all:?}"
    );
    assert!(
        !run.note_titles().iter().any(|t| t.contains("login needed again")),
        "{:?}",
        run.note_titles()
    );
}

/// The wall check only applies when there is no live question: a question or
/// option that merely contains a wall phrase must still be answered.
#[test]
fn t11_wall_phrase_inside_a_question_does_not_block_it() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(&mc_page(110, json!(1), &["You must sign in", "Welcome"]));
    let run = Run::start("wallq", &srv.url(), |_| {}, mc_ai("Welcome"));
    assert_eq!(srv.wait_hits("/vote", 1, 30), vec!["/vote?Welcome".to_string()]);
    assert_eq!(run.wait_result(1, 20)[0]["status"], json!("SUBMITTED"));
    assert!(!run.note_titles().iter().any(|t| t.contains("login needed")));
}

/// Stop must interrupt the startup AI self-test, end as "stopped" (not a crash)
/// and never start Chrome.
#[test]
fn t12_stop_interrupts_startup_ai_test() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(IDLE);
    let slow: AiFn = Arc::new(|_p: &str, stop: &AtomicBool| {
        let end = Instant::now() + Duration::from_secs(20);
        while !stop.load(Ordering::SeqCst) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        Err(anyhow!("killed"))
    });
    let run = Run::start(
        "selftest_stop",
        &srv.url(),
        |c| c.ai_test_on_startup = true,
        Some((slow, ai_never())),
    );
    std::thread::sleep(Duration::from_millis(500));
    let t = Instant::now();
    assert!(run.svc.stop(Duration::from_secs(5)), "stop did not finish");
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    assert_eq!(run.svc.state(), "stopped");
    assert_eq!(run.svc.last_error(), None);
    assert!(!run.note_titles().iter().any(|t| t.contains("crashed")));
    assert!(chrome_pids(&run.profile()).is_empty());
}

/// A panic inside the watcher still has to text the user that it stopped.
#[test]
fn t13_panic_sends_crash_notification() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(&mc_page(130, json!(1), &["12", "2"]));
    let boom: AiFn = Arc::new(|_p: &str, _s: &AtomicBool| panic!("boom"));
    let run = Run::start("panic", &srv.url(), |_| {}, Some((boom, ai_never())));
    run.wait_state("crashed", 30);
    assert!(
        run.note_titles().iter().any(|t| t == "PollEv bot crashed"),
        "{:?}",
        run.note_titles()
    );
    assert!(run.svc.last_error().is_some_and(|e| e.contains("panicked")));
}

/// Stop pressed while the "answered (auto)" notification is being sent must
/// win over the submit that follows it.
#[test]
fn t14_stop_during_auto_notification_skips_submit() {
    if !chrome_or_skip() {
        return;
    }
    let srv = Srv::start(&mc_page(140, json!(1), &["12", "2"]));
    let dir = std::env::temp_dir().join(format!("pollev_e2e_autostop_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let queue = Arc::new(EventQueue::new(1000));
    let svc = WatcherService::new(queue.clone());
    let c = AppConfig {
        poll_url: srv.url(),
        headless: true,
        geolocation_enabled: false,
        ai_enabled: true,
        ai_require_confirmation: false,
        ai_test_on_startup: false,
        notify_imessage: false,
        notify_ntfy: false,
        ..AppConfig::default()
    };
    let in_notify = Arc::new(AtomicBool::new(false));
    let flag = in_notify.clone();
    let notify: NotifyFn = Arc::new(move |t: &str, _m: &str, _u: &str| {
        if t.starts_with("PollEv: answered") {
            flag.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(1500));
        }
    });
    let paths = Paths {
        profile_dir: dir.join("profile"),
        logs_dir: dir.join("logs"),
    };
    svc.start_with(&c, paths, mc_ai("2"), notify).unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    while !in_notify.load(Ordering::SeqCst) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(in_notify.load(Ordering::SeqCst), "never reached the answered notification");
    assert!(svc.stop(Duration::from_secs(10)));
    assert!(srv.hits("/vote").is_empty(), "voted after Stop: {:?}", srv.hits("/vote"));
    for pid in chrome_pids(&dir.join("profile").to_string_lossy()) {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    let _ = std::fs::remove_dir_all(&dir);
}
