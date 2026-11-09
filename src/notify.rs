//! macOS notification + sound, ntfy push, iMessage send, and waiting for an
//! iMessage reply via chat.db. Port of textwatcher.py's notify helpers and
//! ai_answer._wait_for_imessage_reply. See PORT_SPEC.md "notify".
use anyhow::Result;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
        let _ = (title, message, url);
        todo!()
    }

    pub fn send_imessage(&self, text: &str) {
        let _ = text;
        todo!()
    }

    /// Optional caption text, then the image as a file attachment.
    pub fn send_imessage_image(&self, path: &Path, caption: &str) {
        let _ = (path, caption);
        todo!()
    }

    pub fn into_fn(self: Arc<Self>) -> NotifyFn {
        todo!()
    }
}

/// Escape for an AppleScript string literal: backslash, quote, \n, \r.
pub fn applescript_escape(s: &str) -> String {
    let _ = s;
    todo!()
}

pub fn mac_notify(title: &str, message: &str) {
    let _ = (title, message);
    todo!()
}

pub fn mac_sound() {
    todo!()
}

pub fn ntfy(topic: &str, title: &str, message: &str, url: &str) {
    let _ = (topic, title, message, url);
    todo!()
}

/// Block until a new incoming message from `handle` arrives after this call
/// started (skipping exact matches in `sent`), returning its text, or until
/// `stop` is set (Ok(None)). Polls ~/Library/Messages/chat.db every `poll`.
/// Needs Full Disk Access; without it, Err with a clear message.
pub fn wait_for_imessage_reply(handle: &str, sent: &SentTexts, poll: Duration, stop: &AtomicBool) -> Result<Option<String>> {
    let _ = (handle, sent, poll, stop);
    todo!()
}
