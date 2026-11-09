//! Minimal Chrome DevTools Protocol client: launch a Chrome-family browser
//! with a persistent profile and drive one page over a websocket with
//! untyped JSON. Replaces Playwright. See PORT_SPEC.md "cdp".
use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;

pub struct LaunchOptions {
    pub headless: bool,
    pub profile_dir: PathBuf,
    /// None = find_chrome().
    pub chrome_path: Option<PathBuf>,
}

/// Executable of the first Chrome-family browser found (see PORT_SPEC.md).
pub fn find_chrome() -> Option<PathBuf> {
    todo!()
}

/// A running browser. Dropping it closes Chrome (gracefully first, so the
/// profile flushes cookies, then kill).
pub struct Browser {}

/// One page target. All methods take &self (the websocket sits behind a
/// Mutex), and Page is Send + Sync.
pub struct Page {}

impl Browser {
    pub fn launch(opts: &LaunchOptions) -> Result<Browser> {
        let _ = opts;
        todo!()
    }

    /// The page Chrome opened at launch.
    pub fn page(&self) -> &Page {
        todo!()
    }

    /// Some((lat, lon, accuracy_m)) = grant geolocation and override the
    /// position; None = deny the permission outright.
    pub fn set_geolocation(&self, geo: Option<(f64, f64, f64)>) -> Result<()> {
        let _ = geo;
        todo!()
    }

    /// A second, throwaway page (rendering the marked click image).
    pub fn new_page(&self) -> Result<Page> {
        todo!()
    }

    /// Chrome process still running and the page connection usable.
    pub fn is_alive(&self) -> bool {
        todo!()
    }
}

impl Page {
    /// Raw CDP command; protocol errors become Err.
    pub fn call(&self, method: &str, params: Value) -> Result<Value> {
        let _ = (method, params);
        todo!()
    }

    /// Runtime.evaluate with returnByValue + awaitPromise. JS exceptions
    /// become Err. Returns the value (Null for undefined).
    pub fn eval(&self, js: &str) -> Result<Value> {
        let _ = js;
        todo!()
    }

    /// Navigate and wait (up to 30s) for document.readyState == "complete".
    pub fn navigate(&self, url: &str) -> Result<()> {
        let _ = url;
        todo!()
    }

    /// Replace the document with `html` (Page.setDocumentContent).
    pub fn set_content(&self, html: &str) -> Result<()> {
        let _ = html;
        todo!()
    }

    pub fn url(&self) -> Result<String> {
        todo!()
    }

    /// Real mouse click (Input.dispatchMouseEvent) at viewport CSS pixels.
    pub fn click_at(&self, x: f64, y: f64) -> Result<()> {
        let _ = (x, y);
        todo!()
    }

    /// Retry until `finder_js` (a JS expression evaluating to an Element or
    /// null) is attached, visible and not disabled, scroll it to the
    /// viewport center, then click its center with click_at. Err after
    /// `timeout`. This replaces Playwright's auto-waiting click.
    pub fn click_element(&self, finder_js: &str, timeout: Duration) -> Result<()> {
        let _ = (finder_js, timeout);
        todo!()
    }

    /// Like click_element, but at a fraction (0..1) of the element's box.
    pub fn click_element_at(&self, finder_js: &str, fx: f64, fy: f64, timeout: Duration) -> Result<()> {
        let _ = (finder_js, fx, fy, timeout);
        todo!()
    }

    /// Type into the focused element (Input.insertText: trusted input events,
    /// so React-controlled fields see it).
    pub fn insert_text(&self, text: &str) -> Result<()> {
        let _ = text;
        todo!()
    }

    pub fn press_enter(&self) -> Result<()> {
        todo!()
    }

    /// Poll `js_bool` every 50ms until true (Ok(true)) or `timeout`
    /// (Ok(false)). Eval errors count as false (pages navigate mid-poll).
    pub fn wait_for(&self, js_bool: &str, timeout: Duration) -> Result<bool> {
        let _ = (js_bool, timeout);
        todo!()
    }

    /// PNG bytes; clip = (x, y, width, height) in CSS pixels.
    pub fn screenshot_png(&self, clip: Option<(f64, f64, f64, f64)>) -> Result<Vec<u8>> {
        let _ = clip;
        todo!()
    }

    pub fn close(&self) -> Result<()> {
        todo!()
    }
}
