//! Prompts, prompt-injection filtering, response parsing, and the AI CLI
//! backends. Port of app/ai_answer.py minus the run loop (watcher.rs).
//! See PORT_SPEC.md "ai".
use crate::poll::Question;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// prompt -> raw model text. When `stop` becomes true the call must abort
/// promptly (kill the child process) and return Err.
pub type AiFn = Arc<dyn Fn(&str, &AtomicBool) -> Result<String> + Send + Sync>;

pub const CLICK_TYPES: [&str; 2] = ["clickable_image", "clickable_image_multi"];

/// multiple_choice, select_multiple, short_answer, or a click type.
pub fn is_supported(qtype: &str) -> bool {
    let _ = qtype;
    todo!()
}

pub fn build_prompt(q: &Question) -> String {
    let _ = q;
    todo!()
}

pub fn build_click_prompt(q: &Question, image_path: &Path) -> String {
    let _ = (q, image_path);
    todo!()
}

/// (cleaned text, phrases found).
pub fn filter_injection_attempts(text: &str) -> (String, Vec<String>) {
    let _ = text;
    todo!()
}

/// Question and options with injection phrasing replaced by "[filtered]",
/// plus every phrase found.
pub fn filtered_question(q: &Question) -> (Question, Vec<String>) {
    let _ = q;
    todo!()
}

#[derive(Clone, Debug, PartialEq)]
pub struct AiAnswer {
    pub answer: Value,
    pub confidence: Option<i64>,
    pub reasoning: String,
}

pub fn parse_ai_response(raw: &str) -> Result<AiAnswer> {
    let _ = raw;
    todo!()
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClickAnswer {
    /// (x, y) fractions of the image's width/height.
    pub clicks: Vec<(f64, f64)>,
    pub confidence: Option<i64>,
    pub reasoning: String,
}

pub fn parse_click_response(raw: &str) -> Result<ClickAnswer> {
    let _ = raw;
    todo!()
}

/// "lgtm"-style approval -> the AI's answer; any other reply -> the reply
/// itself (minus a leading "change answer to"); None stays None.
pub fn resolve_short_answer_reply(reply: Option<&str>, ai_answer: &str) -> Option<String> {
    let _ = (reply, ai_answer);
    todo!()
}

pub fn answer_schema() -> Value {
    todo!()
}

pub fn click_schema() -> Value {
    todo!()
}

/// additionalProperties: false at every object level (Codex strict mode).
pub fn codex_strict_schema(schema: &Value) -> Value {
    let _ = schema;
    todo!()
}

/// backend: "nrp" | "subscription" | "codex". model/effort None = backend
/// default. cwd: where the CLI runs; must contain the poll image for click
/// questions (Claude's Read tool is scoped to its working directory).
pub fn make_ai_fn(backend: &str, model: Option<String>, effort: Option<String>, schema: Value, cwd: PathBuf) -> Result<AiFn> {
    let _ = (backend, model, effort, schema, cwd);
    todo!()
}

/// One trivial real call (ai_answer.test_ai_backend): (ok, readable detail).
pub fn test_backend(f: &AiFn) -> (bool, String) {
    let _ = f;
    todo!()
}

/// Same lists and sources as app/api.py list_models.
pub fn list_models(backend: &str) -> Result<Vec<String>> {
    let _ = backend;
    todo!()
}
