//! macOS notification + sound, ntfy push, iMessage send, and waiting for an
//! iMessage reply via chat.db. Port of textwatcher.py's notify helpers and
//! ai_answer._wait_for_imessage_reply. See PORT_SPEC.md "notify".
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// (title, message, url). Best effort: never panics, never blocks long.
pub type NotifyFn = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;

/// Every text actually sent, so a reply-waiter can skip Messages' echo of
/// our own outgoing texts when self-texting.
pub type SentTexts = Arc<Mutex<HashSet<String>>>;

#[derive(Clone, Debug, Default)]
pub struct Notifier {
    pub mac: bool,
    pub sound: bool,
    /// None or empty = ntfy off.
    pub ntfy_topic: Option<String>,
    /// Phone or iMessage email; None or empty = iMessage off.
    pub imessage: Option<String>,
    pub sent: SentTexts,
}

impl Notifier {
    /// textwatcher.notify_all.
    pub fn notify_all(&self, title: &str, message: &str, url: &str) {
        if self.mac {
            mac_notify(title, message);
        }
        if self.sound {
            mac_sound();
        }
        let tail = if url.is_empty() {
            String::new()
        } else {
            format!("\n{url}")
        };
        self.send_imessage(&format!("{title}: {message}{tail}"));
        if let Some(topic) = self.ntfy_topic.as_deref() {
            ntfy(topic, title, message, url);
        }
    }

    pub fn send_imessage(&self, text: &str) {
        let Some(to) = self.recipient() else { return };
        self.record(text);
        let script = format!(
            "{}  send \"{}\" to targetBuddy\nend tell",
            imessage_head(to),
            applescript_escape(text)
        );
        run_osascript(&script);
    }

    /// Optional caption text, then the image as a file attachment.
    pub fn send_imessage_image(&self, path: &Path, caption: &str) {
        let Some(to) = self.recipient() else { return };
        let Ok(path) = path.canonicalize() else {
            eprintln!(
                "[imessage] image not found, skipping picture: {}",
                path.display()
            );
            return;
        };
        let mut script = imessage_head(to);
        if !caption.is_empty() {
            self.record(caption);
            script += &format!(
                "  send \"{}\" to targetBuddy\n",
                applescript_escape(caption)
            );
        }
        script += &format!(
            "  send POSIX file \"{}\" to targetBuddy\nend tell",
            applescript_escape(&path.to_string_lossy())
        );
        run_osascript(&script);
    }

    pub fn into_fn(self: Arc<Self>) -> NotifyFn {
        Arc::new(move |t, m, u| self.notify_all(t, m, u))
    }

    fn recipient(&self) -> Option<&str> {
        self.imessage
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "SET ME")
    }

    fn record(&self, text: &str) {
        if let Ok(mut s) = self.sent.lock() {
            s.insert(text.to_string());
        }
    }
}

fn imessage_head(to: &str) -> String {
    format!(
        "tell application \"Messages\"\n  set targetService to 1st service whose service type = iMessage\n  set targetBuddy to buddy \"{}\" of targetService\n",
        applescript_escape(to)
    )
}

/// Run a command, killing it after 15s so a stuck Messages/osascript can't
/// stall the watcher. Failures are logged, never raised.
fn run_bounded(cmd: &mut Command) {
    let mut child = match cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Python let osascript's stderr through; nulling it hid AppleScript
        // syntax errors (-2741) exactly like the bug the escape comment names.
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return eprintln!("[notify] spawn failed: {e}"),
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if !st.success() => return eprintln!("[notify] command failed: {st}"),
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if start.elapsed() > Duration::from_secs(15) => {
                let _ = child.kill();
                let _ = child.wait();
                return eprintln!("[notify] command timed out");
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
        }
    }
}

fn run_osascript(script: &str) {
    run_bounded(Command::new("osascript").arg("-e").arg(script));
}

/// Escape for an AppleScript string literal: backslash, quote, \n, \r.
/// A raw line break inside the literal is a syntax error (-2741).
pub fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

