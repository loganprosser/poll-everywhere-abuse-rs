//! PollEv page logic: detect the live question, submit answers, and the
//! continuous page-setup steps (cookie banner, SSO wall, screen name).
//! Port of app/textwatcher.py. See PORT_SPEC.md "poll".
use crate::cdp::{Browser, Page};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

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
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

const ACTIVITY_JSON_MARKER: &str = "activity: {";

/// First balanced {...} at or after `start`, respecting JSON string
/// literals; None if there's no valid object.
pub fn extract_first_json_object(text: &str, start: usize) -> Option<Value> {
    // Braces, quotes and backslashes are ASCII, so scanning bytes is safe
    // even when the text holds multi-byte characters.
    let bytes = text.as_bytes();
    let begin = start + bytes.get(start..)?.iter().position(|&b| b == b'{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (i, &c) in bytes.iter().enumerate().skip(begin) {
        if in_string {
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[begin..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// The object after the "activity: {" marker, if any.
pub fn extract_activity_json(html: &str) -> Option<Value> {
    let idx = html.find(ACTIVITY_JSON_MARKER)?;
    extract_first_json_object(html, idx)
}

pub fn classify(activity: &Value) -> Extracted {
    let a_type = activity.get("type").and_then(Value::as_str).unwrap_or("");
    let title = activity
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // Keep each option's PollEv id with its text: it ties an answer to a
    // specific option, unlike a bare list of strings.
    let options: Vec<PollOption> = activity
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    let text = o.get("value").and_then(Value::as_str)?;
                    (!text.is_empty()).then(|| PollOption {
                        id: o.get("id").and_then(Value::as_i64),
                        text: text.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    // max_votes is PollEv's per-participant cap (confirmed live): 1 means one
    // click ever (single-select), anything else allows several. NB:
    // responses_per_option is an audience-wide cap per option and does not
    // affect how many options one participant may pick.
    let single = activity.get("max_votes").and_then(Value::as_f64) == Some(1.0);
    let (qtype, options, image_url) = match a_type {
        "multiple_choice_poll" => (
            if single {
                "multiple_choice"
            } else {
                "select_multiple"
            },
            options,
            None,
        ),
        "free_text_poll" => ("short_answer", Vec::new(), None),
        // One catch-all "region:other" option regardless of where you click,
        // so there are no options to resolve against.
        "clickable_image" => (
            if single {
                "clickable_image"
            } else {
                "clickable_image_multi"
            },
            Vec::new(),
            activity
                .pointer("/type_options/imageUrl")
                .and_then(Value::as_str)
                .map(str::to_string),
        ),
        _ => ("unknown", options, None),
    };
    Extracted {
        qtype: qtype.to_string(),
        question: title,
        options,
        image_url,
        raw: activity.clone(),
    }
}

/// textwatcher.short_preview: whitespace collapsed to one line, cut to `n`
/// characters.
fn short_preview(text: &str, n: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > n {
        format!("{}...", one_line.chars().take(n).collect::<String>())
    } else {
        one_line
    }
}

fn build_question_record(url: &str, text: &str, ex: &Extracted) -> Question {
    Question {
        question_id: ex.raw.get("id").and_then(Value::as_i64),
        poll_key: ex
            .raw
            .get("shared_key")
            .and_then(Value::as_str)
            .map(str::to_string),
        timestamp: now_ts(),
        url: url.to_string(),
        qtype: ex.qtype.clone(),
        question: if ex.question.is_empty() {
            short_preview(text, 300)
        } else {
            ex.question.clone()
        },
        options: ex.options.clone(),
        image_url: ex.image_url.clone(),
        answer: Value::Null,
        source: "dom".to_string(),
    }
}

/// One round trip: the document only when it holds the activity marker.
/// PollEv hydrates its renderer from an embedded `activity: {...}` object,
/// absent when nothing is live, so its presence is the trigger.
const CURRENT_QUESTION_JS: &str = r#"(() => {
  const h = document.documentElement.outerHTML;
  const i = h.indexOf('activity: {');
  if (i < 0) return null;
  return {html: h.slice(i, i + 500000), url: location.href,
          text: document.body ? document.body.innerText : ''};
})()"#;

/// The live question right now, or None. Cheap enough to call every tick.
pub fn get_current_question(page: &Page) -> Result<Option<Question>> {
    let v = page.eval(CURRENT_QUESTION_JS)?;
    if v.is_null() {
        return Ok(None);
    }
    let html = v.get("html").and_then(Value::as_str).unwrap_or("");
    let url = v.get("url").and_then(Value::as_str).unwrap_or("");
    let text = v.get("text").and_then(Value::as_str).unwrap_or("");
    // An empty object is as good as none (Python's `if not activity`).
    let Some(activity) =
        extract_activity_json(html).filter(|a| a.as_object().is_some_and(|o| !o.is_empty()))
    else {
        return Ok(None);
    };
    Ok(Some(build_question_record(url, text, &classify(&activity))))
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

fn answer_err(msg: impl Into<String>) -> SubmitError {
    SubmitError::Answer(msg.into())
}

/// Python repr of a str, close enough for error messages.
fn py_str(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn py_str_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    format!("[{}]", items.map(py_str).collect::<Vec<_>>().join(", "))
}

/// textwatcher._resolve_option: by option id first, then exact text.
fn resolve_option<'a>(
    options: &'a [PollOption],
    value: &Value,
) -> std::result::Result<&'a PollOption, SubmitError> {
    if let Some(n) = value.as_f64()
        && let Some(o) = options
            .iter()
            .find(|o| o.id.is_some_and(|id| id as f64 == n))
    {
        return Ok(o);
    }
    if let Some(s) = value.as_str()
        && let Some(o) = options.iter().find(|o| o.text == s)
    {
        return Ok(o);
    }
    let shown = match value.as_str() {
        Some(s) => py_str(s),
        None => value.to_string(),
    };
    Err(answer_err(format!(
        "{shown} doesn't match any current option: {}",
        py_str_list(options.iter().map(|o| o.text.as_str()))
    )))
}

const CLICK_WAIT: Duration = Duration::from_secs(5);
const UNDO_WAIT: Duration = Duration::from_secs(3);

/// Click via `click`; if it still fails after its full wait, try the page's
/// "change answer" path once: click a visible undo button, wait 400ms, retry.
/// PollEv briefly (~300-500ms) disables every option while a vote is in
/// flight (confirmed live), which click_element rides out on its own, so a
/// click that is STILL failing means it is stuck or a different answer is
/// selected and blocking this one.
fn click_or_undo(
    page: &Page,
    click: &dyn Fn() -> Result<()>,
    undo_selector: &str,
    what: &str,
    retry_note: &str,
) -> std::result::Result<(), SubmitError> {
    let first_error = match click() {
        Ok(()) => return Ok(()),
        Err(e) => e,
    };
    let undo_finder = format!(
        "[...document.querySelectorAll({sel})].find(e => {{ const r = e.getBoundingClientRect(); \
         const cs = getComputedStyle(e); return r.width > 0 && r.height > 0 && \
         cs.visibility !== 'hidden' && cs.display !== 'none'; }}) || null",
        sel = json!(undo_selector)
    );
    let has_undo = page
        .eval(&format!("!!({undo_finder})"))
        .is_ok_and(|v| v.as_bool() == Some(true));
    if !has_undo {
        return Err(answer_err(format!(
            "couldn't click {what} -- you've likely already answered this question and \
             changing answers isn't allowed for it: {first_error}"
        )));
    }
    let retry = || -> Result<()> {
        page.click_element(&undo_finder, UNDO_WAIT)?;
        std::thread::sleep(Duration::from_millis(400));
        click()
    };
    retry().map_err(|e| {
        answer_err(format!(
            "couldn't click {what} even after undoing the previous answer{retry_note}: {e}"
        ))
    })
}

/// Prefer the button whose whole text is the option (so "2" never hits "12"),
/// then a case-insensitive whole-text match, and only then the first whose
/// text contains it. innerText follows CSS text-transform (a button styled
/// uppercase reads "A" for the option "a"), so textContent is checked too;
/// without the middle tier, "a" would fall through to the substring hit on
/// "Banana".
fn option_finder(text: &str) -> String {
    format!(
        r#"(() => {{
  const norm = s => (s || '').replace(/\s+/g, ' ').trim();
  const want = norm({});
  const lw = want.toLowerCase();
  const btns = [...document.querySelectorAll('button.component-response-multiple-choice__option__vote')];
  const texts = b => [norm(b.innerText), norm(b.textContent)];
  return btns.find(b => texts(b).includes(want))
    || btns.find(b => texts(b).some(t => t.toLowerCase() === lw))
    || btns.find(b => texts(b).some(t => t.toLowerCase().includes(lw)))
    || null;
}})()"#,
        json!(text)
    )
}

