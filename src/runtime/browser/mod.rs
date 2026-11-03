//! Browser pages for end-to-end tests, driven over the Chrome DevTools Protocol.
//!
//! Tests only see `browser()` and `page` values; nothing here leaks protocol details, so the
//! backend can change later (e.g. to WebDriver BiDi). One Chrome process serves the whole run,
//! and every test case gets its own browser context (like an incognito window) that is closed
//! when the case ends.

mod cdp;
mod chrome;
mod keys;

use super::builtins::base64_decode;
use super::value;
use super::{Failure, PageReport, Shared};
use cdp::Connection;
use rhai::{Array, Dynamic, Engine, EvalAltResult};
use serde_json::{Value as Json, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type R<T> = Result<T, Box<EvalAltResult>>;

/// The Chrome process shared by all worker threads, started on first use.
static CHROME: Mutex<Option<Arc<chrome::Chrome>>> = Mutex::new(None);

/// Where failure screenshots go, relative to the working directory.
pub const RESULTS_DIR: &str = "klausur-results";

/// How long one protocol call may take. Navigation waits separately, in [`Page::navigate`].
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How often waits check the page again.
const POLL: Duration = Duration::from_millis(25);

/// After loading, a page counts as settled once no request was in flight for this long.
const QUIET: Duration = Duration::from_millis(300);

/// How long navigation waits for the network to settle before going on anyway (a page with a
/// long poll never settles).
const SETTLE_MAX: Duration = Duration::from_secs(3);

/// Stop Chrome. Called when klausur exits; contexts and pages die with it.
pub fn shutdown() {
    // `try_lock`: the Ctrl-C handler calls this too, and must not wait for a launch in progress.
    let chrome = CHROME.try_lock().map(|mut c| c.take()).ok().flatten();
    drop(chrome);
}

fn connection(headed: bool) -> Result<Connection, Failure> {
    let mut slot = CHROME.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = slot.as_ref() {
        return Ok(c.conn.clone());
    }
    let chrome = chrome::launch(headed).map_err(Failure::new)?;
    let conn = chrome.conn.clone();
    *slot = Some(Arc::new(chrome));
    Ok(conn)
}

/// The browser state of one test case (or of a `klausur run`): its context and open pages.
pub struct Session {
    conn: Connection,
    context: String,
    pages: RefCell<Vec<Page>>,
    /// How long actions wait and page expectations retry; `browser(timeout: …)` changes it.
    timeout: Rc<Cell<Duration>>,
}

impl Session {
    fn new(shared: &Shared) -> Result<Session, Failure> {
        let conn = connection(shared.config.headed)?;
        let context = conn
            .call(None, "Target.createBrowserContext", json!({}), CALL_TIMEOUT)
            .map_err(|e| Failure::new(format!("couldn't open a browser context: {e}")))?;
        Ok(Session {
            conn,
            context: context["browserContextId"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            pages: RefCell::default(),
            timeout: Rc::new(Cell::new(shared.config.browser_timeout)),
        })
    }

    fn open(&self, shared: &Shared, url: &str) -> R<Page> {
        let call = |method: &str, params: Json| {
            self.conn
                .call(None, method, params, CALL_TIMEOUT)
                .map_err(|e| fail(format!("couldn't open a page: {e}")))
        };
        let target = call(
            "Target.createTarget",
            json!({ "url": "about:blank", "browserContextId": self.context }),
        )?["targetId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let session = call(
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
        )?["sessionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let page = Page(Rc::new(PageInner {
            conn: self.conn.clone(),
            session,
            target,
            console: Arc::default(),
            net: Arc::default(),
            number: self.pages.borrow().len() + 1,
            closed: Cell::new(false),
            timeout: self.timeout.clone(),
        }));
        page.listen();
        for domain in [
            "Runtime.enable",
            "Log.enable",
            "Page.enable",
            "Network.enable",
        ] {
            page.call(domain, json!({}))?;
        }
        self.pages.borrow_mut().push(page.clone());
        page.navigate(shared, url)?;
        Ok(page)
    }

    /// Close every page and the context, so the next case starts without cookies or storage.
    pub fn close(&self) {
        for page in self.pages.borrow().iter() {
            page.close();
        }
        let _ = self.conn.call(
            None,
            "Target.disposeBrowserContext",
            json!({ "browserContextId": self.context }),
            CALL_TIMEOUT,
        );
    }

    /// Attach the state of the last open page to a failure: URL, title, console errors, and a
    /// screenshot saved under `klausur-results/`.
    pub fn describe_failure(&self, f: &mut Failure, name: &str) {
        let pages = self.pages.borrow();
        let Some(page) = pages.iter().rev().find(|p| !p.0.closed.get()) else {
            return;
        };
        let file = if pages.len() > 1 {
            format!("{name}-page{}.png", page.0.number)
        } else {
            format!("{name}.png")
        };
        let screenshot = page
            .screenshot_to(&Path::new(RESULTS_DIR).join(file))
            .map(|p| p.display().to_string());
        let read = |js: &str| {
            page.eval(js)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        };
        f.page = Some(Box::new(PageReport {
            url: read("location.href"),
            title: read("document.title"),
            screenshot,
            console: page.console(),
            failed_requests: page
                .0
                .net
                .lock()
                .map(|n| n.failed.clone())
                .unwrap_or_default(),
        }));
    }
}

