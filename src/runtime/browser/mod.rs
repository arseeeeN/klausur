//! Browser pages for end-to-end tests, driven over the Chrome DevTools Protocol.
//!
//! Tests only see `browser()`, `page` and `locator` values; nothing here leaks protocol details,
//! so the backend can change later (e.g. to WebDriver BiDi). One Chrome process serves the whole
//! run, and every test case gets its own browser context (like an incognito window) that is
//! closed when the case ends. The API follows Playwright's unless there's a reason not to.

mod cdp;
mod chrome;
mod keys;
mod locator;
mod page;
mod route;

use super::value;
use super::{Failure, PageReport, Shared};
use cdp::Connection;
use locator::Locator;
use page::{Page, PageInner, Routes, Timeouts};
use rhai::{Array, Dynamic, Engine, EvalAltResult, Map};
use route::{Action, Route};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

type R<T> = Result<T, Box<EvalAltResult>>;

/// The Chrome process shared by all worker threads, started on first use.
static CHROME: Mutex<Option<Arc<chrome::Chrome>>> = Mutex::new(None);

/// Where failure screenshots go, relative to the working directory. Like Playwright's
/// `test-results/`, it's emptied when `klausur test` starts.
pub const RESULTS_DIR: &str = "klausur-results";

/// How long one protocol call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How often waits check the page again.
const POLL: Duration = Duration::from_millis(25);

/// Stop Chrome. Called when klausur exits; contexts and pages die with it.
pub fn shutdown() {
    // `try_lock`: the Ctrl-C handler calls this too, and must not wait for a launch in progress.
    let chrome = CHROME.try_lock().map(|mut c| c.take()).ok().flatten();
    drop(chrome);
}

/// Empty `klausur-results/` before a test run, so it only holds this run's screenshots.
pub fn clear_results() {
    let _ = std::fs::remove_dir_all(RESULTS_DIR);
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
    timeouts: Rc<Timeouts>,
    /// `browser().route(…)`: routes for every page of the context.
    routes: Routes,
    /// `browser(ignore_requests: […])`: failures that are expected (a login page answered
    /// with 401, say), kept out of failure reports.
    ignored: RefCell<Vec<regex::Regex>>,
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
            timeouts: Rc::default(),
            routes: Routes::default(),
            ignored: RefCell::default(),
        })
    }

    fn open(&self, shared: &Shared, url: &str) -> R<Page> {
        let call = |method: &str, params: serde_json::Value| {
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
            timeouts: self.timeouts.clone(),
            accept_dialogs: Arc::default(),
            routes: Routes::default(),
            context_routes: self.routes.clone(),
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
        // The context's routes apply from the first request on.
        if !self.routes.lock().map(|r| r.is_empty()).unwrap_or(true) {
            page.sync_routes()?;
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

    /// Attach the state of the last open page to a failure: URL, title, console errors, failed
    /// requests, and a screenshot saved under `klausur-results/`.
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
            console: page
                .console()
                .into_iter()
                .filter(|line| !self.is_ignored(console_url(line)))
                .collect(),
            failed_requests: page
                .failed_requests()
                .into_iter()
                .filter(|line| !self.is_ignored(line.rsplit(' ').next()))
                .collect(),
        }));
    }

    fn is_ignored(&self, url: Option<&str>) -> bool {
        url.is_some_and(|u| self.ignored.borrow().iter().any(|re| re.is_match(u)))
    }
}

/// The URL Chrome names at the end of a "Failed to load resource: … (url)" console line.
fn console_url(line: &str) -> Option<&str> {
    line.strip_suffix(')')
        .and_then(|l| l.rsplit_once(" ("))
        .map(|(_, url)| url)
}

