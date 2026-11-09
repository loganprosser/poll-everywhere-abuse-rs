//! Poll Everywhere Abuse: watch a PollEv presenter page, notify, and
//! optionally answer with AI. Rust port of the Python app; see PORT_SPEC.md.
pub mod ai;
pub mod cdp;
pub mod config;
pub mod gui;
pub mod notify;
pub mod poll;
pub mod watcher;