pub fn mac_notify(title: &str, message: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_escape(message),
        applescript_escape(title)
    );
    run_osascript(&script);
}

pub fn mac_sound() {
    let p = "/System/Library/Sounds/Glass.aiff";
    if Path::new(p).exists() {
        run_bounded(Command::new("afplay").arg(p));
    }
}

pub fn ntfy(topic: &str, title: &str, message: &str, url: &str) {
    if topic.is_empty() {
        return;
    }
    let body = if url.is_empty() {
        message.to_string()
    } else {
        format!("{message}\n{url}")
    };
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .new_agent();
    if let Err(e) = agent
        .post(&format!("https://ntfy.sh/{topic}"))
        .header("Title", title)
        .send(body.as_bytes())
    {
        eprintln!("[ntfy] failed: {e}");
    }
}

/// Block until a new incoming message from `handle` arrives after this call
/// started (skipping exact matches in `sent`), returning its text, or until
/// `stop` is set (Ok(None)). Polls ~/Library/Messages/chat.db every `poll`.
/// Needs Full Disk Access; without it, Err with a clear message.
pub fn wait_for_imessage_reply(
    handle: &str,
    sent: &SentTexts,
    poll: Duration,
    stop: &AtomicBool,
) -> Result<Option<String>> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let db = Path::new(&home).join("Library/Messages/chat.db");
    wait_in_db(&db, handle, sent, poll, stop)
}