fn click_option(page: &Page, option: &PollOption) -> std::result::Result<(), SubmitError> {
    let finder = option_finder(&option.text);
    click_or_undo(
        page,
        &|| page.click_element(&finder, CLICK_WAIT),
        "button.component-response-multiple-choice__option__undo",
        &format!("option {}", py_str(&option.text)),
        " -- it may not have been the one blocking it",
    )
}

const IMAGE_FINDER: &str =
    "document.querySelector('img.component-response-clickable-image__image')";

/// Click at a fraction (0..1) of the image's rendered size. PollEv computes
/// the same percentage regardless of natural vs rendered size (confirmed
/// live), so a plain fraction needs no natural dimensions. Same undo-then-retry
/// fallback as options, using the single "Clear last response" button.
fn click_image_point(page: &Page, x: f64, y: f64) -> std::result::Result<(), SubmitError> {
    // Wait like the option click does: right after a question appears the
    // image may not be laid out yet.
    let present = page
        .wait_for(
            &format!(
                "(() => {{ const i = {IMAGE_FINDER}; if (!i) return false; \
                 const r = i.getBoundingClientRect(); return r.width > 0 && r.height > 0; }})()"
            ),
            CLICK_WAIT,
        )
        .unwrap_or(false);
    if !present {
        return Err(answer_err("couldn't find the clickable image on the page"));
    }
    click_or_undo(
        page,
        &|| page.click_element_at(IMAGE_FINDER, x, y, CLICK_WAIT),
        "button.component-response-clickable-image__undo",
        &format!("at ({x:.3}, {y:.3})"),
        "",
    )
}

/// A fractional click point; accepts numbers or numeric strings like
/// Python's float().
fn parse_point(point: &Value) -> std::result::Result<(f64, f64), SubmitError> {
    let num = |v: Option<&Value>| match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    };
    let (Some(x), Some(y)) = (num(point.get("x")), num(point.get("y"))) else {
        return Err(answer_err(format!(
            "each click must be {{'x':.., 'y':..}} fractional coordinates, got {point}"
        )));
    };
    if !((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)) {
        return Err(answer_err(format!(
            "click coordinates must be fractions 0.0-1.0, got {point}"
        )));
    }
    Ok((x, y))
}

/// Answer shapes (textwatcher.submit_answer): multiple_choice = option id
/// (number) or exact text (string); select_multiple = array of ids/texts;
/// short_answer = non-empty string; clickable_image[_multi] = array of
/// {"x","y"} fractions 0..1. Returns what was submitted.
pub fn submit_answer(
    page: &Page,
    q: &Question,
    answer: &Value,
) -> std::result::Result<Value, SubmitError> {
    // Stale check first: the live question must still be the one we were given.
    let current = get_current_question(page)
        .map_err(|e| answer_err(format!("couldn't read the page: {e}")))?;
    let show_id = |id: Option<i64>| id.map_or("None".to_string(), |n| n.to_string());
    let Some(current) = current.as_ref().filter(|c| c.question_id == q.question_id) else {
        return Err(SubmitError::Stale(format!(
            "question {} is no longer live (current: {})",
            show_id(q.question_id),
            show_id(current.as_ref().and_then(|c| c.question_id))
        )));
    };

    let submitted = match q.qtype.as_str() {
        "multiple_choice" => {
            let option = resolve_option(&current.options, answer)?;
            click_option(page, option)?;
            serde_json::to_value(option).unwrap_or(Value::Null)
        }
        "select_multiple" => {
            let Some(list) = answer.as_array().filter(|a| !a.is_empty()) else {
                return Err(answer_err(
                    "select_multiple answer must be a non-empty list of option ids/texts",
                ));
            };
            // Resolve everything first so a bad entry can't leave a partial vote.
            let options = list
                .iter()
                .map(|v| resolve_option(&current.options, v))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (i, option) in options.iter().enumerate() {
                if i > 0 {
                    // Let each vote register before the next click.
                    std::thread::sleep(Duration::from_millis(300));
                }
                click_option(page, option).map_err(|e| {
                    answer_err(format!(
                        "{e} ({i} option(s) already submitted before this failure: {})",
                        py_str_list(options[..i].iter().map(|o| o.text.as_str()))
                    ))
                })?;
            }
            serde_json::to_value(&options).unwrap_or(Value::Null)
        }
        "short_answer" => {
            let Some(text) = answer.as_str().filter(|s| !s.trim().is_empty()) else {
                return Err(answer_err("short_answer answer must be a non-empty string"));
            };
            submit_short_answer(page, text)
                .map_err(|e| answer_err(format!("failed to submit short answer: {e}")))?;
            json!(text)
        }
        "clickable_image" | "clickable_image_multi" => {
            let Some(list) = answer.as_array().filter(|a| !a.is_empty()) else {
                return Err(answer_err(format!(
                    "{} answer must be a non-empty list of {{'x':.., 'y':..}} fractional points",
                    q.qtype
                )));
            };
            let points = list
                .iter()
                .map(parse_point)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (i, &(x, y)) in points.iter().enumerate() {
                if i > 0 {
                    std::thread::sleep(Duration::from_millis(300));
                }
                click_image_point(page, x, y).map_err(|e| {
                    let done = points[..i]
                        .iter()
                        .map(|(x, y)| format!("{{'x': {x:?}, 'y': {y:?}}}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    answer_err(format!(
                        "{e} ({i} click(s) already submitted before this failure: [{done}])"
                    ))
                })?;
            }
            Value::Array(
                points
                    .iter()
                    .map(|&(x, y)| json!({"x": x, "y": y}))
                    .collect(),
            )
        }
        other => {
            return Err(answer_err(format!(
                "don't know how to submit an answer for type {}",
                py_str(other)
            )));
        }
    };

    std::thread::sleep(Duration::from_millis(200));
    Ok(submitted)
}

