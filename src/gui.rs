//! The window: a wry webview showing ui/index.html (the Python app's page,
//! reused as-is) with a small IPC shim that keeps its pywebview.api.* calls
//! working. Port of app/__main__.py + app/api.py. See PORT_SPEC.md "gui".
use anyhow::Result;
use serde_json::Value;

/// Open the window and run the event loop until it closes.
pub fn run() {
    todo!()
}

/// The JS-callable methods, by name with positional args; same names and
/// return shapes as app/api.py's Api.
pub struct Api {}

impl Api {
    pub fn new() -> Self {
        todo!()
    }

    pub fn call(&self, name: &str, args: &[Value]) -> Result<Value> {
        let _ = (name, args);
        todo!()
    }
}

impl Default for Api {
    fn default() -> Self {
        Self::new()
    }
}
