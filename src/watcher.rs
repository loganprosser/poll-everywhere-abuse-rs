//! The watch loop (ai_answer.run + answer_with_ai) and the GUI-facing
//! service around it (watcher_service.py). See PORT_SPEC.md "watcher".
use crate::ai::{self, AiFn};
use crate::cdp::{Browser, LaunchOptions};
use crate::config::AppConfig;
use crate::notify::{self, Notifier, NotifyFn, SentTexts};
use crate::poll::{self, PageSetup, Question, SubmitError};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::hash::{BuildHasher, RandomState};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Structured progress events for the UI, same JSON shapes as the Python
/// app ({"event": "state"|"sso_wall"|"reasoning"|"awaiting_reply"|"result"|"log", ...}).
pub type EventFn = Arc<dyn Fn(Value) + Send + Sync>;
/// (question, answer, confidence, reasoning) -> approved?
pub type ConfirmFn =
    Arc<dyn Fn(&Question, &Value, Option<i64>, &str) -> Result<bool> + Send + Sync>;
/// (question, answer, confidence, reasoning) -> reply text, None = skip.
pub type ReplyFn =
    Arc<dyn Fn(&Question, &Value, Option<i64>, &str) -> Result<Option<String>> + Send + Sync>;
/// (image path, caption): best-effort "picture of the selection" text.
pub type SendImageFn = Arc<dyn Fn(&Path, &str) + Send + Sync>;

pub struct RunOptions {
    pub poll_url: String,
    pub username: String,
    pub headless: bool,
    /// Some((lat, lon, accuracy_m)) = fake position; None = deny.
    pub geo: Option<(f64, f64, f64)>,
    pub profile_dir: PathBuf,
    pub logs_dir: PathBuf,
    /// None = detect + notify only, nothing is ever submitted.
    pub ai: Option<AiFn>,
    /// Click questions; None = use `ai`.
    pub click_ai: Option<AiFn>,
    pub require_confirmation: bool,
    pub confirm: ConfirmFn,
    pub reply: ReplyFn,
    pub notify: NotifyFn,
    pub send_image: Option<SendImageFn>,
    pub on_event: EventFn,
    pub stop: Arc<AtomicBool>,
    /// How often the loop looks at the page (default 150ms).
    pub poll_interval: Duration,
    /// (min, max) hold after detection before submitting; see hold_answer.
    pub answer_delay: (Duration, Duration),
}