/// A file name for a case's screenshot: the test name, plus a short hash of the case.
pub fn file_name(test: &str, case: &str) -> String {
    let mut name = test
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .to_lowercase();
    if !case.is_empty() {
        // FNV-1a: stable across runs and platforms, unlike the std hasher.
        let hash = case.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        name.push_str(&format!("-{:08x}", hash as u32));
    }
    name
}

#[derive(Clone)]
pub struct BrowserHandle(Rc<Session>);

#[derive(Clone)]
pub struct Page(Rc<PageInner>);

struct PageInner {
    conn: Connection,
    session: String,
    target: String,
    /// Console errors and warnings, uncaught exceptions and failed loads, for failure reports.
    console: Arc<Mutex<Vec<String>>>,
    net: Arc<Mutex<Net>>,
    number: usize,
    closed: Cell<bool>,
    timeout: Rc<Cell<Duration>>,
}

/// The page's network traffic: what's in flight, for waiting until it settles, and what failed,
/// for failure reports.
#[derive(Default)]
struct Net {
    /// Request id → "METHOD url".
    in_flight: HashMap<String, String>,
    last_change: Option<Instant>,
    failed: Vec<String>,
}

impl Net {
    fn event(&mut self, method: &str, params: &Json) {
        let id = params["requestId"].as_str().unwrap_or_default().to_string();
        match method {
            "Network.requestWillBeSent" => {
                // Streams stay open for as long as the page lives; they never settle.
                if matches!(params["type"].as_str(), Some("EventSource" | "WebSocket")) {
                    return;
                }
                let r = &params["request"];
                let what = format!(
                    "{} {}",
                    r["method"].as_str().unwrap_or("GET"),
                    r["url"].as_str().unwrap_or_default()
                );
                self.in_flight.insert(id, what);
            }
            "Network.responseReceived" => {
                let status = params["response"]["status"].as_u64().unwrap_or(0);
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

/// What the probe script found for a selector.
#[derive(Default)]
struct Probe {
    count: u64,
    visible: u64,
    /// Present when exactly one element is visible.
    element: Option<String>,
    enabled: bool,
    editable: bool,
    text: Option<String>,
    texts: Vec<String>,
    point: Option<(f64, f64)>,
    covered_by: Option<String>,
}

/// Finds the elements matching a selector and reports whether the single visible one can be
/// acted on. `want` picks the extra checks: "point" scrolls it into view and hit-tests its
/// centre, "editable" also checks it takes text, "text"/"texts" read text.
const PROBE: &str = r#"
(sel, want) => {
  let all;
  try { all = Array.from(document.querySelectorAll(sel)); }
  catch (e) { return { invalid: String(e.message) }; }
  const describe = el => {
    let s = el.tagName.toLowerCase();
    if (el.id) s += '#' + el.id;
    if (typeof el.className === 'string' && el.className.trim())
      s += '.' + el.className.trim().split(/\s+/).slice(0, 3).join('.');
    return '<' + s + '>';
  };
  const isVisible = el => {
    if (!el.checkVisibility({ visibilityProperty: true })) return false;
    const r = el.getBoundingClientRect();
    return r.width > 0 && r.height > 0;
  };
  const textOf = el => ['INPUT', 'TEXTAREA', 'SELECT'].includes(el.tagName)
    ? String(el.value) : el.innerText.trim();
  const shown = all.filter(isVisible);
  const out = { count: all.length, visible: shown.length };
  if (want === 'texts') { out.texts = shown.map(textOf); return out; }
  if (shown.length !== 1) return out;
  const el = shown[0];
  out.element = describe(el);
  out.enabled = !(el.disabled || el.closest('fieldset[disabled]')) &&
    el.getAttribute('aria-disabled') !== 'true';
  if (want === 'text') out.text = textOf(el);
  if (want === 'editable') {
    const kind = (el.getAttribute('type') || 'text').toLowerCase();
    out.editable = !el.readOnly && (el.isContentEditable || el.tagName === 'TEXTAREA' ||
      (el.tagName === 'INPUT' && !['checkbox', 'radio', 'button', 'submit', 'reset', 'file',
        'image', 'range', 'color', 'hidden'].includes(kind)));
  }
  if (want === 'point' || want === 'editable') {
    el.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
    const r = el.getBoundingClientRect();
    const x = r.left + r.width / 2, y = r.top + r.height / 2;
    const hit = document.elementFromPoint(x, y);
    if (hit && (hit === el || el.contains(hit))) { out.x = x; out.y = y; }
    else out.covered_by = hit ? describe(hit) : 'nothing (off screen)';
  }
  return out;
}
"#;

impl Page {
    fn call(&self, method: &str, params: Json) -> R<Json> {
        if self.0.closed.get() {
            return Err(fail("the page is closed"));
        }
        self.0
            .conn
            .call(Some(&self.0.session), method, params, CALL_TIMEOUT)
            .map_err(fail)
    }

    fn listen(&self) {
        let console = self.0.console.clone();
        let net = self.0.net.clone();
        let conn = self.0.conn.clone();
        let session = self.0.session.clone();
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
                // A dialog blocks the page until it's answered; accept it, as a user clicking OK.
                "Page.javascriptDialogOpening" => {
                    push(format!(
                        "{} accepted: {}",
                        params["type"].as_str().unwrap_or("dialog"),
                        params["message"].as_str().unwrap_or_default()
                    ));
                    conn.send(
                        Some(&session),
                        "Page.handleJavaScriptDialog",
                        json!({ "accept": true }),
                    );
                }
                _ => {}
            }),
        );
    }

    fn close(&self) {
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

    fn console(&self) -> Vec<String> {
        self.0.console.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// Evaluate JavaScript in the page and return its result as JSON (promises are awaited).
    fn eval(&self, js: &str) -> R<Json> {
        let r = self.call(
            "Runtime.evaluate",
            json!({
                "expression": js,
                "returnByValue": true,
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
        Ok(r["result"].get("value").cloned().unwrap_or(Json::Null))
    }

    fn probe(&self, selector: &str, want: &str) -> R<Probe> {
        let v = self.eval(&format!("({PROBE})({}, {})", json!(selector), json!(want)))?;
        if let Some(msg) = v.get("invalid").and_then(Json::as_str) {
            return Err(fail(format!(
                "`{selector}` isn't a valid CSS selector: {msg}"
            )));
        }
        let s = |k: &str| v.get(k).and_then(Json::as_str).map(str::to_string);
        Ok(Probe {
            count: v["count"].as_u64().unwrap_or(0),
            visible: v["visible"].as_u64().unwrap_or(0),
            element: s("element"),
            enabled: v["enabled"].as_bool().unwrap_or(false),
            editable: v["editable"].as_bool().unwrap_or(false),
            text: s("text"),
            texts: v["texts"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            point: v["x"].as_f64().zip(v["y"].as_f64()),
            covered_by: s("covered_by"),
        })
    }

    /// Retry `ready` until it gives a value, the page timeout passes, the test times out, or the
    /// run is interrupted. `ready` returns why it isn't ready yet, for the error.
    fn wait_for<T>(
        &self,
        shared: &Shared,
        action: &str,
        mut ready: impl FnMut() -> R<Result<T, String>>,
    ) -> R<T> {
        shared.page_reads.set(shared.page_reads.get() + 1);
        let started = Instant::now();
        let mut until = started + self.0.timeout.get();
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
            if let Err(stop) = super::request::wait(shared, POLL) {
                let stop = if stop.contains("timed out") {
                    "timed out"
                } else {
                    "interrupted"
                };
                return Err(fail(format!("{stop} while `{action}` waited: {why}")));
            }
        }
    }

    /// Wait for exactly one visible element matching `selector` that passes `check`.
    fn element(
        &self,
        shared: &Shared,
        action: &str,
        selector: &str,
        want: &str,
        check: impl Fn(&Probe) -> Option<String>,
    ) -> R<Probe> {
        self.wait_for(shared, &format!("{action}(\"{selector}\")"), || {
            let p = self.probe(selector, want)?;
            Ok(match (p.count, p.visible) {
                (0, _) => Err("nothing matches it".into()),
                (1, 0) => Err("it matches 1 element, but it isn't visible".into()),
                (n, 0) => Err(format!("it matches {n} elements, but none is visible")),
                (_, n) if n > 1 => Err(format!(
                    "it matches {n} visible elements; make it match one (e.g. with \
                     `:nth-child(…)`), or use `texts`"
                )),
                _ => match check(&p) {
                    Some(why) => Err(why),
                    None => Ok(p),
                },
            })
        })
    }

    fn navigate(&self, shared: &Shared, url: &str) -> R<()> {
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
        // `load` doesn't wait for scripts loaded with `import()`, and frameworks such as
        // SvelteKit and Next.js attach their event handlers that way: a click right after `load`
        // can hit a button that does nothing yet. Wait for the network to settle too.
        let started = Instant::now();
        while started.elapsed() < SETTLE_MAX.min(self.0.timeout.get()) {
            if self.0.net.lock().map(|n| n.settled()).unwrap_or(true) {
                break;
            }
            if let Err(stop) = super::request::wait(shared, POLL) {
                let stop = if stop.contains("timed out") {
                    "timed out"
                } else {
                    "interrupted"
                };
                return Err(fail(format!(
                    "{stop} while `navigate(\"{url}\")` waited for the page to settle"
                )));
            }
        }
        Ok(())
    }

    fn click(&self, shared: &Shared, selector: &str) -> R<()> {
        let p = self.element(shared, "click", selector, "point", |p| {
            disabled(p).or_else(|| p.covered_by.as_ref().map(covered))
        })?;
        let (x, y) = p.point.unwrap_or_default();
        for (kind, count) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)] {
            self.call(
                "Input.dispatchMouseEvent",
                json!({ "type": kind, "x": x, "y": y, "button": "left", "clickCount": count }),
            )?;
        }
        Ok(())
    }

    fn fill(&self, shared: &Shared, selector: &str, text: &str) -> R<()> {
        self.element(shared, "fill", selector, "editable", |p| {
            disabled(p)
                .or_else(|| (!p.editable).then(|| format!("{} doesn't take text", el(p))))
                .or_else(|| p.covered_by.as_ref().map(covered))
        })?;
        // Select what's there and type over it: the input events a user would cause.
        self.eval(&format!(
            "(() => {{
               const el = [...document.querySelectorAll({})].find(e => e.checkVisibility());
               el.focus();
               if (el.select) el.select();
               else {{ const r = document.createRange(); r.selectNodeContents(el);
                       const s = getSelection(); s.removeAllRanges(); s.addRange(r); }}
             }})()",
            json!(selector)
        ))?;
        if text.is_empty() {
            self.press("Backspace")
        } else {
            self.call("Input.insertText", json!({ "text": text }))
                .map(|_| ())
        }
    }

    fn press(&self, combo: &str) -> R<()> {
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

    fn text(&self, shared: &Shared, selector: &str) -> R<String> {
        let p = self.element(shared, "text", selector, "text", |_| None)?;
        Ok(p.text.unwrap_or_default())
    }

    fn wait(&self, shared: &Shared, selector: &str) -> R<()> {
        self.wait_for(shared, &format!("wait(\"{selector}\")"), || {
            let p = self.probe(selector, "")?;
            Ok(match (p.count, p.visible) {
                (_, 1..) => Ok(()),
                (0, _) => Err("nothing matches it".into()),
                _ => Err("it matches only hidden elements".into()),
            })
        })
    }

    fn screenshot_to(&self, path: &Path) -> Result<PathBuf, String> {
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
    fn read(&self, shared: &Shared, js: &str) -> R<String> {
        shared.page_reads.set(shared.page_reads.get() + 1);
        Ok(self.eval(js)?.as_str().unwrap_or_default().to_string())
    }
}

fn disabled(p: &Probe) -> Option<String> {
    (!p.enabled).then(|| format!("{} is disabled", el(p)))
}

fn el(p: &Probe) -> String {
    p.element.clone().unwrap_or_else(|| "the element".into())
}

fn covered(by: &String) -> String {
    format!("another element would get the click: {by}")
}

fn fail(msg: impl Into<String>) -> Box<EvalAltResult> {
    Failure::new(msg).into_err()
}

fn slot(shared: &Shared) -> super::BrowserSlot {
    match shared.current_case() {
        Some(case) => case.browser.clone(),
        None => shared.browser.clone(),
    }
}

/// How long page expectations retry: the session's timeout, once `browser()` was called.
pub fn timeout(shared: &Shared) -> Duration {
    slot(shared)
        .borrow()
        .as_ref()
        .map_or(shared.config.browser_timeout, |s| s.timeout.get())
}

/// The current case's session, or the run's own when no case is running.
fn session(shared: &Shared) -> R<Rc<Session>> {
    let slot = slot(shared);
    if let Some(s) = slot.borrow().as_ref() {
        return Ok(s.clone());
    }
    let s = Rc::new(Session::new(shared).map_err(Failure::into_err)?);
    *slot.borrow_mut() = Some(s.clone());
    Ok(s)
}

pub fn register(engine: &mut Engine, shared: &Rc<Shared>) {
    engine.register_type_with_name::<BrowserHandle>("browser");
    engine.register_type_with_name::<Page>("page");
    let s = shared.clone();
    engine.register_fn("browser", move || -> R<BrowserHandle> {
        Ok(BrowserHandle(session(&s)?))
    });
    let s = shared.clone();
    engine.register_fn("browser__kw", move |opts: rhai::Map| -> R<BrowserHandle> {
        let session = session(&s)?;
        for (k, v) in opts {
            match k.as_str() {
                "timeout" => session.timeout.set(crate::testing::parse_duration(&v)?),
                other => return Err(fail(format!("`browser` takes `timeout`, not `{other}`"))),
            }
        }
        Ok(BrowserHandle(session))
    });
    let s = shared.clone();
    engine.register_fn("open", move |b: &mut BrowserHandle, url: &str| {
        b.0.open(&s, url)
    });
    engine.register_get("pages", |b: &mut BrowserHandle| -> Array {
        b.0.pages
            .borrow()
            .iter()
            .cloned()
            .map(Dynamic::from)
            .collect()
    });

    // Actions wait until their element can be acted on.
    let s = shared.clone();
    engine.register_fn("navigate", move |p: &mut Page, url: &str| {
        p.navigate(&s, url)
    });
    let s = shared.clone();
    engine.register_fn("click", move |p: &mut Page, sel: &str| p.click(&s, sel));
    let s = shared.clone();
    engine.register_fn("fill", move |p: &mut Page, sel: &str, text: Dynamic| {
        p.fill(&s, sel, &value::to_text(&text))
    });
    engine.register_fn("press", |p: &mut Page, key: &str| p.press(key));
    let s = shared.clone();
    engine.register_fn("wait", move |p: &mut Page, sel: &str| p.wait(&s, sel));
    let s = shared.clone();
    engine.register_fn("text", move |p: &mut Page, sel: &str| p.text(&s, sel));

    // Reads don't wait; inside `expect`, the whole expectation is retried instead.
    let s = shared.clone();
    engine.register_fn("texts", move |p: &mut Page, sel: &str| -> R<Array> {
        s.page_reads.set(s.page_reads.get() + 1);
        Ok(p.probe(sel, "texts")?
            .texts
            .into_iter()
            .map(Dynamic::from)
            .collect())
    });
    let s = shared.clone();
    engine.register_fn("count", move |p: &mut Page, sel: &str| -> R<i64> {
        s.page_reads.set(s.page_reads.get() + 1);
        Ok(p.probe(sel, "")?.visible as i64)
    });
    let s = shared.clone();
    engine.register_fn("exists", move |p: &mut Page, sel: &str| -> R<bool> {
        s.page_reads.set(s.page_reads.get() + 1);
        Ok(p.probe(sel, "")?.visible > 0)
    });
    let s = shared.clone();
    engine.register_get("url", move |p: &mut Page| p.read(&s, "location.href"));
    let s = shared.clone();
    engine.register_get("title", move |p: &mut Page| p.read(&s, "document.title"));
    let s = shared.clone();
    engine.register_fn("html", move |p: &mut Page| {
        p.read(&s, "document.documentElement.outerHTML")
    });
    let s = shared.clone();
    engine.register_fn("evaluate", move |p: &mut Page, js: &str| -> R<Dynamic> {
        s.page_reads.set(s.page_reads.get() + 1);
        Ok(value::from_json(&p.eval(js)?))
    });
    engine.register_fn("console", |p: &mut Page| -> Array {
        p.console().into_iter().map(Dynamic::from).collect()
    });
    let s = shared.clone();
    engine.register_fn("screenshot", move |p: &mut Page, path: &str| -> R<String> {
        p.screenshot_to(&s.resolve_path(path))
            .map(|p| p.display().to_string())
            .map_err(|e| fail(format!("couldn't save a screenshot to {path}: {e}")))
    });
    engine.register_fn("close", |p: &mut Page| p.close());
}

/// Values the `where:` block of a failed expectation leaves out: the page section of the
/// report says more about them.
pub fn is_handle(v: &Dynamic) -> bool {
    v.is::<Page>() || v.is::<BrowserHandle>()
}