fn wait_in_db(
    db: &Path,
    handle: &str,
    sent: &SentTexts,
    poll: Duration,
    stop: &AtomicBool,
) -> Result<Option<String>> {
    // A zero interval would otherwise busy-loop on the db.
    let poll = poll.max(Duration::from_millis(10));
    let fda = |e: rusqlite::Error| {
        anyhow::anyhow!(
            "can't read chat.db ({e}); grant Full Disk Access to this app in System Settings > Privacy & Security"
        )
    };
    let conn = Connection::open_with_flags(
        db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(fda)?;
    let mut baseline: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(message.ROWID), 0) FROM message JOIN handle ON message.handle_id = handle.ROWID WHERE handle.id = ?1",
            [handle],
            |r| r.get(0),
        )
        .map_err(fda)?;
    loop {
        // Sleep in short slices so Stop is honored quickly.
        let until = Instant::now() + poll;
        while Instant::now() < until {
            if stop.load(Ordering::SeqCst) {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20).min(poll));
        }
        if stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let mut stmt = conn
            .prepare(
                "SELECT message.ROWID, text FROM message JOIN handle ON message.handle_id = handle.ROWID \
                 WHERE handle.id = ?1 AND message.is_from_me = 0 AND message.ROWID > ?2 ORDER BY message.ROWID ASC",
            )
            .map_err(fda)?;
        let rows: Vec<(i64, Option<String>)> = stmt
            .query_map(rusqlite::params![handle, baseline], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .map_err(fda)?
            .collect::<rusqlite::Result<_>>()
            .map_err(fda)?;
        for (rowid, text) in rows {
            baseline = rowid;
            // NULL text = attachment/delivery row, never a typed reply.
            let Some(text) = text else { continue };
            if sent.lock().map(|s| s.contains(&text)).unwrap_or(false) {
                continue;
            }
            return Ok(Some(text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let p =
            std::env::temp_dir().join(format!("pollev_notify_{name}_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE message (ROWID INTEGER PRIMARY KEY, handle_id INTEGER, text TEXT, is_from_me INTEGER);
             INSERT INTO handle VALUES (1, '+15551234567');
             INSERT INTO message VALUES (1, 1, 'old', 0);",
        )
        .unwrap();
        p
    }

    fn insert(p: &Path, text: &str) {
        let c = Connection::open(p).unwrap();
        c.execute(
            "INSERT INTO message (handle_id, text, is_from_me) VALUES (1, ?1, 0)",
            [text],
        )
        .unwrap();
    }

    #[test]
    fn escaping() {
        assert_eq!(applescript_escape("a\"b\\c\nd\re"), "a\\\"b\\\\c\\nd\\re");
    }

    #[test]
    fn returns_new_row_and_skips_echo() {
        let p = temp_db("new");
        let sent: SentTexts = Arc::default();
        sent.lock().unwrap().insert("echo".into());
        let p2 = p.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            insert(&p2, "echo");
            insert(&p2, "yes please");
        });
        let stop = AtomicBool::new(false);
        let got = wait_in_db(&p, "+15551234567", &sent, Duration::from_millis(50), &stop).unwrap();
        t.join().unwrap();
        assert_eq!(got.as_deref(), Some("yes please"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stop_returns_none() {
        let p = temp_db("stop");
        let sent: SentTexts = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            s2.store(true, Ordering::SeqCst);
        });
        let got = wait_in_db(&p, "+15551234567", &sent, Duration::from_secs(5), &stop).unwrap();
        t.join().unwrap();
        assert_eq!(got, None);
        let _ = std::fs::remove_file(&p);
    }

    fn insert_raw(p: &Path, handle_id: i64, text: Option<&str>, from_me: i64) {
        let c = Connection::open(p).unwrap();
        c.execute(
            "INSERT INTO message (handle_id, text, is_from_me) VALUES (?1, ?2, ?3)",
            rusqlite::params![handle_id, text, from_me],
        )
        .unwrap();
    }

    #[test]
    fn only_exact_echo_and_real_incoming_rows_count() {
        let p = temp_db("filter");
        Connection::open(&p)
            .unwrap()
            .execute("INSERT INTO handle VALUES (2, 'other')", [])
            .unwrap();
        let sent: SentTexts = Arc::default();
        sent.lock().unwrap().insert("echo".into());
        let p2 = p.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            insert_raw(&p2, 1, Some("mine"), 1); // outgoing
            insert_raw(&p2, 2, Some("stranger"), 0); // other handle
            insert_raw(&p2, 1, None, 0); // attachment/delivery row
            insert_raw(&p2, 1, Some("echo"), 0); // exact echo
            insert_raw(&p2, 1, Some("echo!"), 0); // not exact: a real reply
        });
        let stop = AtomicBool::new(false);
        let got = wait_in_db(&p, "+15551234567", &sent, Duration::from_millis(50), &stop).unwrap();
        t.join().unwrap();
        assert_eq!(got.as_deref(), Some("echo!"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn stop_is_prompt_even_with_long_poll_and_zero_poll_is_ok() {
        let p = temp_db("prompt");
        let sent: SentTexts = Arc::default();
        let stop = AtomicBool::new(true);
        let t0 = Instant::now();
        let got = wait_in_db(&p, "+15551234567", &sent, Duration::from_secs(30), &stop).unwrap();
        assert_eq!(got, None);
        assert!(t0.elapsed() < Duration::from_secs(2));
        let got = wait_in_db(&p, "+15551234567", &sent, Duration::ZERO, &stop).unwrap();
        assert_eq!(got, None);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn disabled_imessage_sends_and_records_nothing() {
        // Early-return paths only: nothing here may reach osascript.
        for to in [None, Some(""), Some("  "), Some("SET ME")] {
            let n = Notifier {
                imessage: to.map(String::from),
                ..Default::default()
            };
            n.send_imessage("hi");
            n.send_imessage_image(Path::new("/nonexistent.png"), "cap");
            n.notify_all("t", "m", "");
            assert!(n.sent.lock().unwrap().is_empty(), "{to:?}");
        }
    }

    #[test]
    fn missing_db_errors_with_fda_hint() {
        let stop = AtomicBool::new(false);
        let e = wait_in_db(
            Path::new("/nonexistent/chat.db"),
            "x",
            &Arc::default(),
            Duration::from_millis(10),
            &stop,
        )
        .unwrap_err();
        assert!(e.to_string().contains("Full Disk Access"));
    }
}
