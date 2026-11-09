//! The watch loop (ai_answer.run + answer_with_ai) and the GUI-facing
//! service around it (watcher_service.py). See PORT_SPEC.md "watcher".
use crate::ai::AiFn;
use crate::cdp::Browser;
use crate::config::AppConfig;
use crate::notify::NotifyFn;
use crate::poll::Question;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Structured progress events for the UI, same JSON shapes as the Python
/// app ({"event": "state"|"sso_wall"|"reasoning"|"awaiting_reply"|"result"|"log", ...}).
pub type EventFn = Arc<dyn Fn(Value) + Send + Sync>;
/// (question, answer, confidence, reasoning) -> approved?
pub type ConfirmFn = Arc<dyn Fn(&Question, &Value, Option<i64>, &str) -> Result<bool> + Send + Sync>;
/// (question, answer, confidence, reasoning) -> reply text, None = skip.
pub type ReplyFn = Arc<dyn Fn(&Question, &Value, Option<i64>, &str) -> Result<Option<String>> + Send + Sync>;
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
}

/// Blocks until `stop` is set (Ok, after emitting state "stopped") or a
/// fatal error (Err, after emitting state "crashed" and a crash notification).
pub fn run(opts: RunOptions) -> Result<()> {
    let _ = opts;
    todo!()
}

/// One question end to end. Never errors: every outcome is a status in the
/// returned {"question","answer","confidence","reasoning","status"} object
/// (ai_answer.answer_with_ai).
pub fn answer_question(browser: &Browser, q: &Question, opts: &RunOptions) -> Value {
    let _ = (browser, q, opts);
    todo!()
}

/// Bounded thread-safe queue the UI drains; drops the oldest when full.
pub struct EventQueue {}

impl EventQueue {
    pub fn new(max: usize) -> Self {
        let _ = max;
        todo!()
    }

    pub fn put(&self, event: Value) {
        let _ = event;
        todo!()
    }

    pub fn get_events(&self, max: usize) -> Vec<Value> {
        let _ = max;
        todo!()
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
        Paths { profile_dir: crate::config::chrome_profile_dir(), logs_dir: crate::config::logs_dir() }
    }
}

/// Owns one watcher run at a time (watcher_service.WatcherService).
pub struct WatcherService {}

impl WatcherService {
    pub fn new(events: Arc<EventQueue>) -> Self {
        let _ = events;
        todo!()
    }

    /// Real AI backends + real notifications from `cfg`. Err if already running.
    pub fn start(&self, cfg: &AppConfig) -> Result<()> {
        let _ = cfg;
        todo!()
    }

    /// Same as start, with injected AI (text, click) and notifications;
    /// confirm/reply still come from the service (in-app + iMessage, first
    /// wins). Does not validate cfg.
    pub fn start_with(&self, cfg: &AppConfig, paths: Paths, ai: Option<(AiFn, AiFn)>, notify: NotifyFn) -> Result<()> {
        let _ = (cfg, paths, ai, notify);
        todo!()
    }

    /// Set stop, deny/skip any pending decision, wait up to `wait` for the
    /// thread. True if it has finished.
    pub fn stop(&self, wait: Duration) -> bool {
        let _ = wait;
        todo!()
    }

    /// idle|starting|watching|stopping|stopped|crashed
    pub fn state(&self) -> String {
        todo!()
    }

    pub fn is_alive(&self) -> bool {
        todo!()
    }

    pub fn last_error(&self) -> Option<String> {
        todo!()
    }

    /// {"mode","question","answer","confidence"} of the pending decision.
    pub fn pending(&self) -> Option<Value> {
        todo!()
    }

    /// The UI's Accept / Skip / Send. {"resolved": bool, "reason"?: str}
    pub fn decide(&self, decision: &str, text: &str) -> Value {
        let _ = (decision, text);
        todo!()
    }
}
