//! One browser tab: protocol calls, events (console, network, dialogs), waiting, navigation and
//! the keyboard. Element actions are in [`super::locator`].

use super::cdp::Connection;
use super::route::{Route, cdp_pattern};
use super::{CALL_TIMEOUT, POLL, R, fail, keys};
use crate::runtime::Shared;
use crate::runtime::builtins::base64_decode;
use serde_json::{Value as Json, json};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// After loading, a page counts as settled once no request was in flight for this long.
const QUIET: Duration = Duration::from_millis(300);

/// How long navigation waits for the network to settle before going on anyway (a page with a
/// long poll never settles).
const SETTLE_MAX: Duration = Duration::from_secs(3);

/// Timeouts shared by a browser context and its pages; `browser(timeout: …)` changes them.
pub struct Timeouts {
    /// Actions and navigation: Playwright leaves them to the test timeout, but klausur tests
    /// have none by default, so they get 30 s.
    pub action: Cell<Duration>,
    /// How long `expect` retries a page expectation; 5 s like Playwright's `expect`.
    pub expect: Cell<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            action: Cell::new(Duration::from_secs(30)),
            expect: Cell::new(Duration::from_secs(5)),
        }
    }
}

#[derive(Clone)]
pub struct Page(pub(super) Rc<PageInner>);

pub(super) struct PageInner {
    pub conn: Connection,
    pub session: String,
    pub target: String,
    /// Console errors and warnings, uncaught exceptions and accepted dialogs.
    pub console: Arc<Mutex<Vec<String>>>,
    pub net: Arc<Mutex<Net>>,
    pub number: usize,
    pub closed: Cell<bool>,
    pub timeouts: Rc<Timeouts>,
    /// Accept dialogs instead of dismissing them (`page.accept_dialogs()`).
    pub accept_dialogs: Arc<AtomicBool>,
    /// `page.route(…)`, and the context's `browser().route(…)`; the page's come first.
    pub routes: Routes,
    pub context_routes: Routes,
}

pub type Routes = Arc<Mutex<Vec<Route>>>;

/// A request the page made, for `page.requests(glob)`.
#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub url: String,
    pub body: Option<String>,
    /// The response status, or why the request failed.
    pub outcome: Option<Result<u16, String>>,
}

/// The page's network traffic: what's in flight, for waiting until it settles, and what failed,
/// for failure reports.
#[derive(Default)]
pub(super) struct Net {
    /// Request id → "METHOD url".
    in_flight: HashMap<String, String>,
    last_change: Option<Instant>,
    pub failed: Vec<String>,
    pub requests: Vec<Request>,
    /// Request id → index in `requests`.
    index: HashMap<String, usize>,
}

impl Net {
    fn event(&mut self, method: &str, params: &Json) {
        let id = params["requestId"].as_str().unwrap_or_default().to_string();
        match method {
            "Network.requestWillBeSent" => {
                let r = &params["request"];
                let (method, url) = (
                    r["method"].as_str().unwrap_or("GET"),
                    r["url"].as_str().unwrap_or_default(),
                );
                // A redirect reuses the id: the earlier entry keeps its own URL.
                self.index.insert(id.clone(), self.requests.len());
                self.requests.push(Request {
                    method: method.to_string(),
                    url: url.to_string(),
                    body: r["postData"].as_str().map(str::to_string),
                    outcome: None,
                });
                // Streams stay open for as long as the page lives; they never settle.
                if matches!(params["type"].as_str(), Some("EventSource" | "WebSocket")) {
                    return;
                }
                self.in_flight.insert(id, format!("{method} {url}"));
            }
            "Network.responseReceived" => {
                let status = params["response"]["status"].as_u64().unwrap_or(0);
                if let Some(&i) = self.index.get(&id) {
                    self.requests[i].outcome = Some(Ok(status as u16));
                }
                if status >= 400
                    && let Some(what) = self.in_flight.get(&id)
                {
                    self.failed.push(format!("{status} {what}"));
                }
                return;
            }
            "Network.loadingFinished" => {
                self.in_flight.remove(&id);
            }
            "Network.loadingFailed" => {
                if let Some(&i) = self.index.get(&id) {
                    let why = params["errorText"].as_str().unwrap_or("failed");
                    self.requests[i].outcome = Some(Err(why.to_string()));
                }
                if let Some(what) = self.in_flight.remove(&id)
                    && params["canceled"].as_bool() != Some(true)
                {
                    let why = params["errorText"].as_str().unwrap_or("failed");
                    self.failed.push(format!("{why} {what}"));
                }
            }
            _ => return,
        }
        self.last_change = Some(Instant::now());
    }