/// A file name for a case's screenshot: the test file and name, plus a short hash of the case.
pub fn file_name(file: &str, test: &str, case: &str) -> String {
    let stem = Path::new(file)
        .file_name()
        .and_then(|f| f.to_str())
        .map(|f| f.trim_end_matches(".kl").trim_end_matches(".test"))
        .unwrap_or_default();
    let mut name = format!("{stem} {test}")
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

fn fail(msg: impl Into<String>) -> Box<EvalAltResult> {
    Failure::new(msg).into_err()
}

fn slot(shared: &Shared) -> super::BrowserSlot {
    match shared.current_case() {
        Some(case) => case.browser.clone(),
        None => shared.browser.clone(),
    }
}

/// How long page expectations retry: the browser's `expect_timeout`, 5 s by default.
pub fn expect_timeout(shared: &Shared) -> Duration {
    slot(shared)
        .borrow()
        .as_ref()
        .map_or(Timeouts::default().expect.get(), |s| {
            s.timeouts.expect.get()
        })
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

/// What a route answers with: `"abort"`, or a response map like Playwright's `route.fulfill`:
/// `#{ status, headers, content_type, json | body | path }`.
fn route_action(shared: &Shared, v: &Dynamic) -> R<Action> {
    if v.is_string() {
        return match v.to_string().as_str() {
            "abort" => Ok(Action::Abort),
            other => Err(fail(format!(
                "a route answers with a response map or \"abort\", not {}",
                json!(other)
            ))),
        };
    }
    let Some(map) = v.read_lock::<Map>().map(|m| m.clone()) else {
        return Err(fail(format!(
            "a route answers with a response map or \"abort\", not {}",
            value::type_name(v)
        )));
    };
    let (_, named) = options(
        map,
        &["status", "headers", "content_type", "json", "body", "path"],
        "route",
    )?;
    let mut status = 200;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut content_type = None;
    let mut body = Vec::new();
    for (k, v) in named {
        match k.as_str() {
            "status" => {
                status = v
                    .as_int()
                    .ok()
                    .and_then(|s| u16::try_from(s).ok())
                    .ok_or_else(|| fail("a route's `status` must be a number such as 404"))?
            }
            "headers" => {
                let h = v
                    .read_lock::<Map>()
                    .map(|m| m.clone())
                    .ok_or_else(|| fail("a route's `headers` must be a map"))?;
                headers.extend(
                    h.into_iter()
                        .map(|(k, v)| (k.to_string(), value::to_text(&v))),
                );
            }
            "content_type" => content_type = Some(v.to_string()),
            "json" => {
                let j = value::to_json(&v).map_err(fail)?;
                body = j.to_string().into_bytes();
                content_type.get_or_insert_with(|| "application/json".to_string());
            }
            "body" => body = super::builtins::bytes_of(&v),
            _ => {
                let path = v.to_string();
                let full = shared.resolve_path(&path);
                body = std::fs::read(&full)
                    .map_err(|e| fail(format!("a route can't read {path}: {e}")))?;
                content_type
                    .get_or_insert_with(|| super::builtins::guess_content_type(&path).to_string());
            }
        }
    }
    if let Some(ct) = content_type
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        headers.push(("Content-Type".into(), ct));
    }
    Ok(Action::Fulfil {
        status,
        headers,
        body,
    })
}

fn add_route(routes: &Routes, glob: &str, action: Action) {
    if let Ok(mut r) = routes.lock() {
        r.push(Route::new(glob, action));
    }
}

fn remove_routes(routes: &Routes, glob: &str) {
    if let Ok(mut r) = routes.lock() {
        r.retain(|r| r.glob != glob);
    }
}

fn request_map(r: &page::Request) -> Dynamic {
    let (status, error) = match &r.outcome {
        Some(Ok(s)) => (Dynamic::from(i64::from(*s)), Dynamic::UNIT),
        Some(Err(e)) => (Dynamic::UNIT, Dynamic::from(e.clone())),
        None => (Dynamic::UNIT, Dynamic::UNIT),
    };
    Dynamic::from(value::map_of([
        ("method", Dynamic::from(r.method.clone())),
        ("url", Dynamic::from(r.url.clone())),
        (
            "headers",
            Dynamic::from(value::map_of(
                r.headers
                    .iter()
                    .map(|(k, v)| (k.as_str(), Dynamic::from(v.clone()))),
            )),
        ),
        ("body", r.body.clone().map_or(Dynamic::UNIT, Dynamic::from)),
        ("status", status),
        ("error", error),
    ]))
}

/// A string, or an array of strings: `select("a")`, `select(["a", "b"])`.
fn strings(v: &Dynamic, what: &str) -> R<Vec<String>> {
    if v.is_array() {
        v.clone()
            .into_array()
            .unwrap_or_default()
            .iter()
            .map(|x| strings(x, what).map(|mut s| s.remove(0)))
            .collect()
    } else if v.is_string() {
        Ok(vec![v.to_string()])
    } else {
        Err(fail(format!(
            "{what} must be a string or an array of strings, not {}",
            value::type_name(v)
        )))
    }
}

/// Named options: checks the names, and returns the positional `_0`, `_1`, … in order.
fn options(map: Map, allowed: &[&str], call: &str) -> R<(Vec<Dynamic>, Map)> {
    let mut positional = Vec::new();
    let mut named = Map::new();
    for (k, v) in map {
        if let Some(i) = k.strip_prefix('_').and_then(|i| i.parse::<usize>().ok()) {
            positional.push((i, v));
        } else if allowed.contains(&k.as_str()) {
            named.insert(k, v);
        } else {
            return Err(fail(format!(
                "`{call}` takes {}, not `{k}`",
                allowed
                    .iter()
                    .map(|a| format!("`{a}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    positional.sort_by_key(|(i, _)| *i);
    Ok((positional.into_iter().map(|(_, v)| v).collect(), named))
}

fn filter(l: &Locator, opts: Map) -> R<Locator> {
    let (_, named) = options(opts, &["has_text", "has_not_text", "visible"], "filter")?;
    let mut out = l.clone();
    for (k, v) in named {
        out = match k.as_str() {
            "has_text" => out.has_text(&v.to_string(), false),
            "has_not_text" => out.has_text(&v.to_string(), true),
            _ => out.visible(
                v.as_bool()
                    .map_err(|_| fail("`visible` takes true or false"))?,
            ),
        };
    }
    Ok(out)
}

pub fn register(engine: &mut Engine, shared: &Rc<Shared>) {
    engine.register_type_with_name::<BrowserHandle>("browser");
    engine.register_type_with_name::<Page>("page");
    engine.register_type_with_name::<Locator>("locator");

    // --- browser() ------------------------------------------------------------------------
    let s = shared.clone();
    engine.register_fn("browser", move || -> R<BrowserHandle> {
        Ok(BrowserHandle(session(&s)?))
    });
    let s = shared.clone();
    engine.register_fn("browser__kw", move |opts: Map| -> R<BrowserHandle> {
        let session = session(&s)?;
        let (_, named) = options(
            opts,
            &["timeout", "expect_timeout", "ignore_requests"],
            "browser",
        )?;
        for (k, v) in named {
            match k.as_str() {
                "timeout" => session
                    .timeouts
                    .action
                    .set(crate::testing::parse_duration(&v)?),
                "expect_timeout" => session
                    .timeouts
                    .expect
                    .set(crate::testing::parse_duration(&v)?),
                _ => session.ignored.borrow_mut().extend(
                    strings(&v, "`ignore_requests`")?
                        .iter()
                        .map(|g| route::glob_regex(g)),
                ),
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

    // --- Locators -------------------------------------------------------------------------
    engine.register_fn("locator", |p: &mut Page, sel: &str| Locator::css(p, sel));
    engine.register_fn("locator", |l: &mut Locator, sel: &str| l.locator(sel));
    engine.register_fn("nth", |l: &mut Locator, i: i64| l.nth(i));
    engine.register_fn("first", |l: &mut Locator| l.nth(0));
    engine.register_fn("last", |l: &mut Locator| l.nth(-1));
    engine.register_fn("filter", |l: &mut Locator, opts: Map| filter(l, opts));
    engine.register_fn("filter__kw", |l: &mut Locator, opts: Map| filter(l, opts));
    engine.register_fn("to_string", |l: &mut Locator| {
        format!("locator({})", json!(l.describe()))
    });

    // Every element method exists on locators, and on pages with a selector first:
    // `page.click(".x")` is `page.locator(".x").click()`.
    macro_rules! element_fn {
        ($name:literal, |$l:ident, $s:ident $(, $arg:ident: $t:ty)*| $body:expr) => {{
            let sh = shared.clone();
            engine.register_fn($name, move |$l: &mut Locator $(, $arg: $t)*| {
                let $s = &*sh;
                $body
            });
            let sh = shared.clone();
            engine.register_fn($name, move |p: &mut Page, sel: &str $(, $arg: $t)*| {
                let $l = &mut Locator::css(p, sel);
                let $s = &*sh;
                $body
            });
            // Like Playwright, locators narrow down with `.locator(sel)` rather than taking a
            // selector in each method; say so instead of "isn't defined".
            engine.register_fn(
                $name,
                |l: &mut Locator, sel: &str $(, $arg: $t)*| -> R<()> {
                    $(let _ = $arg;)*
                    let _ = l;
                    Err(fail(format!(
                        "a locator's `{0}` takes no selector; narrow the locator down first: \
                         `.locator({1}).{0}(…)`",
                        $name,
                        json!(sel)
                    )))
                },
            );
        }};
    }
    element_fn!("click", |l, s| l.click(s));
    element_fn!("hover", |l, s| l.hover(s));
    element_fn!("fill", |l, s, text: Dynamic| l
        .fill(s, &value::to_text(&text)));
    element_fn!("check", |l, s| l.set_checked(s, true));
    element_fn!("uncheck", |l, s| l.set_checked(s, false));
    element_fn!("select", |l, s, values: Dynamic| l
        .select(s, strings(&values, "`select`'s options")?));
    element_fn!("upload", |l, s, files: Dynamic| l
        .upload(s, strings(&files, "`upload`'s files")?));
    element_fn!("wait", |l, s| l.wait(s, "visible"));
    element_fn!("text", |l, s| l.text(s));
    element_fn!("value", |l, s| l.value(s));
    element_fn!("checked", |l, s| l.checked(s));
    element_fn!("texts", |l, s| l.texts(s));
    element_fn!("count", |l, s| l.count(s));
    element_fn!("visible", |l, s| l.is_visible(s));
    // `press(key)` on a page presses it wherever the focus is; on an element it focuses first.
    element_fn!("press", |l, s, key: &str| l.press(s, key));
    engine.register_fn("press", |p: &mut Page, key: &str| p.press(key));
    // `wait(state: "hidden")`, and `page.wait(sel, state: "hidden")`.
    let s = shared.clone();
    engine.register_fn("wait__kw", move |l: &mut Locator, opts: Map| -> R<()> {
        let (_, named) = options(opts, &["state"], "wait")?;
        let state = named.get("state").map(|v| v.to_string());
        l.wait(&s, state.as_deref().unwrap_or("visible"))
    });
    let s = shared.clone();
    engine.register_fn("wait__kw", move |p: &mut Page, opts: Map| -> R<()> {
        let (positional, named) = options(opts, &["state"], "wait")?;
        let Some(sel) = positional.first() else {
            return Err(fail("`wait` needs a selector"));
        };
        let state = named.get("state").map(|v| v.to_string());
        Locator::css(p, &sel.to_string()).wait(&s, state.as_deref().unwrap_or("visible"))
    });

    // --- Pages ----------------------------------------------------------------------------
    let s = shared.clone();
    engine.register_fn("reload", move |p: &mut Page| p.reload(&s));
    let s = shared.clone();
    engine.register_fn("navigate", move |p: &mut Page, url: &str| {
        p.navigate(&s, url)
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
    engine.register_fn("accept_dialogs", |p: &mut Page| {
        p.0.accept_dialogs
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let s = shared.clone();
    engine.register_fn("screenshot", move |p: &mut Page, path: &str| -> R<String> {
        p.screenshot_to(&s.resolve_path(path))
            .map(|p| p.display().to_string())
            .map_err(|e| fail(format!("couldn't save a screenshot to {path}: {e}")))
    });
    engine.register_fn("close", |p: &mut Page| p.close());

    // --- Network --------------------------------------------------------------------------
    let s = shared.clone();
    engine.register_fn(
        "route",
        move |p: &mut Page, glob: &str, response: Dynamic| -> R<()> {
            add_route(&p.0.routes, glob, route_action(&s, &response)?);
            p.sync_routes()
        },
    );
    let s = shared.clone();
    engine.register_fn(
        "route",
        move |b: &mut BrowserHandle, glob: &str, response: Dynamic| -> R<()> {
            add_route(&b.0.routes, glob, route_action(&s, &response)?);
            for page in b.0.pages.borrow().iter().filter(|p| !p.0.closed.get()) {
                page.sync_routes()?;
            }
            Ok(())
        },
    );
    engine.register_fn("unroute", |p: &mut Page, glob: &str| -> R<()> {
        remove_routes(&p.0.routes, glob);
        p.sync_routes()
    });
    engine.register_fn("unroute", |b: &mut BrowserHandle, glob: &str| -> R<()> {
        remove_routes(&b.0.routes, glob);
        for page in b.0.pages.borrow().iter().filter(|p| !p.0.closed.get()) {
            page.sync_routes()?;
        }
        Ok(())
    });
    // `requests()` / `requests(glob)`: what the page requested so far, oldest first.
    let s = shared.clone();
    engine.register_fn("requests", move |p: &mut Page| -> Array {
        s.page_reads.set(s.page_reads.get() + 1);
        p.requests().iter().map(request_map).collect()
    });
    let s = shared.clone();
    engine.register_fn("requests", move |p: &mut Page, glob: &str| -> Array {
        s.page_reads.set(s.page_reads.get() + 1);
        let re = route::glob_regex(glob);
        p.requests()
            .iter()
            .filter(|r| re.is_match(&r.url))
            .map(request_map)
            .collect()
    });
}

/// Values the `where:` block of a failed expectation leaves out: the page section of the
/// report says more about them.
pub fn is_handle(v: &Dynamic) -> bool {
    v.is::<Page>() || v.is::<BrowserHandle>() || v.is::<Locator>()
}