const TEXTAREA: &str = "document.querySelector(\"textarea[name='response']\")";

fn submit_short_answer(page: &Page, text: &str) -> Result<()> {
    // Wait for the form like Playwright's fill did, then replace its contents
    // with real (trusted) input so React-controlled fields see it.
    let ready = format!("(() => {{ const t = {TEXTAREA}; return !!t && !t.disabled; }})()");
    if !page.wait_for(&ready, CLICK_WAIT)? {
        bail!("response box not found on the page");
    }
    page.eval(&format!(
        "(() => {{ const t = {TEXTAREA}; t.focus(); t.select(); }})()"
    ))?;
    page.insert_text(text)?;
    page.click_element(
        "document.querySelector(\"input[type='submit']\")",
        CLICK_WAIT,
    )
}

static SSO_LOGIN_WALL: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?is)",
        r"pre-registration required|", // confirmed live, exact PollEv wall heading
        r"log in to (?:the )?poll everywhere account|", // confirmed live: "...account registered with <name> to respond"
        r"log ?in to .{0,80}?to (?:respond|participate)|",
        r"sign ?in to (?:respond|participate)|",
        r"registered participants only|must be logged ?in|please log ?in to continue|",
        r"you must sign in",
    ))
    .ok()
});

/// textwatcher.SSO_LOGIN_WALL_PATTERN against the page's body text.
pub fn sso_wall_showing(body_text: &str) -> bool {
    SSO_LOGIN_WALL
        .as_ref()
        .is_some_and(|re| re.is_match(body_text))
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

/// Shared JS: visibility helpers and one finder per page element PageSetup
/// touches. The snapshot reports on them and the actions re-resolve the same
/// finder, so what we decide on is what we click.
const SETUP_HELPERS: &str = r#"
const vis = e => {
  if (!e || !e.isConnected) return false;
  const cs = getComputedStyle(e);
  if (cs.display === 'none' || cs.visibility === 'hidden') return false;
  const r = e.getBoundingClientRect();
  return r.width > 0 && r.height > 0;
};
const txt = e => String(e.innerText || e.value || '').replace(/\s+/g, ' ').trim();
const all = s => [...document.querySelectorAll(s)];
const first = s => all(s).find(vis) || null;
const BTN = 'button,[role=button],input[type=submit],input[type=button]';
const byText = (s, re) => all(s).find(e => vis(e) && re.test(txt(e))) || null;
const f = {
  password: () => first('input[type=password]'),
  email: () => first('input#email, input[type=email]'),
  emailSubmit: () => first("input[type='submit'], button[type='submit']"),
  cornell: () => byText(BTN, /log ?in with cornell/i),
  netid: () => first('input#user_identifier'),
  cont: () => byText(BTN, /^continue$/i),
  // The wall's own Log in is an <a class=mdc-button>; the site nav has an
  // unrelated "Log in" link (presenter login) that breaks the session if
  // clicked (confirmed live), so scope to the class.
  wallLogin: () => all('a.mdc-button').find(e => vis(e) && /^\s*log ?in\s*$/i.test(txt(e))) || null,
  cookie: () => all('button,[role=button],a').find(e => {
    if (!vis(e)) return false;
    const t = txt(e);
    return t.length > 0 && t.length < 40 && /\b(accept|agree|allow)\b/i.test(t);
  }) || null,
  name: () => first('input[type=text]') || first('input[type=name]') ||
    first('input:not([type=hidden]):not([type=password]):not([type=checkbox]):not([type=radio]):not([type=submit]):not([type=button]):not([type=image]):not([type=file]):not([type=reset])'),
  nameSubmit: () => byText('button,[role=button],input[type=submit]', /continue|submit|enter|join|start/i),
};
"#;

fn setup_snapshot_js() -> String {
    format!(
        r#"(() => {{{SETUP_HELPERS}
const inp = e => e ? {{empty: !e.value, value: e.value || ''}} : null;
return {{
  url: location.href,
  text: document.body ? document.body.innerText : '',
  password: !!f.password(),
  email: inp(f.email()),
  cornell: !!f.cornell(),
  netid: inp(f.netid()),
  wall_login: !!f.wallLogin(),
  cookie: !!f.cookie(),
  name: inp(f.name()),
  name_submit: !!f.nameSubmit(),
  activity: document.documentElement.outerHTML.includes('activity: {{'),
}};
}})()"#
    )
}

/// A click-ready finder expression for one of the helpers above.
fn setup_finder(which: &str) -> String {
    format!("(() => {{{SETUP_HELPERS}\nreturn f.{which}();\n}})()")
}

/// Focus + select via JS, then real text input (Never a password: callers
/// only pass identifiers).
fn setup_fill(page: &Page, which: &str, text: &str) -> Result<()> {
    let found = page.eval(&format!(
        "(() => {{{SETUP_HELPERS}\nconst el = f.{which}(); if (!el) return false; \
         el.focus(); try {{ el.select(); }} catch (e) {{}} return true; }})()"
    ))?;
    if found.as_bool() != Some(true) {
        bail!("{which} input not found");
    }
    page.insert_text(text)
}