    fn settled(&self) -> bool {
        self.in_flight.is_empty() && self.last_change.is_none_or(|t| t.elapsed() >= QUIET)
    }
}

/// Why a wait stopped early, as the start of an error message.
fn stopped(stop: &str) -> &'static str {
    if stop.contains("timed out") {
        "timed out"
    } else {
        "interrupted"
    }
}

impl Page {
    pub(super) fn call(&self, method: &str, params: Json) -> R<Json> {
        if self.0.closed.get() {
            return Err(fail("the page is closed"));
        }
        self.0
            .conn
            .call(Some(&self.0.session), method, params, CALL_TIMEOUT)
            .map_err(fail)
    }

    pub(super) fn listen(&self) {
        let console = self.0.console.clone();
        let net = self.0.net.clone();
        let conn = self.0.conn.clone();
        let session = self.0.session.clone();
        let accept = self.0.accept_dialogs.clone();
        let routes = [self.0.routes.clone(), self.0.context_routes.clone()];
        let push = move |line: String| {
            if let Ok(mut c) = console.lock() {
                c.push(line);
            }
        };
        self.0.conn.listen(
            &self.0.session.clone(),
            Arc::new(move |method, params| match method {
                m if m.starts_with("Network.") => {
                    if let Ok(mut n) = net.lock() {
                        n.event(m, params);
                    }
                }
                // A request matched a route's coarse pattern: answer it, or let it through.
                "Fetch.requestPaused" => {
                    let id = params["requestId"].as_str().unwrap_or_default();
                    let url = params["request"]["url"].as_str().unwrap_or_default();
                    // Like Playwright, the route added last wins, and the page's come first.
                    let answer = routes.iter().find_map(|r| {
                        let r = r.lock().ok()?;
                        r.iter()
                            .rev()
                            .find(|r| r.matches(url))
                            .map(|r| r.answer(id))
                    });
                    let (method, params) = answer
                        .unwrap_or_else(|| ("Fetch.continueRequest", json!({ "requestId": id })));
                    conn.send(Some(&session), method, params);
                }
                "Runtime.consoleAPICalled" => {
                    let kind = params["type"].as_str().unwrap_or_default();
                    if kind == "error" || kind == "warning" {
                        let args: Vec<String> = params["args"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|a| match &a["value"] {
                                Json::String(s) => s.clone(),
                                Json::Null => a["description"].as_str().unwrap_or("").to_string(),
                                v => v.to_string(),
                            })
                            .collect();
                        push(format!("console.{kind}: {}", args.join(" ")));
                    }
                }
                "Runtime.exceptionThrown" => {
                    let d = &params["exceptionDetails"];
                    let msg = d["exception"]["description"]
                        .as_str()
                        .or(d["text"].as_str())
                        .unwrap_or("an exception");
                    push(format!(
                        "uncaught {}",
                        msg.lines().next().unwrap_or_default()
                    ));
                }
                "Log.entryAdded" => {
                    let e = &params["entry"];
                    if matches!(e["level"].as_str(), Some("error" | "warning")) {
                        let url = e["url"]
                            .as_str()
                            .map(|u| format!(" ({u})"))
                            .unwrap_or_default();
                        push(format!("{}{url}", e["text"].as_str().unwrap_or_default()));
                    }
                }
                // A dialog blocks the page until it's answered. Like Playwright, dismiss it unless
                // the test asked to accept dialogs, and log it.
                "Page.javascriptDialogOpening" => {
                    let accepting = accept.load(Ordering::SeqCst);
                    push(format!(
                        "{} {}: {}",
                        params["type"].as_str().unwrap_or("dialog"),
                        if accepting { "accepted" } else { "dismissed" },
                        params["message"].as_str().unwrap_or_default()
                    ));
                    conn.send(
                        Some(&session),
                        "Page.handleJavaScriptDialog",
                        json!({ "accept": accepting }),
                    );
                }
                _ => {}
            }),
        );
    }