/// Pages navigate mid-tick, so a failed tick is normal; this many in a row
/// means the page or browser is really gone.
const MAX_CONSECUTIVE_ERRORS: u32 = 40;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicked holder must not take the UI down with it.
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Print + append to logs_dir/ai_answer.log. Opens the file per call: one
/// line per notable event, not a hot loop. Best effort, like the Python log().
fn log(logs_dir: &Path, msg: &str) {
    let line = format!("[{}] {msg}", poll::now_ts());
    eprintln!("{line}");
    let _ = std::fs::create_dir_all(logs_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs_dir.join("ai_answer.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// An on_event failure (a panicking UI callback) is logged and swallowed,
/// never fatal.
fn emit(opts: &RunOptions, event: Value) {
    if catch_unwind(AssertUnwindSafe(|| (opts.on_event)(event))).is_err() {
        log(
            &opts.logs_dir,
            "[ai_answer] on_event callback failed (non-fatal): panic",
        );
    }
}

fn state_event(state: &str) -> Value {
    json!({"event": "state", "state": state})
}

/// Python's repr() of a str: single quotes unless the text has a ' and no ",
/// control characters escaped.
fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python's repr() of a JSON-ish value, close enough for status text.
fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => py_str_repr(s),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        other => other.to_string(),
    }
}

/// Python's str() of an answer: strings bare, lists as a Python list.
fn py_display(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// Blocks until `stop` is set (Ok, after emitting state "stopped") or a
/// fatal error (Err, after emitting state "crashed" and a crash notification).
pub fn run(opts: RunOptions) -> Result<()> {
    emit(&opts, state_event("starting"));
    // A panic must still reach the crash path (and the crash notification).
    let res = catch_unwind(AssertUnwindSafe(|| watch(&opts))).unwrap_or_else(|p| {
        let m = p
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".into());
        Err(anyhow!("the watcher thread panicked: {m}"))
    });
    match res {
        Ok(()) => {
            log(
                &opts.logs_dir,
                "[ai_answer] stopped cleanly (stop requested)",
            );
            emit(&opts, state_event("stopped"));
            Ok(())
        }
        Err(e) => {
            let msg = format!("{e:#}");
            // Stop during launch/navigation can't interrupt them; their failure
            // after the user asked to stop is not a crash worth a push alert.
            if opts.stop.load(Ordering::SeqCst) {
                log(
                    &opts.logs_dir,
                    &format!("[ai_answer] stopped (stop requested; ignoring error: {msg})"),
                );
                emit(&opts, state_event("stopped"));
                return Ok(());
            }
            // Anything that escapes would otherwise die silently; text it too,
            // since the point is running this unattended during class.
            log(&opts.logs_dir, &format!("[ai_answer] CRASHED: {msg}"));
            emit(
                &opts,
                json!({"event": "state", "state": "crashed", "error": msg}),
            );
            (opts.notify)(
                "PollEv bot crashed",
                &format!("{msg}\nIt has stopped watching -- go answer manually, then restart it."),
                &opts.poll_url,
            );
            Err(e)
        }
    }
}

/// Per-run loop state.
struct Watch {
    setup: PageSetup,
    watching: bool,
    /// The wall is up (or we're off pollev.com signing in to get past it).
    wall_up: bool,
    /// The wall came back after "watching"; a "cleared" event is owed.
    mid_session: bool,
    startup_wall_sent: bool,
    question_seen: bool,
    /// When we first saw pollev.com. The SPA shell is on pollev.com before a
    /// login wall renders, so "watching" waits for a question or WALL_GRACE.
    on_pollev_since: Option<Instant>,
}

/// Python polled for the wall for 5s after load; same settle time here.
const WALL_GRACE: Duration = Duration::from_secs(5);

impl Watch {
    /// One look at the page: setup actions, wall bookkeeping, then the live
    /// question (None while a wall or a human-only step is showing).
    fn step(&mut self, browser: &Browser, opts: &RunOptions) -> Result<Option<Question>> {
        let page = browser.page();
        let st = self.setup.tick(page, &opts.username, self.question_seen)?;
        if st.wall {
            if !self.wall_up {
                self.wall_up = true;
                if self.watching {
                    // Session expiry (or a later question re-requiring registration)
                    // otherwise just looks like "no question" forever (confirmed live).
                    log(
                        &opts.logs_dir,
                        "[ai_answer] SSO wall reappeared mid-session -- re-authenticating",
                    );
                    self.mid_session = true;
                    emit(opts, json!({"event": "sso_wall", "phase": "mid_session"}));
                    let url = page.url().unwrap_or_else(|_| opts.poll_url.clone());
                    (opts.notify)(
                        "PollEv: login needed again",
                        "The registration wall came back mid-session -- attempting to log back in automatically.",
                        &url,
                    );
                } else if !self.startup_wall_sent {
                    self.startup_wall_sent = true;
                    emit(opts, json!({"event": "sso_wall", "phase": "startup"}));
                }
            }
            return Ok(None);
        }
        // Off pollev.com we're mid-login: the wall isn't "cleared" until we're back.
        if self.wall_up && st.on_pollev {
            self.wall_up = false;
            if self.mid_session {
                self.mid_session = false;
                log(
                    &opts.logs_dir,
                    "[ai_answer] re-authentication finished, resuming detection",
                );
                emit(opts, json!({"event": "sso_wall", "phase": "cleared"}));
            }
        }
        if st.awaiting_human {
            return Ok(None);
        }
        let q = poll::get_current_question(page)?;
        self.question_seen |= q.is_some();
        if st.on_pollev && self.on_pollev_since.is_none() {
            self.on_pollev_since = Some(Instant::now());
        }
        if !self.watching
            && (q.is_some() || self.on_pollev_since.is_some_and(|t| t.elapsed() >= WALL_GRACE))
        {
            self.watching = true;
            emit(opts, state_event("watching"));
            log(
                &opts.logs_dir,
                &format!(
                    "[ai_answer] watching {} (require_confirmation={})",
                    opts.poll_url,
                    if opts.require_confirmation {
                        "True"
                    } else {
                        "False"
                    }
                ),
            );
        }
        Ok(q)
    }
}

fn watch(opts: &RunOptions) -> Result<()> {
    let _ = std::fs::create_dir_all(&opts.logs_dir);
    let browser = Browser::launch(&LaunchOptions {
        headless: opts.headless,
        profile_dir: opts.profile_dir.clone(),
        chrome_path: None,
    })?;
    browser.set_geolocation(opts.geo)?;
    browser.page().navigate(&opts.poll_url)?;

    let mut w = Watch {
        setup: PageSetup::default(),
        watching: false,
        wall_up: false,
        mid_session: false,
        startup_wall_sent: false,
        question_seen: false,
        on_pollev_since: None,
    };
    let mut last_question_id: Option<i64> = None;
    let mut errors = 0u32;
    loop {
        if opts.stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        if !browser.is_alive() {
            bail!("the Chrome window was closed");
        }
        match w.step(&browser, opts) {
            Ok(q) => {
                errors = 0;
                // Same as Python: an id-less question never matches, so never fires.
                if let Some(q) = q.filter(|q| q.question_id != last_question_id) {
                    last_question_id = q.question_id;
                    log(
                        &opts.logs_dir,
                        &format!("[ai_answer] new question: {}", q.question),
                    );
                    let result = answer_question(&browser, &q, opts);
                    log(
                        &opts.logs_dir,
                        &format!("[ai_answer] {}", result["status"].as_str().unwrap_or("")),
                    );
                }
            }
            Err(e) => {
                errors += 1;
                log(
                    &opts.logs_dir,
                    &format!("[ai_answer] detection error ({errors}): {e:#}"),
                );
                if errors >= MAX_CONSECUTIVE_ERRORS {
                    bail!("the page stopped responding ({errors} failures in a row): {e:#}");
                }
            }
        }
        std::thread::sleep(opts.poll_interval);
    }
}

fn result_event(
    q: &Question,
    answer: &Value,
    confidence: Option<i64>,
    reasoning: &str,
    status: &str,
) -> Value {
    json!({
        "event": "result", "question_id": q.question_id, "type": q.qtype,
        "question": q.question, "answer": answer, "confidence": confidence,
        "reasoning": reasoning, "status": status,
    })
}

fn outcome(
    q: &Question,
    answer: Value,
    confidence: Option<i64>,
    reasoning: &str,
    status: &str,
) -> Value {
    json!({
        "question": serde_json::to_value(q).unwrap_or(Value::Null),
        "answer": answer, "confidence": confidence, "reasoning": reasoning, "status": status,
    })
}

struct AiOut {
    answer: Value,
    confidence: Option<i64>,
    reasoning: String,
    /// Click questions only: where the image lives and where we'd click.
    image: Option<(PathBuf, Vec<(f64, f64)>)>,
}

/// Uniform in [lo, hi]. std's randomly keyed hasher is plenty for jitter.
fn random_between((lo, hi): (Duration, Duration)) -> Duration {
    let bits = RandomState::new().hash_one(()) >> 11; // 53 bits -> [0, 1)
    lo + hi.saturating_sub(lo).mul_f64(bits as f64 / (1u64 << 53) as f64)
}

/// Whether a held answer should go in now: someone else has answered
/// (results_count rose since detection), or the question closed or changed
/// (submit then reports it, and the loop moves on to the next one).
fn turn_over(q: &Question, live: Option<&Question>) -> bool {
    match live {
        Some(l) if l.question_id == q.question_id => {
            matches!((q.results_count, l.results_count), (Some(then), Some(now)) if now > then)
        }
        _ => true,
    }
}

/// Keeps us from being the first vote in the instant a poll opens: waits
/// until `deadline`, or less if turn_over says go. Watches the page while it
/// waits, so a next question is not missed.
fn hold_answer(browser: &Browser, q: &Question, deadline: Instant, opts: &RunOptions) {
    let start = Instant::now();
    let mut why = "timer";
    while Instant::now() < deadline {
        if opts.stop.load(Ordering::SeqCst) {
            return;
        }
        // A failed look (page mid-navigation) just means keep waiting.
        if let Ok(live) = poll::get_current_question(browser.page())
            && turn_over(q, live.as_ref())
        {
            why = "someone else answered, or the question changed";
            break;
        }
        std::thread::sleep(opts.poll_interval);
    }
    log(
        &opts.logs_dir,
        &format!("[ai_answer] held the answer {:.1}s ({why})", start.elapsed().as_secs_f64()),
    );
}

/// The model step. Err is the final status text (FAILED ...).
fn ask_ai(
    q: &Question,
    filtered: &Question,
    opts: &RunOptions,
    ai: &AiFn,
) -> std::result::Result<AiOut, String> {
    let stop: &AtomicBool = &opts.stop;
    if ai::CLICK_TYPES.contains(&q.qtype.as_str()) {
        let Some(url) = q.image_url.as_deref().filter(|u| !u.is_empty()) else {
            return Err(
                "FAILED (bad AI response): clickable_image question has no image_url".into(),
            );
        };
        // Must land under logs_dir (the AI's cwd): Claude Code's sandbox denies
        // Read outside the project directory (confirmed live).
        let id = q.question_id.map_or("None".to_string(), |n| n.to_string());
        let path = opts.logs_dir.join("images").join(format!("{id}.png"));
        if !path.exists() {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            poll::download_image(url, &path)
                .map_err(|e| format!("FAILED (AI call errored): {e:#}"))?;
        }
        let call = opts.click_ai.as_ref().unwrap_or(ai);
        let raw = call(&ai::build_click_prompt(filtered, &path), stop)
            .map_err(|e| format!("FAILED (AI call errored): {e:#}"))?;
        let parsed =
            ai::parse_click_response(&raw).map_err(|e| format!("FAILED (bad AI response): {e}"))?;
        let answer = Value::Array(
            parsed
                .clicks
                .iter()
                .map(|(x, y)| json!({"x": x, "y": y}))
                .collect(),
        );
        Ok(AiOut {
            answer,
            confidence: parsed.confidence,
            reasoning: parsed.reasoning,
            image: Some((path, parsed.clicks)),
        })
    } else {
        let raw = ai(&ai::build_prompt(filtered), stop)
            .map_err(|e| format!("FAILED (AI call errored): {e:#}"))?;
        let parsed =
            ai::parse_ai_response(&raw).map_err(|e| format!("FAILED (bad AI response): {e}"))?;
        Ok(AiOut {
            answer: parsed.answer,
            confidence: parsed.confidence,
            reasoning: parsed.reasoning,
            image: None,
        })
    }
}

/// One question end to end. Never errors: every outcome is a status in the
/// returned {"question","answer","confidence","reasoning","status"} object
/// (ai_answer.answer_with_ai).
pub fn answer_question(browser: &Browser, q: &Question, opts: &RunOptions) -> Value {
    // Counted from detection, so time spent on the AI call is part of it.
    let deadline = Instant::now() + random_between(opts.answer_delay);
    let url = browser.page().url().unwrap_or_else(|_| q.url.clone());
    let notify = |title: &str, body: &str| (opts.notify)(title, body, &url);
    let qtype = q.qtype.as_str();
    let is_click = ai::CLICK_TYPES.contains(&qtype);
    let is_short = qtype == "short_answer";

    if let Err(e) = poll::save_question_json(&opts.logs_dir, q) {
        log(
            &opts.logs_dir,
            &format!("[ai_answer] couldn't save question json (non-fatal): {e:#}"),
        );
    }

    let Some(ai_fn) = opts.ai.as_ref() else {
        // Notify-only mode: an intentional state, not a FAILED.
        let mut summary = q.question.clone();
        if !q.options.is_empty() {
            let opts_text: Vec<&str> = q.options.iter().map(|o| o.text.as_str()).collect();
            summary += &format!(" | {}", opts_text.join(" / "));
        }
        let status = "DETECTED (answering disabled -- answer it yourself)";
        notify(
            "PollEv: question detected",
            &format!("{summary}\n(AI answering is off in settings.)"),
        );
        emit(opts, result_event(q, &Value::Null, None, "", status));
        return outcome(q, Value::Null, None, "", status);
    };

    notify(
        &format!("PollEv: we have a question ({qtype})"),
        "Reasoning about it now...",
    );

    if !ai::is_supported(qtype) {
        let status = format!(
            "SKIPPED (can't auto-answer type {} yet)",
            py_repr(&json!(qtype))
        );
        notify(
            "PollEv: unsupported question type",
            &format!(
                "Question: {}\nType: {qtype} isn't one I can auto-answer -- please answer it yourself.",
                q.question
            ),
        );
        return outcome(q, Value::Null, None, "", &status);
    }

    let (filtered, injection) = ai::filtered_question(q);
    let mut require_confirmation = opts.require_confirmation;
    if !injection.is_empty() {
        // A detected injection attempt never gets to auto-submit quietly.
        require_confirmation = true;
        let shown = py_repr(&Value::Array(injection.iter().map(|s| json!(s)).collect()));
        notify(
            "PollEv: possible prompt injection detected",
            &format!(
                "Filtered {} suspicious phrase(s) from this question before reasoning about it: {shown}\n\
                 Forcing a confirmation step regardless of settings.",
                injection.len()
            ),
        );
    }

    emit(
        opts,
        json!({"event": "reasoning", "question_id": q.question_id, "type": qtype, "question": q.question}),
    );

    let mut answer = Value::Null;
    let mut confidence: Option<i64> = None;
    let mut reasoning = String::new();
    let mut status: Option<String> = None;
    let mut image: Option<(PathBuf, Vec<(f64, f64)>)> = None;

    match ask_ai(q, &filtered, opts, ai_fn) {
        Ok(out) => {
            answer = out.answer;
            confidence = out.confidence;
            reasoning = out.reasoning;
            image = out.image;
        }
        // Stop kills the AI child (deliberate change); that error is not an AI
        // failure, so don't report it as one or text the user a false alarm.
        Err(_) if opts.stop.load(Ordering::SeqCst) => {
            let status = "SKIPPED (stopped)";
            emit(opts, result_event(q, &Value::Null, None, "", status));
            return outcome(q, Value::Null, None, "", status);
        }
        Err(s) => status = Some(s),
    }

    if status.is_none() {
        let confidence_str = confidence.map_or("unknown".to_string(), |c| format!("{c}/100"));
        let awaiting = |mode: &str| {
            json!({
                "event": "awaiting_reply", "question_id": q.question_id, "type": qtype,
                "question": q.question, "answer": answer, "confidence": confidence,
                "reasoning": reasoning, "mode": mode,
            })
        };
        if is_short {
            notify(
                "PollEv: suggested answer",
                &format!(
                    "Question: {}\nSuggested answer: {}\nConfidence: {confidence_str} | Reasoning: {reasoning}\n\n\
                     Reply \"lgtm\" to submit it as-is, or reply with your own answer to use that instead.",
                    q.question,
                    py_display(&answer)
                ),
            );
            emit(opts, awaiting("short_answer"));
            match (opts.reply)(q, &answer, confidence, &reasoning) {
                Err(e) => status = Some(format!("FAILED (reply step errored): {e:#}")),
                Ok(reply) => {
                    match ai::resolve_short_answer_reply(reply.as_deref(), &py_display(&answer)) {
                        None => status = Some("SKIPPED (no reply)".into()),
                        Some(resolved) => answer = Value::String(resolved),
                    }
                }
            }
        } else {
            // Short by design. The pause before submitting is hold_answer below,
            // which keeps watching the page so quick-succession polls aren't missed.
            let answer_summary = if is_click {
                let pts = image.as_ref().map(|(_, c)| c.as_slice()).unwrap_or(&[]);
                pts.iter()
                    .map(|(x, y)| format!("({x:.2},{y:.2})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                py_display(&answer)
            };
            notify(
                &format!(
                    "PollEv: answered{}",
                    if require_confirmation { "" } else { " (auto)" }
                ),
                &format!(
                    "Q: {}\nAnswer: {answer_summary}\nConfidence: {confidence_str}\n{}",
                    q.question,
                    if require_confirmation {
                        "Reply to submit it."
                    } else {
                        "Submitting now -- reply or change it yourself if this is wrong."
                    }
                ),
            );
            if require_confirmation {
                emit(opts, awaiting("confirm"));
                match (opts.confirm)(q, &answer, confidence, &reasoning) {
                    Err(e) => status = Some(format!("FAILED (confirm step errored): {e:#}")),
                    Ok(false) => status = Some("SKIPPED (no reply / denied)".into()),
                    Ok(true) => {}
                }
            }
        }

        if status.is_none() {
            hold_answer(browser, q, deadline, opts);
        }
        // Auto mode notifies synchronously above; Stop pressed meanwhile wins.
        if status.is_none() && opts.stop.load(Ordering::SeqCst) {
            status = Some("SKIPPED (stopped)".into());
        }
        if status.is_none() {
            status = Some(match poll::submit_answer(browser.page(), q, &answer) {
                Ok(_) => "SUBMITTED".to_string(),
                Err(SubmitError::Stale(e)) => {
                    format!("FAILED (question closed before we submitted): {e}")
                }
                Err(SubmitError::Answer(e)) => format!("FAILED (answer rejected): {e}"),
            });
            if status.as_deref() == Some("SUBMITTED")
                && let (Some(send), Some((path, clicks))) =
                    (opts.send_image.as_ref(), image.as_ref())
            {
                // Best-effort picture of the selection: never turns a real
                // SUBMITTED into a FAILED.
                let marked = path.with_file_name(format!(
                    "{}_marked.png",
                    path.file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default()
                ));
                match poll::render_marked_image(browser, path, clicks, &marked) {
                    Ok(()) => send(&marked, &format!("Clicked for: {} -- {url}", q.question)),
                    Err(e) => log(
                        &opts.logs_dir,
                        &format!("[ai_answer] couldn't send marked image (non-fatal): {e:#}"),
                    ),
                }
            }
        }
    }

    // Every path funnels here with a final status: emit it for the UI before
    // the human-facing notification.
    let status = status.unwrap_or_default();
    emit(
        opts,
        result_event(q, &answer, confidence, &reasoning, &status),
    );
    notify(
        &format!("PollEv: {}", status.split(' ').next().unwrap_or("")),
        &status,
    );
    outcome(q, answer, confidence, &reasoning, &status)
}

/// Bounded thread-safe queue the UI drains; drops the oldest when full.
pub struct EventQueue {
    max: usize,
    q: Mutex<VecDeque<Value>>,
}

impl EventQueue {
    pub fn new(max: usize) -> Self {
        EventQueue {
            max: max.max(1),
            q: Mutex::new(VecDeque::new()),
        }
    }

    pub fn put(&self, event: Value) {
        let mut q = lock(&self.q);
        // Oldest events are the stale ones: drop them, keep the newest.
        while q.len() >= self.max {
            q.pop_front();
        }
        q.push_back(event);
    }

    pub fn get_events(&self, max: usize) -> Vec<Value> {
        let mut q = lock(&self.q);
        let n = max.min(q.len());
        q.drain(..n).collect()
    }
}

/// Where a run keeps Chrome's profile and its logs (tests use temp dirs).
#[derive(Clone, Debug)]
pub struct Paths {
    pub profile_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            profile_dir: crate::config::chrome_profile_dir(),
            logs_dir: crate::config::logs_dir(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    /// MC / select / click: resolves with Approve.
    Confirm,
    ShortAnswer,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Confirm => "confirm",
            Mode::ShortAnswer => "short_answer",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Resolution {
    Approve(bool),
    /// Reply text; None = skip.
    Reply(Option<String>),
}

/// One pending confirm/short-answer decision. The watcher thread blocks in
/// wait() while the UI (or an iMessage reply) calls resolve(). First resolver
/// wins; later ones are ignored no-ops.
struct PendingDecision {
    question: Question,
    answer: Value,
    confidence: Option<i64>,
    mode: Mode,
    slot: Mutex<Option<(Resolution, String)>>,
    cv: Condvar,
}

impl PendingDecision {
    fn new(question: Question, answer: Value, confidence: Option<i64>, mode: Mode) -> Self {
        PendingDecision {
            question,
            answer,
            confidence,
            mode,
            slot: Mutex::new(None),
            cv: Condvar::new(),
        }
    }

    /// True if this call was the winner.
    fn resolve(&self, result: Resolution, by: &str) -> bool {
        let mut slot = lock(&self.slot);
        if slot.is_some() {
            return false;
        }
        *slot = Some((result, by.to_string()));
        self.cv.notify_all();
        true
    }

    /// What "no" means for this mode. Stop must never approve: a truthy
    /// value here would submit the pending answer (the Python bug).
    fn deny(&self) -> Resolution {
        match self.mode {
            Mode::Confirm => Resolution::Approve(false),
            Mode::ShortAnswer => Resolution::Reply(None),
        }
    }

    /// (result, resolved_by), or None on timeout.
    fn wait(&self, timeout: Duration) -> Option<(Resolution, String)> {
        let slot = lock(&self.slot);
        let (slot, _) = self
            .cv
            .wait_timeout_while(slot, timeout, |s| s.is_none())
            .unwrap_or_else(|e| e.into_inner());
        slot.clone()
    }
}

/// State shared between the service handle, the watcher thread and the
/// confirm/reply closures.
struct Shared {
    events: Arc<EventQueue>,
    state: Mutex<String>,
    pending: Mutex<Option<Arc<PendingDecision>>>,
    last_error: Mutex<Option<String>>,
    channels: Mutex<Vec<String>>,
    sent: SentTexts,
}

impl Shared {
    fn set_state(&self, state: &str, error: Option<&str>) {
        *lock(&self.state) = state.to_string();
        let mut ev = state_event(state);
        if let Some(e) = error {
            ev["error"] = json!(e);
        }
        self.events.put(ev);
    }

    /// Keep service state in sync with the run's own state events: the GUI
    /// polls state(), not just the queue, and without this it would stay
    /// "stopping" forever after a clean stop (caught live).
    fn on_event(&self, event: Value, stop: &AtomicBool) {
        if event["event"] == "state" {
            match event["state"].as_str() {
                // A "watching" that lands after Stop must not undo "stopping".
                Some("watching") if stop.load(Ordering::SeqCst) => {}
                Some(s @ ("watching" | "stopped" | "crashed")) => {
                    *lock(&self.state) = s.to_string()
                }
                _ => {}
            }
        }
        self.events.put(event);
    }

    fn set_pending(&self, p: Option<Arc<PendingDecision>>) {
        *lock(&self.pending) = p;
    }

    fn clear_pending(&self, p: &Arc<PendingDecision>) {
        let mut cur = lock(&self.pending);
        if cur.as_ref().is_some_and(|c| Arc::ptr_eq(c, p)) {
            *cur = None;
        }
    }

    /// Registers `pending`, announces it, and blocks until the first of: the
    /// UI's decision, an iMessage reply, or Stop.
    fn ask(
        &self,
        pending: Arc<PendingDecision>,
        reasoning: &str,
        channels: Vec<String>,
        imessage: Option<&str>,
        stop: &AtomicBool,
    ) -> Resolution {
        self.set_pending(Some(pending.clone()));
        self.events.put(json!({
            "event": "awaiting_reply", "mode": pending.mode.as_str(),
            "question_id": pending.question.question_id, "type": pending.question.qtype,
            "question": pending.question.question, "answer": pending.answer,
            "confidence": pending.confidence, "reasoning": reasoning,
            "channels": channels,
        }));
        // Fresh stop flag PER QUESTION: it ends this question's chat.db poller
        // when the in-app button wins. A shared one would stay set after the
        // first UI win and silently kill every later question's poller.
        let msg_stop = Arc::new(AtomicBool::new(false));
        let rx = imessage.map(|handle| {
            let (tx, rx) = mpsc::channel();
            let (handle, sent, st) = (handle.to_string(), self.sent.clone(), msg_stop.clone());
            std::thread::spawn(move || {
                let _ = tx.send(notify::wait_for_imessage_reply(
                    &handle,
                    &sent,
                    Duration::from_secs(2),
                    &st,
                ));
            });
            rx
        });
        let result = loop {
            if let Some((r, _by)) = pending.wait(Duration::from_millis(250)) {
                break r;
            }
            if stop.load(Ordering::SeqCst) {
                // Also covers Stop landing before this pending was registered.
                pending.resolve(pending.deny(), "stop");
                continue;
            }
            if let Some(rx) = &rx
                && let Ok(r) = rx.try_recv()
            {
                match r {
                    // Any reply approves a confirm; short answers need the text.
                    Ok(Some(text)) => {
                        let res = match pending.mode {
                            Mode::Confirm => Resolution::Approve(true),
                            Mode::ShortAnswer => Resolution::Reply(Some(text)),
                        };
                        // Loses harmlessly if the UI just won; next loop returns the winner.
                        pending.resolve(res, "imessage");
                    }
                    Ok(None) => {}
                    // A broken chat.db (no Full Disk Access) must not deny the
                    // question: the in-app buttons still work.
                    Err(e) => eprintln!("[watcher] iMessage reply channel failed: {e:#}"),
                }
            }
        };
        msg_stop.store(true, Ordering::SeqCst);
        self.clear_pending(&pending);
        result
    }
}

/// Owns one watcher run at a time (watcher_service.WatcherService).
pub struct WatcherService {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
    stop: Mutex<Arc<AtomicBool>>,
}

impl WatcherService {
    pub fn new(events: Arc<EventQueue>) -> Self {
        WatcherService {
            shared: Arc::new(Shared {
                events,
                state: Mutex::new("idle".into()),
                pending: Mutex::new(None),
                last_error: Mutex::new(None),
                channels: Mutex::new(Vec::new()),
                sent: SentTexts::default(),
            }),
            thread: Mutex::new(None),
            stop: Mutex::new(Arc::new(AtomicBool::new(false))),
        }
    }

    /// Real AI backends + real notifications from `cfg`. Err if already running.
    pub fn start(&self, cfg: &AppConfig) -> Result<()> {
        if self.is_alive() {
            bail!("watcher already running");
        }
        let paths = Paths::default();
        let notifier = Arc::new(Notifier {
            mac: cfg.notify_mac,
            sound: cfg.notify_sound,
            ntfy_topic: Some(cfg.ntfy_topic.trim().to_string())
                .filter(|t| cfg.notify_ntfy && !t.is_empty()),
            imessage: Some(cfg.imessage_recipient.trim().to_string())
                .filter(|r| cfg.notify_imessage && !r.is_empty()),
            sent: self.shared.sent.clone(),
        });
        let ai = if cfg.ai_enabled {
            // The CLI runs in logs_dir so it can Read the poll images saved there.
            std::fs::create_dir_all(&paths.logs_dir)?;
            let make = |schema: Value| {
                ai::make_ai_fn(
                    &cfg.ai_backend,
                    Some(cfg.ai_model.trim().to_string()),
                    Some(cfg.ai_effort.trim().to_string()),
                    schema,
                    paths.logs_dir.clone(),
                )
            };
            Some((make(ai::answer_schema())?, make(ai::click_schema())?))
        } else {
            None
        };
        let n = notifier.clone();
        let send_image: SendImageFn = Arc::new(move |p, caption| n.send_imessage_image(p, caption));
        self.spawn(cfg, paths, ai, notifier.into_fn(), Some(send_image))
    }

    /// Same as start, with injected AI (text, click) and notifications;
    /// confirm/reply still come from the service (in-app + iMessage, first
    /// wins). Does not validate cfg.
    pub fn start_with(
        &self,
        cfg: &AppConfig,
        paths: Paths,
        ai: Option<(AiFn, AiFn)>,
        notify: NotifyFn,
    ) -> Result<()> {
        self.spawn(cfg, paths, ai, notify, None)
    }

    fn spawn(
        &self,
        cfg: &AppConfig,
        paths: Paths,
        ai: Option<(AiFn, AiFn)>,
        notify: NotifyFn,
        send_image: Option<SendImageFn>,
    ) -> Result<()> {
        let mut th = lock(&self.thread);
        if th.as_ref().is_some_and(|h| !h.is_finished()) {
            bail!("watcher already running");
        }
        let stop = Arc::new(AtomicBool::new(false));
        *lock(&self.stop) = stop.clone();
        let shared = self.shared.clone();
        shared.set_pending(None);
        *lock(&shared.last_error) = None;
        lock(&shared.channels).clear();
        shared.set_state("starting", None);

        // ai disabled = detect + notify only; nothing is proposed or submitted.
        let ai = ai.filter(|_| cfg.ai_enabled);
        let recipient = Some(cfg.imessage_recipient.trim().to_string())
            .filter(|r| cfg.notify_imessage && !r.is_empty());
        if ai.is_some() && recipient.is_some() {
            lock(&shared.channels).push("imessage".into());
        }
        let confirm: ConfirmFn = {
            let (shared, stop, recipient) = (shared.clone(), stop.clone(), recipient.clone());
            Arc::new(move |q, answer, confidence, reasoning| {
                let p = Arc::new(PendingDecision::new(
                    q.clone(),
                    answer.clone(),
                    confidence,
                    Mode::Confirm,
                ));
                let channels = lock(&shared.channels).clone();
                let r = shared.ask(p, reasoning, channels, recipient.as_deref(), &stop);
                Ok(r == Resolution::Approve(true))
            })
        };
        let reply: ReplyFn = {
            let (shared, stop, recipient) = (shared.clone(), stop.clone(), recipient.clone());
            Arc::new(move |q, answer, confidence, reasoning| {
                let p = Arc::new(PendingDecision::new(
                    q.clone(),
                    answer.clone(),
                    confidence,
                    Mode::ShortAnswer,
                ));
                let mut channels = lock(&shared.channels).clone();
                if channels.is_empty() {
                    channels.push("app".into());
                }
                let r = shared.ask(p, reasoning, channels, recipient.as_deref(), &stop);
                Ok(match r {
                    Resolution::Reply(t) => t,
                    Resolution::Approve(_) => None,
                })
            })
        };
        let on_event: EventFn = {
            let (shared, stop) = (shared.clone(), stop.clone());
            Arc::new(move |ev| shared.on_event(ev, &stop))
        };
        let startup_test = (cfg.ai_test_on_startup && ai.is_some())
            .then(|| ai.as_ref().map(|a| a.0.clone()))
            .flatten();
        let test_stop = stop.clone();
        let opts = RunOptions {
            poll_url: cfg.poll_url.trim().to_string(),
            username: cfg.username.trim().to_string(),
            headless: cfg.headless,
            geo: cfg
                .geolocation_enabled
                .then_some((cfg.geo_lat, cfg.geo_lon, cfg.geo_accuracy)),
            profile_dir: paths.profile_dir,
            logs_dir: paths.logs_dir,
            require_confirmation: ai.is_some() && cfg.ai_require_confirmation,
            click_ai: ai.as_ref().map(|a| a.1.clone()),
            ai: ai.map(|a| a.0),
            confirm,
            reply,
            notify,
            send_image,
            on_event,
            stop,
            poll_interval: Duration::from_millis(150),
            // A hand-edited bad value (negative, NaN) means no hold, not a panic.
            answer_delay: (
                Duration::try_from_secs_f64(cfg.answer_delay_secs.0).unwrap_or_default(),
                Duration::try_from_secs_f64(cfg.answer_delay_secs.1).unwrap_or_default(),
            ),
        };
        let handle = std::thread::Builder::new()
            .name("pollev-watcher".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                    if let Some(f) = &startup_test {
                        shared
                            .events
                            .put(json!({"event": "log", "message": "Testing AI backend..."}));
                        let (ok, detail) = ai::test_backend(f, &test_stop);
                        if test_stop.load(Ordering::SeqCst) {
                            return Ok(()); // Stop pressed during the self-test
                        }
                        if !ok {
                            bail!("AI backend startup test failed: {detail}");
                        }
                    }
                    run(opts)
                }));
                let err = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:#}")),
                    Err(_) => Some("the watcher thread panicked".to_string()),
                };
                if let Some(msg) = err {
                    *lock(&shared.last_error) = Some(msg.clone());
                    // run() already announced its own crash; don't repeat it.
                    if *lock(&shared.state) != "crashed" {
                        shared.set_state("crashed", Some(&msg));
                    }
                }
            });
        match handle {
            Ok(h) => {
                *th = Some(h);
                Ok(())
            }
            Err(e) => {
                let e = anyhow!("couldn't start the watcher thread: {e}");
                self.shared.set_state("crashed", Some(&e.to_string()));
                Err(e)
            }
        }
    }

    /// Set stop, deny/skip any pending decision, wait up to `wait` for the
    /// thread. True if it has finished.
    pub fn stop(&self, wait: Duration) -> bool {
        if !self.is_alive() {
            return true;
        }
        self.shared.set_state("stopping", None);
        lock(&self.stop).store(true, Ordering::SeqCst);
        // Unblock a pending confirm/short-answer wait so the thread can exit.
        // Deny, never approve: Stop must not submit the pending answer.
        let pending = lock(&self.shared.pending).clone();
        if let Some(p) = pending {
            p.resolve(p.deny(), "stop");
        }
        let deadline = Instant::now() + wait;
        while self.is_alive() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.is_alive() {
            return false;
        }
        if let Some(h) = lock(&self.thread).take() {
            let _ = h.join();
        }
        // The thread may have ended just before "stopping" was set, so its own
        // final state event came first and would leave "stopping" forever.
        if self.state() == "stopping" {
            self.shared.set_state("stopped", None);
        }
        true
    }

    /// idle|starting|watching|stopping|stopped|crashed
    pub fn state(&self) -> String {
        lock(&self.shared.state).clone()
    }

    pub fn is_alive(&self) -> bool {
        lock(&self.thread)
            .as_ref()
            .is_some_and(|h| !h.is_finished())
    }

    pub fn last_error(&self) -> Option<String> {
        lock(&self.shared.last_error).clone()
    }

    /// {"mode","question","answer","confidence"} of the pending decision.
    pub fn pending(&self) -> Option<Value> {
        let p = lock(&self.shared.pending).clone()?;
        Some(json!({
            "mode": p.mode.as_str(), "question": p.question.question,
            "answer": p.answer, "confidence": p.confidence,
        }))
    }

    /// The UI's Accept / Skip / Send. {"resolved": bool, "reason"?: str}
    pub fn decide(&self, decision: &str, text: &str) -> Value {
        let Some(p) = lock(&self.shared.pending).clone() else {
            return json!({"resolved": false, "reason": "nothing pending"});
        };
        let res = match (p.mode, decision) {
            (Mode::Confirm, "accept") => Resolution::Approve(true),
            (Mode::Confirm, "skip") => Resolution::Approve(false),
            // Accept = the AI's suggestion as-is. "lgtm" is an approval, so
            // resolve_short_answer_reply keeps the AI text verbatim (passing the
            // text itself would strip a leading "use"/"change to").
            (Mode::ShortAnswer, "accept") => Resolution::Reply(Some("lgtm".to_string())),
            (Mode::ShortAnswer, "send") => Resolution::Reply(Some(text.to_string())),
            (Mode::ShortAnswer, "skip") => Resolution::Reply(None),
            (mode, _) => {
                return json!({
                    "resolved": false,
                    "reason": format!("unknown decision {} for {} mode", py_repr(&json!(decision)), mode.as_str()),
                });
            }
        };
        json!({"resolved": p.resolve(res, "app")})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::find_chrome;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn question(id: i64) -> Question {
        Question {
            question_id: Some(id),
            poll_key: None,
            timestamp: "20260101_000000".into(),
            url: "http://x/".into(),
            qtype: "multiple_choice".into(),
            question: "Pick".into(),
            options: vec![],
            image_url: None,
            answer: Value::Null,
            source: "dom".into(),
            results_count: None,
        }
    }

    #[test]
    fn hold_ends_on_new_response_or_question_change() {
        let at = |id, n| Question { results_count: n, ..question(id) };
        assert!(!turn_over(&at(1, Some(3)), Some(&at(1, Some(3)))), "no one new yet");
        assert!(turn_over(&at(1, Some(3)), Some(&at(1, Some(4)))), "someone else answered");
        assert!(!turn_over(&at(1, None), Some(&at(1, Some(4)))), "no baseline, no early go");
        assert!(turn_over(&at(1, Some(3)), Some(&at(2, Some(0)))), "question changed");
        assert!(turn_over(&at(1, Some(3)), None), "question closed");

        let (lo, hi) = (Duration::from_secs_f64(13.8), Duration::from_secs_f64(20.4));
        let picks: Vec<Duration> = (0..200).map(|_| random_between((lo, hi))).collect();
        assert!(picks.iter().all(|d| (lo..=hi).contains(d)));
        assert!(picks.iter().any(|d| *d != picks[0]), "not random");
        assert_eq!(random_between((hi, lo)), hi, "inverted range is just its min");
    }

    #[test]
    fn event_queue_drops_oldest() {
        let q = EventQueue::new(3);
        for i in 0..5 {
            q.put(json!({"n": i}));
        }
        let got: Vec<i64> = q
            .get_events(10)
            .iter()
            .map(|e| e["n"].as_i64().unwrap())
            .collect();
        assert_eq!(got, vec![2, 3, 4]);
        assert!(q.get_events(10).is_empty());
        q.put(json!({"n": 9}));
        q.put(json!({"n": 10}));
        assert_eq!(q.get_events(1).len(), 1);
        assert_eq!(q.get_events(10).len(), 1);
    }

    #[test]
    fn pending_decision_first_resolver_wins() {
        let p = PendingDecision::new(question(1), json!("A"), Some(80), Mode::Confirm);
        assert_eq!(p.wait(Duration::from_millis(10)), None);
        assert!(p.resolve(Resolution::Approve(true), "app"));
        assert!(!p.resolve(Resolution::Approve(false), "imessage"));
        assert_eq!(
            p.wait(Duration::from_millis(10)),
            Some((Resolution::Approve(true), "app".into()))
        );
        // Stop's denial is never truthy.
        assert_eq!(p.deny(), Resolution::Approve(false));
        let s = PendingDecision::new(question(1), json!("A"), None, Mode::ShortAnswer);
        assert_eq!(s.deny(), Resolution::Reply(None));
    }

    #[test]
    fn decide_maps_decisions_and_first_wins() {
        let svc = WatcherService::new(Arc::new(EventQueue::new(10)));
        assert_eq!(
            svc.decide("accept", ""),
            json!({"resolved": false, "reason": "nothing pending"})
        );
        let p = Arc::new(PendingDecision::new(
            question(1),
            json!("A"),
            None,
            Mode::Confirm,
        ));
        svc.shared.set_pending(Some(p.clone()));
        assert_eq!(svc.pending().unwrap()["mode"], json!("confirm"));
        let r = svc.decide("send", "x");
        assert_eq!(r["resolved"], json!(false));
        assert_eq!(
            r["reason"],
            json!("unknown decision 'send' for confirm mode")
        );
        assert_eq!(svc.decide("skip", ""), json!({"resolved": true}));
        assert_eq!(svc.decide("accept", ""), json!({"resolved": false}));
        assert_eq!(
            p.wait(Duration::ZERO).unwrap().0,
            Resolution::Approve(false)
        );
        let s = Arc::new(PendingDecision::new(
            question(1),
            json!("AI"),
            None,
            Mode::ShortAnswer,
        ));
        svc.shared.set_pending(Some(s.clone()));
        assert_eq!(svc.decide("accept", ""), json!({"resolved": true}));
        assert_eq!(
            s.wait(Duration::ZERO).unwrap().0,
            // "lgtm" so the AI text is submitted verbatim (see decide()).
            Resolution::Reply(Some("lgtm".into()))
        );
        // Accept must not mangle an answer that starts with an override word.
        assert_eq!(
            ai::resolve_short_answer_reply(Some("lgtm"), "Use cases"),
            Some("Use cases".into())
        );
    }

    #[test]
    fn python_style_display() {
        assert_eq!(py_display(&json!("a")), "a");
        // Python: repr("it's") is "it's"; both quote kinds -> single quotes, escaped.
        assert_eq!(py_display(&json!(["a", "it's"])), "['a', \"it's\"]");
        assert_eq!(py_repr(&json!("a'b\"c")), "'a\\'b\"c'");
        assert_eq!(py_repr(&json!("l1\nl2\\")), "'l1\\nl2\\\\'");
        assert_eq!(py_repr(&json!(null)), "None");
    }

    // ---- end to end against a fake PollEv page on 127.0.0.1 ----

    /// Serves the poll page at "/" and records "/vote?<text>" hits.
    fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
        let activity = json!({
            "id": 77, "shared_key": "k", "type": "multiple_choice_poll", "title": "Pick one",
            "max_votes": 1,
            "options": [{"id": 1, "value": "12"}, {"id": 2, "value": "2"}],
        });
        let vote = "component-response-multiple-choice__option__vote";
        let onclick = "fetch('/vote?' + encodeURIComponent(this.innerText.trim()))";
        let page = format!(
            "<html><body><script>window.legacyActivityResponseView = {{setup() {{}}}};\n\
             window.legacyActivityResponseView.setup({{activity: {activity}}});</script>\
             <button class='{vote}' onclick=\"{onclick}\">12</button>\
             <button class='{vote}' onclick=\"{onclick}\">2</button></body></html>"
        );
        let votes = Arc::new(Mutex::new(Vec::new()));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let seen = votes.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (page, seen) = (page.clone(), seen.clone());
                std::thread::spawn(move || {
                    let mut s = stream;
                    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 4096];
                    let n = s.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, body) = if let Some(v) = path.strip_prefix("/vote?") {
                        seen.lock().unwrap().push(percent_decode(v));
                        ("200 OK", String::new())
                    } else if path == "/" {
                        ("200 OK", page)
                    } else {
                        ("404 Not Found", String::new())
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(body.as_bytes());
                });
            }
        });
        (url, votes)
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

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pollev_watcher_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn stub_ai() -> AiFn {
        Arc::new(|_prompt: &str, _stop: &AtomicBool| {
            Ok(r#"{"answer":"2","confidence":90,"reasoning":"stub"}"#.to_string())
        })
    }

    fn wait_until(what: &str, secs: u64, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }

    fn notes() -> (NotifyFn, Arc<Mutex<Vec<(String, String)>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = log.clone();
        (
            Arc::new(move |t: &str, m: &str, _u: &str| {
                l.lock().unwrap().push((t.into(), m.into()))
            }),
            log,
        )
    }

    #[test]
    fn e2e_run_submits_the_exact_option() {
        if find_chrome().is_none() {
            eprintln!("skipping: no Chrome-family browser");
            return;
        }
        let (url, votes) = serve();
        let dir = temp_dir("run");
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (notify, notes) = notes();
        let ev = events.clone();
        let opts = RunOptions {
            poll_url: url,
            username: String::new(),
            headless: true,
            geo: None,
            profile_dir: dir.join("profile"),
            logs_dir: dir.join("logs"),
            ai: Some(stub_ai()),
            click_ai: None,
            require_confirmation: false,
            confirm: Arc::new(|_, _, _, _| Ok(true)),
            reply: Arc::new(|_, _, _, _| Ok(None)),
            notify,
            send_image: None,
            on_event: Arc::new(move |e| ev.lock().unwrap().push(e)),
            stop: stop.clone(),
            poll_interval: Duration::from_millis(100),
            answer_delay: (Duration::ZERO, Duration::ZERO),
        };
        let h = std::thread::spawn(move || run(opts));
        let has_result = |events: &Mutex<Vec<Value>>| {
            events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e["event"] == "result")
        };
        wait_until("a result event", 60, || {
            has_result(&events) || h.is_finished()
        });
        wait_until("the vote to reach the server", 5, || {
            !votes.lock().unwrap().is_empty() || h.is_finished()
        });
        stop.store(true, Ordering::SeqCst);
        let r = h.join().unwrap();
        assert!(r.is_ok(), "{r:?}");

        let evs = events.lock().unwrap().clone();
        let result = evs
            .iter()
            .find(|e| e["event"] == "result")
            .expect("result event");
        assert_eq!(result["status"], json!("SUBMITTED"), "{evs:?}");
        assert_eq!(result["answer"], json!("2"));
        assert_eq!(result["question_id"], json!(77));
        assert_eq!(*votes.lock().unwrap(), vec!["2".to_string()]);
        let states: Vec<&str> = evs
            .iter()
            .filter(|e| e["event"] == "state")
            .map(|e| e["state"].as_str().unwrap())
            .collect();
        assert_eq!(states, vec!["starting", "watching", "stopped"], "{evs:?}");
        assert_eq!(
            evs.last().unwrap(),
            &json!({"event": "state", "state": "stopped"})
        );
        let titles: Vec<String> = notes.lock().unwrap().iter().map(|n| n.0.clone()).collect();
        assert_eq!(
            titles,
            vec![
                "PollEv: we have a question (multiple_choice)",
                "PollEv: answered (auto)",
                "PollEv: SUBMITTED"
            ]
        );
        assert!(dir.join("logs/questions/index.jsonl").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn e2e_service_stop_denies_pending_confirmation() {
        if find_chrome().is_none() {
            eprintln!("skipping: no Chrome-family browser");
            return;
        }
        let (url, votes) = serve();
        let dir = temp_dir("svc");
        let queue = Arc::new(EventQueue::new(500));
        let svc = WatcherService::new(queue.clone());
        let cfg = AppConfig {
            poll_url: url,
            headless: true,
            geolocation_enabled: false,
            ai_enabled: true,
            ai_require_confirmation: true,
            ai_test_on_startup: false,
            notify_imessage: false,
            answer_delay_secs: (0.0, 0.0),
            ..AppConfig::default()
        };
        let (notify, _notes) = notes();
        let paths = Paths {
            profile_dir: dir.join("profile"),
            logs_dir: dir.join("logs"),
        };
        svc.start_with(&cfg, paths, Some((stub_ai(), stub_ai())), notify)
            .unwrap();
        assert!(
            svc.start_with(
                &cfg,
                Paths {
                    profile_dir: dir.join("profile2"),
                    logs_dir: dir.join("logs2"),
                },
                None,
                Arc::new(|_, _, _| {})
            )
            .is_err()
        );

        let mut all: Vec<Value> = Vec::new();
        wait_until("awaiting_reply", 60, || {
            all.extend(queue.get_events(100));
            all.iter()
                .any(|e| e["event"] == "awaiting_reply" && e.get("channels").is_some())
                && svc.pending().is_some()
        });
        assert_eq!(svc.pending().unwrap()["mode"], json!("confirm"));
        assert!(svc.stop(Duration::from_secs(30)));
        all.extend(queue.get_events(500));

        assert!(
            votes.lock().unwrap().is_empty(),
            "stop must not approve: {:?}",
            votes.lock().unwrap()
        );
        assert_eq!(svc.state(), "stopped");
        assert!(!svc.is_alive());
        assert!(svc.pending().is_none());
        let result = all
            .iter()
            .find(|e| e["event"] == "result")
            .expect("result event");
        assert_eq!(result["status"], json!("SKIPPED (no reply / denied)"));
        assert_eq!(
            all.last().unwrap(),
            &json!({"event": "state", "state": "stopped"}),
            "{all:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ask_denies_when_stop_is_already_set() {
        // Stop landing before the pending decision is registered must not hang.
        for (mode, want) in [
            (Mode::Confirm, Resolution::Approve(false)),
            (Mode::ShortAnswer, Resolution::Reply(None)),
        ] {
            let svc = WatcherService::new(Arc::new(EventQueue::new(10)));
            let p = Arc::new(PendingDecision::new(question(5), json!("A"), Some(7), mode));
            let stop = AtomicBool::new(true);
            let got = svc.shared.ask(p, "why", vec!["app".into()], None, &stop);
            assert_eq!(got, want);
            assert!(svc.pending().is_none(), "pending must be cleared");
            let ev = svc.shared.events.get_events(10);
            assert_eq!(
                ev,
                vec![json!({
                    "event": "awaiting_reply", "mode": mode.as_str(), "question_id": 5,
                    "type": "multiple_choice", "question": "Pick", "answer": "A",
                    "confidence": 7, "reasoning": "why", "channels": ["app"],
                })]
            );
        }
    }

    #[test]
    fn stop_after_thread_already_ended_does_not_stick_on_stopping() {
        let svc = WatcherService::new(Arc::new(EventQueue::new(10)));
        // A thread that is still alive when stop() looks, ends without ever
        // emitting its own "stopped" (it already did, in the real race).
        *lock(&svc.thread) = Some(std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100))
        }));
        assert!(svc.stop(Duration::from_secs(5)));
        assert_eq!(svc.state(), "stopped");
        assert!(!svc.is_alive());
    }

    #[test]
    fn failed_startup_test_ends_crashed_without_a_browser() {
        let dir = temp_dir("startup");
        let queue = Arc::new(EventQueue::new(50));
        let svc = WatcherService::new(queue.clone());
        let cfg = AppConfig {
            poll_url: "http://127.0.0.1:1/".into(),
            ai_enabled: true,
            ai_test_on_startup: true,
            notify_imessage: false,
            answer_delay_secs: (0.0, 0.0),
            ..AppConfig::default()
        };
        let bad: AiFn = Arc::new(|_: &str, _: &AtomicBool| Err(anyhow!("backend is down")));
        let (notify, _n) = notes();
        let paths = Paths {
            profile_dir: dir.join("profile"),
            logs_dir: dir.join("logs"),
        };
        svc.start_with(&cfg, paths, Some((bad.clone(), bad)), notify)
            .unwrap();
        wait_until("thread end", 10, || !svc.is_alive());
        assert_eq!(svc.state(), "crashed");
        let err = svc.last_error().unwrap();
        assert_eq!(err, "AI backend startup test failed: backend is down");
        let ev = queue.get_events(50);
        assert!(ev.contains(&json!({"event": "log", "message": "Testing AI backend..."})));
        assert_eq!(
            ev.last().unwrap(),
            &json!({"event": "state", "state": "crashed", "error": err})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stop_during_ai_call_is_skipped_not_failed() {
        if find_chrome().is_none() {
            eprintln!("skipping: no Chrome-family browser");
            return;
        }
        let (url, votes) = serve();
        let dir = temp_dir("stopai");
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let called = Arc::new(AtomicBool::new(false));
        let (notify, notes) = notes();
        let (c, ev) = (called.clone(), events.clone());
        // Like the real CLI wrapper: blocks until Stop, then errors "stopped".
        let ai: AiFn = Arc::new(move |_: &str, st: &AtomicBool| {
            c.store(true, Ordering::SeqCst);
            while !st.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(anyhow!("stopped"))
        });
        let opts = RunOptions {
            poll_url: url,
            username: String::new(),
            headless: true,
            geo: None,
            profile_dir: dir.join("profile"),
            logs_dir: dir.join("logs"),
            ai: Some(ai),
            click_ai: None,
            require_confirmation: false,
            confirm: Arc::new(|_, _, _, _| Ok(true)),
            reply: Arc::new(|_, _, _, _| Ok(None)),
            notify,
            send_image: None,
            on_event: Arc::new(move |e| ev.lock().unwrap().push(e)),
            stop: stop.clone(),
            poll_interval: Duration::from_millis(100),
            answer_delay: (Duration::ZERO, Duration::ZERO),
        };
        let h = std::thread::spawn(move || run(opts));
        wait_until("the AI call to start", 60, || {
            called.load(Ordering::SeqCst) || h.is_finished()
        });
        stop.store(true, Ordering::SeqCst);
        assert!(h.join().unwrap().is_ok());
        let evs = events.lock().unwrap().clone();
        let result = evs.iter().find(|e| e["event"] == "result").expect("result");
        assert_eq!(result["status"], json!("SKIPPED (stopped)"), "{evs:?}");
        assert!(votes.lock().unwrap().is_empty());
        assert_eq!(
            evs.last().unwrap(),
            &json!({"event": "state", "state": "stopped"})
        );
        let titles: Vec<String> = notes.lock().unwrap().iter().map(|n| n.0.clone()).collect();
        assert_eq!(titles, vec!["PollEv: we have a question (multiple_choice)"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