fn setup_click(page: &Page, which: &str) -> Result<()> {
    page.click_element(&setup_finder(which), Duration::from_secs(3))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Field {
    empty: bool,
    value: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Snapshot {
    url: String,
    text: String,
    password: bool,
    email: Option<Field>,
    cornell: bool,
    netid: Option<Field>,
    wall_login: bool,
    cookie: bool,
    name: Option<Field>,
    name_submit: bool,
    activity: bool,
}

/// Substring like the Python app (SSO hosts such as id.polleverywhere.com and
/// Cornell never contain "pollev.com"), but ignoring the query and fragment:
/// a login page's `return_to=https%3A%2F%2Fpollev.com...` must not make it
/// look like PollEv itself.
fn url_on_pollev(url: &str) -> bool {
    url.split(['?', '#'])
        .next()
        .is_some_and(|u| u.contains("pollev.com"))
}

/// Continuous replacement for dismiss_cookie_banner,
/// wait_for_manual_sso_login, advance_sso_identifier_steps and
/// handle_initial_pollev_steps. tick() acts on whatever is on screen right
/// now and never sleeps for a fixed grace period.
#[derive(Debug, Default)]
pub struct PageSetup {
    cookie_done: bool,
    cookie_tries: u8,
    name_done: bool,
    last_wall_click: Option<Instant>,
    last_sso_action: Option<(String, Instant)>,
}

impl PageSetup {
    /// Reads the page once, then does at most one action. Failed actions are
    /// logged, not returned (pages navigate mid-click; the watcher counts Err
    /// ticks toward a fatal limit), so only a failed page read is an Err.
    pub fn tick(
        &mut self,
        page: &Page,
        username: &str,
        question_seen: bool,
    ) -> Result<SetupStatus> {
        let snap: Snapshot = serde_json::from_value(page.eval(&setup_snapshot_js())?)
            .context("unexpected page snapshot")?;
        let on_pollev = url_on_pollev(&snap.url);
        // Only with no live question: its text can contain wall phrases (Python parity).
        let wall = on_pollev && !snap.activity && sso_wall_showing(&snap.text);
        let status = SetupStatus {
            on_pollev,
            wall,
            awaiting_human: !on_pollev && snap.password,
        };
        let username = username.trim();

        // 1. Cookie banner, once. Only on pollev.com: SSO pages can have an
        // unrelated "Accept" (e.g. a consent screen) that must stay a human choice.
        if on_pollev && snap.cookie && !self.cookie_done {
            self.cookie_tries += 1;
            match setup_click(page, "cookie") {
                Ok(()) => self.cookie_done = true,
                Err(e) => {
                    eprintln!("[setup] cookie click failed: {e}");
                    // A banner we can't click must not cost 3s every tick forever.
                    self.cookie_done = self.cookie_tries >= 3;
                }
            }
            return Ok(status);
        }

        // 2. The registration wall: jump to SSO via its own Log in button.
        if wall {
            let due = self
                .last_wall_click
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(5));
            if snap.wall_login && due {
                self.last_wall_click = Some(Instant::now());
                match setup_click(page, "wallLogin") {
                    Ok(()) => eprintln!("[sso] clicked the poll's Log in button"),
                    Err(e) => eprintln!("[sso] wall Log in click failed: {e}"),
                }
            }
            return Ok(status);
        }

        // 3. Off pollev.com: identifier-only SSO screens. Never a password.
        if !on_pollev {
            if !snap.password {
                self.sso_step(page, &snap, username);
            }
            return Ok(status);
        }

        // 4. Screen name, once, and only before any question was seen.
        if !snap.activity
            && !question_seen
            && !self.name_done
            && !username.is_empty()
            && snap.name.as_ref().is_some_and(|n| n.empty)
        {
            match setup_fill(page, "name", username) {
                Err(e) => eprintln!("[setup] couldn't fill screen name: {e}"),
                Ok(()) => {
                    self.name_done = true;
                    eprintln!("[setup] filled username: {username}");
                    let submitted = snap.name_submit && setup_click(page, "nameSubmit").is_ok();
                    if !submitted && let Err(e) = page.press_enter() {
                        eprintln!("[setup] could not submit username step: {e}");
                    }
                }
            }
        }
        Ok(status)
    }

    /// One identifier-screen action: PollEv email page -> "Log in with
    /// Cornell" chooser -> Cornell NetID page (cornell_sso_flow.md steps 1-3).
    /// A field already holding exactly our value (fill ok, click failed) is
    /// clicked again rather than refilled. Never the same URL within 2s.
    fn sso_step(&mut self, page: &Page, snap: &Snapshot, user: &str) {
        if let Some((url, at)) = &self.last_sso_action
            && *url == snap.url
            && at.elapsed() < Duration::from_secs(2)
        {
            return;
        }
        let email = format!("{user}@cornell.edu");
        let attempt = || -> Result<Option<&'static str>> {
            // Fresh fields and the chooser button come first. A field that
            // already holds our value is only a retry (fill ok, click failed),
            // and must not win on the chooser page, whose prefilled readonly
            // email would otherwise click the first submit button there
            // ("Use password instead") instead of "Log in with Cornell".
            if !user.is_empty()
                && let Some(e) = &snap.email
                && e.empty
            {
                setup_fill(page, "email", &email)?;
                setup_click(page, "emailSubmit")?;
                return Ok(Some("filled email, continuing"));
            }
            if snap.cornell {
                setup_click(page, "cornell")?;
                return Ok(Some("chose Cornell NetID login"));
            }
            if !user.is_empty()
                && let Some(n) = &snap.netid
                && n.empty
            {
                setup_fill(page, "netid", user)?;
                setup_click(page, "cont")?;
                return Ok(Some("filled NetID, continuing"));
            }
            if !user.is_empty() && snap.email.as_ref().is_some_and(|e| e.value == email) {
                setup_click(page, "emailSubmit")?;
                return Ok(Some("filled email, continuing"));
            }
            if !user.is_empty() && snap.netid.as_ref().is_some_and(|n| n.value == user) {
                setup_click(page, "cont")?;
                return Ok(Some("filled NetID, continuing"));
            }
            Ok(None)
        };
        match attempt() {
            Ok(None) => {}
            Ok(Some(what)) => {
                eprintln!("[sso] {what}");
                self.last_sso_action = Some((snap.url.clone(), Instant::now()));
            }
            Err(e) => {
                eprintln!("[sso] step failed: {e}");
                self.last_sso_action = Some((snap.url.clone(), Instant::now()));
            }
        }
    }
}

/// Download a clickable-image poll's image. Fails loudly: with no image there
/// is nothing to reason about.
pub fn download_image(url: &str, dest: &Path) -> Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(15)))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("downloading {url}"))?;
    let bytes = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))?;
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(dest, bytes).with_context(|| format!("writing {}", dest.display()))
}

const MARKER_RADIUS: u32 = 14;

/// Copy of `image` with a marker at each (x, y) fraction, rendered in a
/// throwaway page of `browser` and screenshotted to `dest` (PNG).
pub fn render_marked_image(
    browser: &Browser,
    image: &Path,
    clicks: &[(f64, f64)],
    dest: &Path,
) -> Result<()> {
    let bytes = std::fs::read(image).with_context(|| format!("reading {}", image.display()))?;
    let is_png = image
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    let mime = if is_png { "image/png" } else { "image/jpeg" };
    // A data: URI, not file://: a set_content page (about:blank origin)
    // silently fails to load file:// images (confirmed live).
    let data_uri = format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    );
    // Lime fill + black/white double ring: a red-only marker was invisible
    // against a red target (confirmed live); this can't vanish into any background.
    let r = MARKER_RADIUS;
    let markers: String = clicks
        .iter()
        .map(|(x, y)| {
            format!(
                "<div style=\"position:absolute;left:calc({}% - {r}px);top:calc({}% - {r}px);\
                 width:{}px;height:{}px;border-radius:50%;background:#39ff14;border:3px solid black;\
                 box-shadow:0 0 0 2px white;box-sizing:border-box;\"></div>",
                x * 100.0,
                y * 100.0,
                r * 2,
                r * 2
            )
        })
        .collect();
    let html = format!(
        "<body style=\"margin:0\"><div id=\"marked-wrapper\" style=\"position:relative;\
         display:inline-block;line-height:0\"><img id=\"target-image\" src=\"{data_uri}\" \
         style=\"display:block\">{markers}</div></body>"
    );
    let page = browser.new_page()?;
    let result = (|| -> Result<()> {
        page.set_content(&html)?;
        let loaded = page.wait_for(
            "(() => { const i = document.getElementById('target-image'); \
             return !!i && i.complete && i.naturalWidth > 0; })()",
            Duration::from_secs(10),
        )?;
        if !loaded {
            bail!("image failed to load while rendering marked image");
        }
        let rect = page.eval(
            "(() => { const r = document.getElementById('marked-wrapper').getBoundingClientRect(); \
             return [r.left + scrollX, r.top + scrollY, r.width, r.height]; })()",
        )?;
        let n = |i: usize| {
            rect.get(i)
                .and_then(Value::as_f64)
                .ok_or_else(|| anyhow!("couldn't measure the marked image"))
        };
        let png = page.screenshot_png(Some((n(0)?, n(1)?, n(2)?, n(3)?)))?;
        std::fs::write(dest, png).with_context(|| format!("writing {}", dest.display()))
    })();
    let _ = page.close();
    result
}