    pub(super) fn close(&self) {
        if !self.0.closed.replace(true) {
            self.0.conn.forget(&self.0.session);
            let _ = self.0.conn.call(
                None,
                "Target.closeTarget",
                json!({ "targetId": self.0.target }),
                CALL_TIMEOUT,
            );
        }
    }

    pub(super) fn console(&self) -> Vec<String> {
        self.0.console.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Tell Chrome which requests to pause for the routes, or to stop pausing when there are none.
    pub(super) fn sync_routes(&self) -> R<()> {
        let globs: Vec<String> = [&self.0.routes, &self.0.context_routes]
            .iter()
            .filter_map(|r| r.lock().ok())
            .flat_map(|r| r.iter().map(|r| r.glob.clone()).collect::<Vec<_>>())
            .collect();
        if globs.is_empty() {
            return self.call("Fetch.disable", json!({})).map(|_| ());
        }
        let patterns: Vec<Json> = globs
            .iter()
            .map(|g| json!({ "urlPattern": cdp_pattern(g), "requestStage": "Request" }))
            .collect();
        self.call("Fetch.enable", json!({ "patterns": patterns }))
            .map(|_| ())
    }

    pub(super) fn requests(&self) -> Vec<Request> {
        self.0
            .net
            .lock()
            .map(|n| n.requests.clone())
            .unwrap_or_default()
    }

    pub(super) fn failed_requests(&self) -> Vec<String> {
        self.0
            .net
            .lock()
            .map(|n| n.failed.clone())
            .unwrap_or_default()
    }

    /// Evaluate JavaScript and return its result as JSON (promises are awaited).
    pub(super) fn eval(&self, js: &str) -> R<Json> {
        let r = self.evaluate_raw(js, true)?;
        Ok(r["result"].get("value").cloned().unwrap_or(Json::Null))
    }

    /// `Runtime.evaluate`; `by_value: false` returns a handle (`result.objectId`) instead.
    pub(super) fn evaluate_raw(&self, js: &str, by_value: bool) -> R<Json> {
        let r = self.call(
            "Runtime.evaluate",
            json!({
                "expression": js,
                "returnByValue": by_value,
                "awaitPromise": true,
                "userGesture": true,
            }),
        )?;
        if let Some(ex) = r.get("exceptionDetails") {
            let msg = ex["exception"]["description"]
                .as_str()
                .or(ex["text"].as_str())
                .unwrap_or("an exception");
            return Err(fail(msg.lines().next().unwrap_or_default().to_string()));
        }
        Ok(r)
    }

    /// Retry `ready` until it gives a value, the action timeout passes, the test times out, or
    /// the run is interrupted. `ready` returns why it isn't ready yet, for the error.
    pub(super) fn wait_for<T>(
        &self,
        shared: &Shared,
        action: &str,
        mut ready: impl FnMut() -> R<Result<T, String>>,
    ) -> R<T> {
        shared.page_reads.set(shared.page_reads.get() + 1);
        let started = Instant::now();
        let mut until = started + self.0.timeouts.action.get();
        // Inside a retrying `expect`, waits end with its window.
        if let Some(window) = shared.expect_window.get() {
            until = until.min(window.max(started));
        }
        loop {
            let why = match ready()? {
                Ok(v) => return Ok(v),
                Err(why) => why,
            };
            if Instant::now() >= until {
                return Err(fail(format!(
                    "`{action}` gave up after {}: {why}",
                    crate::testing::humantime(started.elapsed())
                )));
            }
            if let Err(stop) = crate::runtime::request::wait(shared, POLL) {
                return Err(fail(format!(
                    "{} while `{action}` waited: {why}",
                    stopped(stop)
                )));
            }
        }
    }

    pub(super) fn navigate(&self, shared: &Shared, url: &str) -> R<()> {
        // Mark the old document, so the wait below can't mistake it for the new one.
        self.eval("window.__klausurOld = true")?;
        let r = self.call("Page.navigate", json!({ "url": url }))?;
        if let Some(e) = r.get("errorText").and_then(Json::as_str) {
            return Err(fail(format!("couldn't open {url}: {e}")));
        }
        self.wait_for(shared, &format!("navigate(\"{url}\")"), || {
            // During the switch to the new document an evaluation can fail; that's "not yet".
            let state = self
                .eval("window.__klausurOld ? 'old' : document.readyState")
                .unwrap_or(Json::Null);
            Ok(match state.as_str() {
                Some("complete") => Ok(()),
                Some("old") => Err("the page hasn't started loading".into()),
                _ => Err("the page is still loading".into()),
            })
        })?;
        // Playwright waits for `load` only. But frameworks such as SvelteKit and Next.js attach
        // their event handlers from scripts `load` doesn't wait for, so a click right after it
        // can hit a button that does nothing yet. Wait for the network to settle too.
        let started = Instant::now();
        while started.elapsed() < SETTLE_MAX.min(self.0.timeouts.action.get()) {
            if self.0.net.lock().map(|n| n.settled()).unwrap_or(true) {
                break;
            }
            if let Err(stop) = crate::runtime::request::wait(shared, POLL) {
                return Err(fail(format!(
                    "{} while `navigate(\"{url}\")` waited for the page to settle",
                    stopped(stop)
                )));
            }
        }
        Ok(())
    }

    /// Press a key, or a combination such as `Shift+Tab`, on whatever has focus.
    pub(super) fn press(&self, combo: &str) -> R<()> {
        let key = keys::parse(combo).map_err(fail)?;
        let mut down = json!({
            "type": if key.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "key": key.key,
            "code": key.code,
            "windowsVirtualKeyCode": key.key_code,
            "modifiers": key.modifiers,
        });
        if let Some(text) = &key.text {
            down["text"] = json!(text);
        }
        self.call("Input.dispatchKeyEvent", down)?;
        self.call(
            "Input.dispatchKeyEvent",
            json!({
                "type": "keyUp",
                "key": key.key,
                "code": key.code,
                "windowsVirtualKeyCode": key.key_code,
                "modifiers": key.modifiers,
            }),
        )?;
        Ok(())
    }

    pub(super) fn mouse(&self, kind: &str, x: f64, y: f64, clicks: u32) -> R<()> {
        self.call(
            "Input.dispatchMouseEvent",
            json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": clicks }),
        )
        .map(|_| ())
    }

    pub(super) fn screenshot_to(&self, path: &Path) -> Result<PathBuf, String> {
        let r = self
            .call("Page.captureScreenshot", json!({ "format": "png" }))
            .map_err(|e| e.to_string())?;
        let png = base64_decode(r["data"].as_str().unwrap_or_default())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, png).map_err(|e| e.to_string())?;
        Ok(path.to_path_buf())
    }

    /// A string property of the page, such as `location.href`; counts as reading the page.
    pub(super) fn read(&self, shared: &Shared, js: &str) -> R<String> {
        shared.page_reads.set(shared.page_reads.get() + 1);
        Ok(self.eval(js)?.as_str().unwrap_or_default().to_string())
    }
}
