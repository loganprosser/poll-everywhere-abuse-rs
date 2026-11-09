//! PollEv page logic: detect the live question, submit answers, and the
//! continuous page-setup steps (cookie banner, SSO wall, screen name).
//! Port of app/textwatcher.py. See PORT_SPEC.md "poll".
use crate::cdp::{Browser, Page};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PollOption {
    pub id: Option<i64>,
    pub text: String,
}

/// Same shape as textwatcher.build_question_record, and what gets saved to
/// questions/poll_<ts>.json.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub question_id: Option<i64>,
    pub poll_key: Option<String>,
    pub timestamp: String,
    pub url: String,
    #[serde(rename = "type")]
    pub qtype: String,
    pub question: String,
    pub options: Vec<PollOption>,
    pub image_url: Option<String>,
    pub answer: Value,
    pub source: String,
}

/// Classification of PollEv's embedded activity JSON
/// (textwatcher.extract_poll_dom).
#[derive(Clone, Debug, PartialEq)]
pub struct Extracted {
    pub qtype: String,
    pub question: String,
    pub options: Vec<PollOption>,
    pub image_url: Option<String>,
    pub raw: Value,
}

/// Local time as %Y%m%d_%H%M%S.
pub fn now_ts() -> String {
    todo!()
}

/// First balanced {...} at or after `start`, respecting JSON string
/// literals; None if there's no valid object.
pub fn extract_first_json_object(text: &str, start: usize) -> Option<Value> {
    let _ = (text, start);
    todo!()
}

/// The object after the "activity: {" marker, if any.
pub fn extract_activity_json(html: &str) -> Option<Value> {
    let _ = html;
    todo!()
}

pub fn classify(activity: &Value) -> Extracted {
    let _ = activity;
    todo!()
}

/// The live question right now, or None. Cheap enough to call every tick.
pub fn get_current_question(page: &Page) -> Result<Option<Question>> {
    let _ = page;
    todo!()
}

#[derive(Debug)]
pub enum SubmitError {
    /// The question is no longer the live one (textwatcher.StaleQuestionError).
    Stale(String),
    /// Answer doesn't fit the question, or the page refused it (AnswerError).
    Answer(String),
}

impl std::fmt::Display for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubmitError::Stale(m) | SubmitError::Answer(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for SubmitError {}

/// Answer shapes (textwatcher.submit_answer): multiple_choice = option id
/// (number) or exact text (string); select_multiple = array of ids/texts;
/// short_answer = non-empty string; clickable_image[_multi] = array of
/// {"x","y"} fractions 0..1. Returns what was submitted.
pub fn submit_answer(page: &Page, q: &Question, answer: &Value) -> std::result::Result<Value, SubmitError> {
    let _ = (page, q, answer);
    todo!()
}

/// textwatcher.SSO_LOGIN_WALL_PATTERN against the page's body text.
pub fn sso_wall_showing(body_text: &str) -> bool {
    let _ = body_text;
    todo!()
}

/// What the page looked like this tick, after PageSetup acted on it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SetupStatus {
    /// URL is on pollev.com (not an SSO page).
    pub on_pollev: bool,
    /// The registration/login wall is showing.
    pub wall: bool,
    /// A password field is showing: the human's turn (never automated).
    pub awaiting_human: bool,
}

/// Continuous replacement for dismiss_cookie_banner,
/// wait_for_manual_sso_login, advance_sso_identifier_steps and
/// handle_initial_pollev_steps. tick() acts on whatever is on screen right
/// now and never sleeps for a fixed grace period.
#[derive(Debug, Default)]
pub struct PageSetup {}

impl PageSetup {
    pub fn tick(&mut self, page: &Page, username: &str, question_seen: bool) -> Result<SetupStatus> {
        let _ = (page, username, question_seen);
        todo!()
    }
}

pub fn download_image(url: &str, dest: &Path) -> Result<()> {
    let _ = (url, dest);
    todo!()
}

/// Copy of `image` with a marker at each (x, y) fraction, rendered in a
/// throwaway page of `browser` and screenshotted to `dest` (PNG).
pub fn render_marked_image(browser: &Browser, image: &Path, clicks: &[(f64, f64)], dest: &Path) -> Result<()> {
    let _ = (browser, image, clicks, dest);
    todo!()
}

/// questions/poll_<ts>.json plus one line appended to questions/index.jsonl.
pub fn save_question_json(logs_dir: &Path, q: &Question) -> Result<PathBuf> {
    let _ = (logs_dir, q);
    todo!()
}