/// questions/poll_<ts>.json plus one line appended to questions/index.jsonl.
pub fn save_question_json(logs_dir: &Path, q: &Question) -> Result<PathBuf> {
    let dir = logs_dir.join("questions");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let body = serde_json::to_string_pretty(q)?;
    // Two questions inside one second must not overwrite each other's file.
    let mut n = 0u32;
    let path = loop {
        let name = if n == 0 {
            format!("poll_{}.json", q.timestamp)
        } else {
            format!("poll_{}_{n}.json", q.timestamp)
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(body.as_bytes())?;
                break path;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            Err(e) => return Err(e).with_context(|| format!("writing {}", path.display())),
        }
    };
    // One line per question ever detected, newest last: a quick overview
    // without opening every file.
    let line = json!({
        "timestamp": q.timestamp,
        "question_id": q.question_id,
        "type": q.qtype,
        "question": q.question,
        "file": path.file_name().and_then(|n| n.to_str()).unwrap_or_default(),
    });
    let mut index = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("index.jsonl"))?;
    writeln!(index, "{line}")?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::{LaunchOptions, find_chrome};
    use std::io::Read;

    // ---- pure functions ----

    #[test]
    fn json_object_respects_braces_in_strings() {
        let text = r#"junk activity: {"title": "\\frac{1}{2} and \"}\" ok", "n": {"a": [1, 2]}} trailing {"x": 1}"#;
        let v = extract_activity_json(text).unwrap();
        assert_eq!(v["title"], json!("\\frac{1}{2} and \"}\" ok"));
        assert_eq!(v["n"]["a"], json!([1, 2]));
        // Unbalanced, invalid, or missing objects are None.
        assert!(extract_first_json_object("{\"a\": 1", 0).is_none());
        assert!(extract_first_json_object("{not json}", 0).is_none());
        assert!(extract_first_json_object("no braces", 0).is_none());
        assert!(extract_first_json_object("{}", 99).is_none());
        assert!(extract_activity_json("nothing here {\"a\":1}").is_none());
        // Multi-byte text before the object doesn't confuse byte offsets.
        let v = extract_first_json_object("héllo → {\"k\": \"é}\"}", 0).unwrap();
        assert_eq!(v["k"], json!("é}"));
    }

    #[test]
    fn classify_maps_each_type() {
        let mc = classify(&json!({
            "id": 5, "type": "multiple_choice_poll", "title": "  Pick  ", "max_votes": 1,
            "options": [{"id": 1, "value": "A"}, {"id": 2, "value": ""}, {"id": 3, "value": "C"}, {"id": 4}]
        }));
        assert_eq!(mc.qtype, "multiple_choice");
        assert_eq!(mc.question, "Pick");
        assert_eq!(
            mc.options,
            vec![
                PollOption {
                    id: Some(1),
                    text: "A".into()
                },
                PollOption {
                    id: Some(3),
                    text: "C".into()
                }
            ]
        );
        assert_eq!(mc.image_url, None);
        for max_votes in [json!(null), json!(3)] {
            let sm = classify(&json!({"type": "multiple_choice_poll", "max_votes": max_votes}));
            assert_eq!(sm.qtype, "select_multiple");
        }
        assert_eq!(
            classify(&json!({"type": "multiple_choice_poll"})).qtype,
            "select_multiple"
        );
        let fr = classify(
            &json!({"type": "free_text_poll", "title": "Why?", "options": [{"id": 1, "value": "x"}]}),
        );
        assert_eq!(fr.qtype, "short_answer");
        assert!(fr.options.is_empty());
        let img = classify(&json!({
            "type": "clickable_image", "max_votes": 1, "type_options": {"imageUrl": "http://x/y.png"}
        }));
        assert_eq!(img.qtype, "clickable_image");
        assert_eq!(img.image_url.as_deref(), Some("http://x/y.png"));
        let multi =
            classify(&json!({"type": "clickable_image", "type_options": {"imageUrl": "u"}}));
        assert_eq!(multi.qtype, "clickable_image_multi");
        let unk = classify(
            &json!({"type": "word_cloud", "title": "T", "options": [{"id": 9, "value": "w"}]}),
        );
        assert_eq!(unk.qtype, "unknown");
        assert_eq!(unk.options.len(), 1);
        assert_eq!(unk.raw["type"], json!("word_cloud"));
    }

    #[test]
    fn wall_pattern_matches_and_rejects() {
        for yes in [
            "Pre-registration required",
            "Please log in to the Poll Everywhere account registered with Prof to respond",
            "Log in to Poll Everywhere account",
            "log in to the class to participate",
            "Sign in to respond",
            "signin to participate",
            "Registered participants only",
            "You must be logged in",
            "please login to continue",
            "You must sign in",
            "log in to the thing\nacross lines to respond",
        ] {
            assert!(sso_wall_showing(yes), "{yes}");
        }
        for no in [
            "Waiting for presentation",
            "Log in",
            "Respond to the poll",
            "Please log in sometime",
            "",
        ] {
            assert!(!sso_wall_showing(no), "{no}");
        }
    }

    #[test]
    fn option_resolution_prefers_id_then_exact_text() {
        let opts = vec![
            PollOption {
                id: Some(1),
                text: "12".into(),
            },
            PollOption {
                id: Some(2),
                text: "2".into(),
            },
            PollOption {
                id: None,
                text: "none".into(),
            },
        ];
        assert_eq!(resolve_option(&opts, &json!(2)).unwrap().text, "2");
        assert_eq!(resolve_option(&opts, &json!("12")).unwrap().id, Some(1));
        assert_eq!(resolve_option(&opts, &json!("none")).unwrap().id, None);
        let err = resolve_option(&opts, &json!("1")).unwrap_err().to_string();
        assert!(
            err.contains("'1' doesn't match any current option: ['12', '2', 'none']"),
            "{err}"
        );
        assert!(resolve_option(&opts, &json!(99)).is_err());
    }

    #[test]
    fn preview_and_timestamp() {
        assert_eq!(short_preview("a \n b\t c", 300), "a b c");
        assert_eq!(short_preview("abcdef", 3), "abc...");
        let ts = now_ts();
        assert_eq!(ts.len(), 15);
        assert_eq!(&ts[8..9], "_");
        let ex =
            classify(&json!({"id": 7, "shared_key": "k", "type": "free_text_poll", "title": ""}));
        let q = build_question_record("http://u", "body  text", &ex);
        assert_eq!(q.question, "body text");
        assert_eq!((q.question_id, q.poll_key.as_deref()), (Some(7), Some("k")));
        assert_eq!(q.source, "dom");
    }

    #[test]
    fn save_question_writes_file_and_index() {
        let dir = std::env::temp_dir().join(format!("pollev-poll-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ex = classify(
            &json!({"id": 7, "type": "multiple_choice_poll", "max_votes": 1,
            "title": "Q?", "options": [{"id": 1, "value": "A"}]}),
        );
        let q = build_question_record("http://u", "t", &ex);
        let p1 = save_question_json(&dir, &q).unwrap();
        let p2 = save_question_json(&dir, &q).unwrap(); // same second: no overwrite
        assert_ne!(p1, p2);
        let back: Question = serde_json::from_str(&std::fs::read_to_string(&p1).unwrap()).unwrap();
        assert_eq!(back, q);
        let index = std::fs::read_to_string(dir.join("questions/index.jsonl")).unwrap();
        let lines: Vec<Value> = index
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["question"], json!("Q?"));
        assert_eq!(lines[0]["type"], json!("multiple_choice"));
        assert_eq!(
            lines[0]["file"],
            json!(p1.file_name().unwrap().to_str().unwrap())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- browser fixtures ----

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
            std::env::temp_dir().join(format!("pollev-poll-test-{}-{name}", std::process::id()));
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
        fn finish(self) {
            let Fixture { browser, profile } = self;
            drop(browser);
            let _ = std::fs::remove_dir_all(profile);
        }
    }

    /// Local one-page HTTP server (any path), so a test page can have a URL
    /// that contains "pollev.com".
    fn serve(status: u16, body: Vec<u8>) -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                // One thread per connection with a read timeout: Chrome opens
                // speculative sockets that never send a request, and one of
                // those must not stall the real request behind it.
                let body = body.clone();
                std::thread::spawn(move || {
                    let mut s = s;
                    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                    let mut buf = [0u8; 4096];
                    if !matches!(s.read(&mut buf), Ok(n) if n > 0) {
                        return;
                    }
                    let head = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(&body);
                });
            }
        });
        port
    }

    /// A page carrying PollEv's embedded `activity: {...}` marker.
    fn poll_page(activity: Value, body: &str) -> String {
        format!(
            "<html><body style='margin:0'><script>window.__setup = {{activity: {activity}}};</script>{body}</body></html>"
        )
    }

    const VOTE: &str = "component-response-multiple-choice__option__vote";
    const UNDO: &str = "component-response-multiple-choice__option__undo";
    const RECORD: &str = "(window.clicks=window.clicks||[]).push(this.innerText.trim())";

    fn vote_buttons(texts: &[&str]) -> String {
        texts
            .iter()
            .map(|t| format!("<button class='{VOTE}' onclick=\"{RECORD}\">{t}</button>"))
            .collect()
    }

    fn mc_activity(id: i64, max_votes: Value, opts: &[(i64, &str)]) -> Value {
        json!({
            "id": id, "shared_key": "key", "type": "multiple_choice_poll", "title": "Pick one",
            "max_votes": max_votes,
            "options": opts.iter().map(|(i, t)| json!({"id": i, "value": t})).collect::<Vec<_>>()
        })
    }

    fn clicks(page: &Page) -> Value {
        page.eval("window.clicks || []").unwrap()
    }

    fn current(page: &Page) -> Question {
        get_current_question(page)
            .unwrap()
            .expect("a live question")
    }

    #[test]
    fn current_question_none_without_marker() {
        let Some(f) = launch("nomarker") else { return };
        let page = f.browser.page();
        page.set_content("<html><body>Waiting for the presentation to start</body></html>")
            .unwrap();
        assert_eq!(get_current_question(page).unwrap(), None);
        f.finish();
    }

    #[test]
    fn submit_answer_flows() {
        let Some(f) = launch("submit") else { return };
        let page = f.browser.page();

        // Multiple choice: "12" comes first in the DOM, answer "2" must hit "2".
        let opts = [(1, "12"), (2, "2")];
        let html = poll_page(
            mc_activity(100, json!(1), &opts),
            &vote_buttons(&["12", "2"]),
        );
        page.set_content(&html).unwrap();
        let q = current(page);
        assert_eq!(q.qtype, "multiple_choice");
        assert_eq!(q.question, "Pick one");
        assert_eq!(
            (q.question_id, q.poll_key.as_deref()),
            (Some(100), Some("key"))
        );
        assert_eq!(q.options.len(), 2);
        let done = submit_answer(page, &q, &json!("2")).unwrap();
        assert_eq!(done, json!({"id": 2, "text": "2"}));
        assert_eq!(clicks(page), json!(["2"]), "exact text match");

        // By option id.
        page.eval("window.clicks = []").unwrap();
        submit_answer(page, &q, &json!(1)).unwrap();
        assert_eq!(clicks(page), json!(["12"]), "by id");

        // A bad answer is an Answer error and clicks nothing.
        page.eval("window.clicks = []").unwrap();
        match submit_answer(page, &q, &json!("nope")) {
            Err(SubmitError::Answer(m)) => {
                assert!(m.contains("doesn't match any current option"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(clicks(page), json!([]));

        // select_multiple clicks both, in order.
        let opts = [(1, "Red"), (2, "Green"), (3, "Blue")];
        let html = poll_page(
            mc_activity(101, Value::Null, &opts),
            &vote_buttons(&["Red", "Green", "Blue"]),
        );
        page.set_content(&html).unwrap();
        let q = current(page);
        assert_eq!(q.qtype, "select_multiple");
        let done = submit_answer(page, &q, &json!([3, "Red"])).unwrap();
        assert_eq!(clicks(page), json!(["Blue", "Red"]));
        assert_eq!(done.as_array().unwrap().len(), 2);
        assert!(matches!(
            submit_answer(page, &q, &json!([])),
            Err(SubmitError::Answer(_))
        ));

        // Short answer: replaces what's in the box, then clicks submit.
        let html = poll_page(
            json!({"id": 102, "type": "free_text_poll", "title": "Why?"}),
            "<textarea name='response'>old text</textarea>\
             <input type='submit' onclick=\"window.sent = document.querySelector('textarea').value\">",
        );
        page.set_content(&html).unwrap();
        let q = current(page);
        assert_eq!(q.qtype, "short_answer");
        assert_eq!(
            submit_answer(page, &q, &json!("hello\nworld")).unwrap(),
            json!("hello\nworld")
        );
        assert_eq!(page.eval("window.sent").unwrap(), json!("hello\nworld"));
        assert!(matches!(
            submit_answer(page, &q, &json!("   ")),
            Err(SubmitError::Answer(_))
        ));

        // Clickable image: (0.25, 0.75) of a 400x200 image is (100, 150).
        let html = poll_page(
            json!({"id": 103, "type": "clickable_image", "title": "Where?", "max_votes": 1,
                   "type_options": {"imageUrl": "http://img/x.png"}}),
            "<img class='component-response-clickable-image__image' \
             src='data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7' \
             style='display:block;margin:20px 0 0 30px;width:400px;height:200px' \
             onclick=\"window.pt = [event.offsetX, event.offsetY]\">",
        );
        page.set_content(&html).unwrap();
        let q = current(page);
        assert_eq!(q.qtype, "clickable_image");
        assert_eq!(q.image_url.as_deref(), Some("http://img/x.png"));
        let done = submit_answer(page, &q, &json!([{"x": 0.25, "y": 0.75}])).unwrap();
        assert_eq!(done, json!([{"x": 0.25, "y": 0.75}]));
        let pt = page.eval("window.pt").unwrap();
        let (px, py) = (pt[0].as_f64().unwrap(), pt[1].as_f64().unwrap());
        assert!(
            (px - 100.0).abs() <= 3.0 && (py - 150.0).abs() <= 3.0,
            "{px},{py}"
        );
        assert!(matches!(
            submit_answer(page, &q, &json!([{"x": 1.5, "y": 0.0}])),
            Err(SubmitError::Answer(_))
        ));

        // Stale: the live question id changed since `q` was read.
        let html = poll_page(
            mc_activity(100, json!(1), &opts),
            &vote_buttons(&["Red", "Green", "Blue"]),
        );
        page.set_content(&html).unwrap();
        let old = current(page);
        let html = poll_page(
            mc_activity(999, json!(1), &opts),
            &vote_buttons(&["Red", "Green", "Blue"]),
        );
        page.set_content(&html).unwrap();
        page.eval("window.clicks = []").unwrap();
        match submit_answer(page, &old, &json!("Red")) {
            Err(SubmitError::Stale(m)) => assert!(m.contains("100") && m.contains("999"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(clicks(page), json!([]));
        // ...and no question at all is stale too.
        page.set_content("<body>idle</body>").unwrap();
        assert!(matches!(
            submit_answer(page, &old, &json!("Red")),
            Err(SubmitError::Stale(_))
        ));
        f.finish();
    }

    #[test]
    fn click_falls_back_to_undo() {
        let Some(f) = launch("undo") else { return };
        let page = f.browser.page();
        // The option stays disabled until the undo button is pressed.
        let body = format!(
            "<button id='a' class='{VOTE}' disabled onclick=\"{RECORD}\">A</button>\
             <button class='{UNDO}' onclick=\"document.getElementById('a').disabled = false\">Undo</button>"
        );
        let html = poll_page(mc_activity(1, json!(1), &[(1, "A")]), &body);
        page.set_content(&html).unwrap();
        let q = current(page);
        submit_answer(page, &q, &json!("A")).unwrap();
        assert_eq!(clicks(page), json!(["A"]));
        f.finish();
    }

    #[test]
    fn option_match_survives_css_uppercase_and_waits_out_disabled_window() {
        let Some(f) = launch("optcase") else { return };
        let page = f.browser.page();
        // "Banana" comes first and contains "a"; the real "a" button is styled
        // uppercase, so its innerText is "A". It must still win over the substring hit.
        let body = format!(
            "<button class='{VOTE}' onclick=\"{RECORD}\">Banana</button>\
             <button class='{VOTE}' style='text-transform:uppercase' onclick=\"{RECORD}\">a</button>"
        );
        let html = poll_page(mc_activity(1, json!(1), &[(1, "Banana"), (2, "a")]), &body);
        page.set_content(&html).unwrap();
        let q = current(page);
        submit_answer(page, &q, &json!("a")).unwrap();
        assert_eq!(clicks(page), json!(["A"]));

        page.eval("window.clicks = []").unwrap();
        // Every option is disabled for ~300ms while a vote is in flight.
        let body = format!(
            "<button id='x' class='{VOTE}' disabled onclick=\"{RECORD}\">Yes</button>\
             <script>setTimeout(() => document.getElementById('x').disabled = false, 400)</script>"
        );
        let html = poll_page(mc_activity(2, json!(1), &[(1, "Yes")]), &body);
        page.set_content(&html).unwrap();
        let q = current(page);
        submit_answer(page, &q, &json!("Yes")).unwrap();
        assert_eq!(clicks(page), json!(["Yes"]));
        f.finish();
    }

    #[test]
    fn image_click_waits_for_the_image_to_appear() {
        let Some(f) = launch("lateimg") else { return };
        let page = f.browser.page();
        let html = poll_page(
            json!({"id": 5, "type": "clickable_image", "title": "Where?", "max_votes": 1}),
            "<div id='slot'></div><script>setTimeout(() => { document.getElementById('slot').innerHTML = \
             \"<img class='component-response-clickable-image__image' \
             src='data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7' \
             style='display:block;width:200px;height:100px' onclick='window.hit = 1'>\"; }, 700)</script>",
        );
        page.set_content(&html).unwrap();
        let q = current(page);
        submit_answer(page, &q, &json!([{"x": 0.5, "y": 0.5}])).unwrap();
        assert_eq!(page.eval("window.hit").unwrap(), json!(1));
        f.finish();
    }

    #[test]
    fn sso_chooser_prefers_cornell_over_prefilled_email() {
        let Some(f) = launch("chooser") else { return };
        let page = f.browser.page();
        // id.polleverywhere.com/login/password: a prefilled readonly email and
        // several submit buttons; only "Log in with Cornell Net ID" may be clicked.
        page.set_content(
            "<input id='username' type='email' readonly value='abc123@cornell.edu'>\
             <button type='submit' onclick='window.pw=1'>Use password instead</button>\
             <button type='submit' onclick='window.cornell=1'>Log in with Cornell Net ID</button>",
        )
        .unwrap();
        let mut setup = PageSetup::default();
        setup.tick(page, "abc123", false).unwrap();
        assert_eq!(page.eval("window.cornell").unwrap(), json!(1));
        assert_eq!(page.eval("window.pw").unwrap(), Value::Null);
        f.finish();
    }

    #[test]
    fn pollev_url_check_ignores_query_and_fragment() {
        assert!(url_on_pollev("https://pollev.com/class"));
        assert!(url_on_pollev("http://127.0.0.1:1/pollev.com/class"));
        assert!(url_on_pollev("https://www.pollev.com/class?x=1#y"));
        assert!(!url_on_pollev("about:blank"));
        assert!(!url_on_pollev(
            "https://id.polleverywhere.com/login?legacy_only=true"
        ));
        assert!(!url_on_pollev(
            "https://id.polleverywhere.com/login?return_to=https%3A%2F%2Fpollev.com%2Fclass"
        ));
        assert!(!url_on_pollev(
            "https://shibidp.cit.cornell.edu/idp/profile/SAML2/Redirect/SSO#pollev.com"
        ));
    }

    #[test]
    fn page_setup_cookie_and_screen_name_once() {
        let Some(f) = launch("setup-name") else {
            return;
        };
        let port = serve(200, b"<html><body>x</body></html>".to_vec());
        let page = f.browser.page();
        page.navigate(&format!("http://127.0.0.1:{port}/pollev.com/class"))
            .unwrap();
        page.set_content(
            "<body><button onclick=\"window.cookie=(window.cookie||0)+1\">Agree</button>\
             <input type='text' id='n' placeholder='Screen name'>\
             <button onclick=\"window.cont=(window.cont||0)+1; window.name=document.getElementById('n').value\">Continue</button></body>",
        )
        .unwrap();
        let mut setup = PageSetup::default();
        let st = setup.tick(page, "abc123", false).unwrap();
        assert!(st.on_pollev && !st.wall && !st.awaiting_human);
        assert_eq!(page.eval("window.cookie").unwrap(), json!(1));
        assert_eq!(
            page.eval("window.cont").unwrap(),
            Value::Null,
            "one action per tick"
        );
        for _ in 0..4 {
            setup.tick(page, "abc123", false).unwrap();
        }
        assert_eq!(page.eval("window.cookie").unwrap(), json!(1), "cookie once");
        assert_eq!(page.eval("window.cont").unwrap(), json!(1), "continue once");
        assert_eq!(page.eval("window.name").unwrap(), json!("abc123"));
        f.finish();
    }

    #[test]
    fn page_setup_skips_name_after_question_and_wall_login_scoped() {
        let Some(f) = launch("setup-wall") else {
            return;
        };
        let port = serve(200, b"<html><body>x</body></html>".to_vec());
        let page = f.browser.page();
        page.navigate(&format!("http://127.0.0.1:{port}/pollev.com/class"))
            .unwrap();
        // A question was seen: no screen-name step.
        page.set_content(
            "<input type='text' id='n'><button onclick=\"window.cont=1\">Continue</button>",
        )
        .unwrap();
        let mut setup = PageSetup::default();
        setup.tick(page, "abc123", true).unwrap();
        assert_eq!(page.eval("window.cont").unwrap(), Value::Null);
        assert_eq!(
            page.eval("document.getElementById('n').value").unwrap(),
            json!("")
        );

        // The wall: only a.mdc-button "Log in" is clicked, not the nav link; once per 5s.
        page.set_content(
            "<a href='#top' onclick=\"window.navClicked=1\">Log in</a>\
             <h1>Pre-registration required</h1>\
             <a class='mt-8 mdc-button' onclick=\"window.wall=(window.wall||0)+1\">Log in</a>",
        )
        .unwrap();
        let mut setup = PageSetup::default();
        for _ in 0..3 {
            let st = setup.tick(page, "abc123", false).unwrap();
            assert!(st.wall && st.on_pollev);
        }
        assert_eq!(page.eval("window.wall").unwrap(), json!(1));
        assert_eq!(page.eval("window.navClicked").unwrap(), Value::Null);
        f.finish();
    }

    #[test]
    fn page_setup_sso_identifier_steps_and_password_stop() {
        let Some(f) = launch("setup-sso") else { return };
        let page = f.browser.page();
        // about:blank is off pollev.com, like an SSO page.
        page.set_content(
            "<input id='email' type='email'>\
             <input type='submit' value='Continue' onclick=\"window.sub=(window.sub||0)+1; window.email=document.getElementById('email').value\">",
        )
        .unwrap();
        let mut setup = PageSetup::default();
        let st = setup.tick(page, "abc123", false).unwrap();
        assert!(!st.on_pollev && !st.wall && !st.awaiting_human);
        assert_eq!(
            page.eval("window.email").unwrap(),
            json!("abc123@cornell.edu")
        );
        // Same URL within 2s: no repeat.
        setup.tick(page, "abc123", false).unwrap();
        assert_eq!(page.eval("window.sub").unwrap(), json!(1));

        // Chooser, then NetID page.
        page.set_content("<button type='submit' onclick=\"window.cornell=1\">Log in with Cornell Net ID</button>")
            .unwrap();
        let mut setup = PageSetup::default();
        setup.tick(page, "abc123", false).unwrap();
        assert_eq!(page.eval("window.cornell").unwrap(), json!(1));
        page.set_content(
            "<input id='user_identifier'>\
             <button>Other</button><button type='submit' onclick=\"window.netid=document.getElementById('user_identifier').value\">Continue</button>",
        )
        .unwrap();
        let mut setup = PageSetup::default();
        setup.tick(page, "abc123", false).unwrap();
        assert_eq!(page.eval("window.netid").unwrap(), json!("abc123"));

        // A password field: the human's turn, nothing is automated.
        page.set_content(
            "<input type='password'><input id='email' type='email'>\
             <input type='submit' onclick=\"window.bad=1\">",
        )
        .unwrap();
        let mut setup = PageSetup::default();
        let st = setup.tick(page, "abc123", false).unwrap();
        assert!(st.awaiting_human);
        assert_eq!(page.eval("window.bad").unwrap(), Value::Null);
        assert_eq!(
            page.eval("document.getElementById('email').value").unwrap(),
            json!("")
        );
        f.finish();
    }

    #[test]
    fn download_image_saves_bytes_and_fails_on_http_error() {
        let dir = std::env::temp_dir().join(format!("pollev-poll-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let ok = serve(200, vec![1, 2, 3, 4, 5]);
        let dest = dir.join("img/a.png");
        download_image(&format!("http://127.0.0.1:{ok}/a.png"), &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), vec![1, 2, 3, 4, 5]);
        let bad = serve(404, b"nope".to_vec());
        assert!(
            download_image(&format!("http://127.0.0.1:{bad}/a.png"), &dir.join("b.png")).is_err()
        );
        assert!(!dir.join("b.png").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_marked_image_screenshots_image_size() {
        let Some(f) = launch("marked") else { return };
        let dir = std::env::temp_dir().join(format!("pollev-poll-mark-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Make a real 120x80 PNG with Chrome itself.
        let page = f.browser.page();
        page.set_content("<body style='margin:0'><div style='width:120px;height:80px;background:#c00'></div></body>")
            .unwrap();
        let src = dir.join("src.png");
        std::fs::write(
            &src,
            page.screenshot_png(Some((0.0, 0.0, 120.0, 80.0))).unwrap(),
        )
        .unwrap();
        let dest = dir.join("marked.png");
        render_marked_image(&f.browser, &src, &[(0.25, 0.75), (0.5, 0.5)], &dest).unwrap();
        let png = std::fs::read(&dest).unwrap();
        assert_eq!(&png[..4], b"\x89PNG");
        let dim = |b: &[u8]| {
            (
                u32::from_be_bytes([b[16], b[17], b[18], b[19]]),
                u32::from_be_bytes([b[20], b[21], b[22], b[23]]),
            )
        };
        assert_eq!(dim(&png), dim(&std::fs::read(&src).unwrap()));
        // A non-image fails instead of writing garbage.
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"not an image").unwrap();
        assert!(render_marked_image(&f.browser, &junk, &[], &dir.join("x.png")).is_err());
        assert!(!dir.join("x.png").exists());
        // The throwaway page is gone and the browser is fine.
        assert!(f.browser.is_alive());
        let _ = std::fs::remove_dir_all(&dir);
        f.finish();
    }
}
