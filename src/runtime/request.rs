//! Request blocks at runtime: binding arguments, rendering, sending, expectations, captures.

use super::builtins::{FileRef, Multipart, parse_sse};
use super::http::{self, Prepared, Received};
use super::undo::UndoAction;
use super::value::{self, RawText, map_of, to_json, to_text};
use super::{Exchange, Failure, ScriptFn, Shared};
use crate::source::Span;
use crate::style;
use crate::syntax::InterpMode;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use rhai::{Blob, Dynamic, Engine, EvalAltResult, Map, Scope};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Characters left alone in URL components: RFC 3986 "unreserved".
const COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

pub struct ParamRt {
    pub name: String,
    pub default: Option<usize>,
}

pub enum TPart {
    Lit(String),
    Expr {
        unit: usize,
        mode: InterpMode,
        span: Span,
    },
}

pub struct TemplateRt {
    pub parts: Vec<TPart>,
    pub span: Span,
}

pub enum SendRt {
    Json(TemplateRt),
    Raw(TemplateRt),
    Expr(usize),
}

#[derive(Clone)]
pub enum UndoRt {
    Request(Rc<BodyRt>),
    Call(usize),
    /// `undo(|| ...)` in a script, with the unit that defined the closure (none for the library).
    Script(rhai::FnPtr, Option<usize>),
}

pub struct BodyRt {
    pub method: String,
    pub url: TemplateRt,
    pub headers: Vec<(String, TemplateRt)>,
    pub send: Option<SendRt>,
    /// `expect` units (each compiled as an `expect <expr>` statement).
    pub expect: Vec<usize>,
    pub captures: Vec<(String, usize)>,
    pub undo: Option<(UndoRt, Span)>,
    pub span: Span,
    /// `timeout(...)` modifier: a unit evaluating to a duration.
    pub timeout: Option<usize>,
    /// `poll(...)` modifier: repeat the request until a condition holds.
    pub poll: Option<PollRt>,
}

pub struct PollRt {
    /// An expression evaluated with the response in scope, or a closure called with the response.
    pub until: usize,
    pub every: Option<usize>,
    pub within: Option<usize>,
    pub span: Span,
}

pub struct RequestRt {
    pub name: String,
    pub span: Span,
    pub params: Vec<ParamRt>,
    pub body: Rc<BodyRt>,
    /// Source text of the protocol line, for listings.
    pub summary: String,
}

impl RequestRt {
    pub fn required(&self) -> usize {
        self.params
            .iter()
            .take_while(|p| p.default.is_none())
            .count()
    }
}

pub enum Args {
    Positional(Vec<Dynamic>),
    Named(Map),
}

type R<T> = Result<T, Box<EvalAltResult>>;

/// Call a request: the native function behind `@name`.
pub fn call(shared: &Rc<Shared>, engine: &Engine, req: &RequestRt, args: Args) -> R<Dynamic> {
    let mut scope = bind(shared, engine, req, args)?;
    let (mut result, exchange) = match &req.body.poll {
        Some(poll) => poll_until(shared, engine, req, poll, &mut scope)?,
        None => {
            reset_http_flags(shared);
            match perform(shared, engine, &req.body, &mut scope) {
                Ok(ok) => ok,
                Err(e) if shared.state.borrow().last_http_timed_out => {
                    return Err(timed_out(shared, req, &scope, e));
                }
                Err(e) => return Err(e),
            }
        }
    };
    check(shared, engine, &req.body.expect, &mut scope, &exchange)?;
    for (name, unit) in &req.body.captures {
        let v = shared
            .eval(engine, *unit, &mut scope)
            .map_err(|e| with_exchange(shared, e, &exchange))?;
        scope.push_dynamic(name.clone(), v.clone());
        result.insert(name.as_str().into(), v);
    }
    if let Some((undo, span)) = &req.body.undo {
        register_undo(shared, req, undo, *span, &scope, &exchange, &result);
    }
    Ok(result.into())
}

fn reset_http_flags(shared: &Shared) {
    let mut state = shared.state.borrow_mut();
    state.last_http_timed_out = false;
    state.last_http_failed = false;
}

/// A request timed out: the server may still complete it, so register its undo anyway. Undo
/// actions treat "already gone" as success, and one that needs captures from the response
/// will fail and show up in the dirty report.
fn timed_out(
    shared: &Rc<Shared>,
    req: &RequestRt,
    scope: &Scope,
    e: Box<EvalAltResult>,
) -> Box<EvalAltResult> {
    let Some((undo, span)) = &req.body.undo else {
        return e;
    };
    let mut f = shared.to_failure(&e, None);
    let exchange = f.exchange.as_deref().cloned().unwrap_or_default();
    if register_undo(shared, req, undo, *span, scope, &exchange, &Map::new()) {
        f.details.push((
            "note".into(),
            "the request timed out, so its outcome is unknown; its undo was registered anyway"
                .into(),
        ));
    }
    f.into_err()
}

const POLL_EVERY: Duration = Duration::from_secs(1);
const POLL_WITHIN: Duration = Duration::from_secs(60);

/// `poll(...)`: send the request until `until` holds. Failed connections and timeouts count
/// as "not yet". Returns the response that satisfied the condition, with its scope.
fn poll_until(
    shared: &Rc<Shared>,
    engine: &Engine,
    req: &RequestRt,
    poll: &PollRt,
    scope: &mut Scope<'static>,
) -> R<(Map, Exchange)> {
    let base = scope.clone();
    let mut duration = |unit: Option<usize>, default: Duration| -> R<Duration> {
        let Some(unit) = unit else { return Ok(default) };
        crate::testing::parse_duration(&shared.eval(engine, unit, scope)?)
            .map_err(|e| shared.to_failure(&e, Some(unit)).into_err())
    };
    let every = duration(poll.every, POLL_EVERY)?;
    let within = duration(poll.within, POLL_WITHIN)?;
    let started = Instant::now();
    let mut attempts = 0;
    let mut any_timed_out = false;
    loop {
        attempts += 1;
        let mut s = base.clone();
        reset_http_flags(shared);
        let last = match perform(shared, engine, &req.body, &mut s) {
            Ok((result, exchange)) => {
                let held = until_holds(shared, engine, poll, &mut s, &result)
                    .map_err(|e| with_exchange(shared, e, &exchange))?;
                if held {
                    *scope = s;
                    return Ok((result, exchange));
                }
                Err(exchange)
            }
            Err(e) if shared.state.borrow().last_http_failed => {
                any_timed_out |= shared.state.borrow().last_http_timed_out;
                Ok(e)
            }
            Err(e) => return Err(e),
        };
        let elapsed = started.elapsed();
        let gave_up = if elapsed + every > within {
            Some(format!(
                "`{}` polled {attempts} {} over {}, but `until` never held",
                req.name,
                if attempts == 1 { "time" } else { "times" },
                crate::testing::humantime(elapsed)
            ))
        } else {
            wait(shared, every).err().map(|why| {
                format!(
                    "`{}` was still polling after {attempts} attempts when {why}",
                    req.name
                )
            })
        };
        let Some(message) = gave_up else { continue };
        let mut f = match last {
            Err(exchange) => {
                let mut f = Failure::new(message);
                f.exchange = Some(Box::new(exchange));
                f
            }
            Ok(e) => {
                let mut f = shared.to_failure(&e, None);
                f.details.push((
                    "last attempt".into(),
                    std::mem::replace(&mut f.message, message),
                ));
                f
            }
        };
        f.span = Some(poll.span);
        let e = f.into_err();
        return Err(if any_timed_out {
            timed_out(shared, req, &base, e)
        } else {
            e
        });
    }
}

/// Evaluate `until` against a response: a boolean expression, or a closure taking the response.
fn until_holds(
    shared: &Rc<Shared>,
    engine: &Engine,
    poll: &PollRt,
    scope: &mut Scope,
    response: &Map,
) -> R<bool> {
    let unit = poll.until;
    let mut v = shared.eval(engine, unit, scope)?;
    if let Some(f) = v.clone().try_cast::<rhai::FnPtr>() {
        let u = shared.unit(unit);
        v = f
            .call::<Dynamic>(engine, &u.ast, (Dynamic::from(response.clone()),))
            .map_err(|e| shared.to_failure(&e, Some(unit)).into_err())?;
    }
    v.as_bool().map_err(|_| {
        let mut f = Failure::new(format!(
            "`until` must be true or false, got {}",
            value::display(&v)
        ));
        f.span = Some(poll.span);
        f.into_err()
    })
}

/// Sleep between attempts, waking up for interrupts and the test's timeout.
pub fn wait(shared: &Shared, d: Duration) -> Result<(), &'static str> {
    let until = Instant::now() + d;
    loop {
        if shared.flags.interrupted() {
            return Err("it was interrupted");
        }
        let now = Instant::now();
        if let Some(deadline) = shared.flags.deadline.get()
            && shared.flags.unwinding.get() == 0
            && now >= deadline
        {
            return Err("the test timed out");
        }
        if now >= until {
            return Ok(());
        }
        std::thread::sleep((until - now).min(Duration::from_millis(50)));
    }
}

fn bind(shared: &Rc<Shared>, engine: &Engine, req: &RequestRt, args: Args) -> R<Scope<'static>> {
    let mut scope = Scope::new();
    let (mut positional, mut named) = match args {
        Args::Positional(v) => (v.into_iter().map(Some).collect::<Vec<_>>(), Map::new()),
        Args::Named(m) => (Vec::new(), m),
    };
    if let Some(extra) = named.keys().find(|k| {
        !req.params
            .iter()
            .enumerate()
            .any(|(i, p)| p.name == k.as_str() || format!("_{i}") == k.as_str())
    }) {
        let names: Vec<&str> = req.params.iter().map(|p| p.name.as_str()).collect();
        let mut msg = format!("`{}` has no parameter `{extra}`", req.name);
        if let Some(s) = crate::diag::suggest(extra, names.iter().copied()) {
            msg.push_str(&format!(" (did you mean `{s}`?)"));
        }
        return Err(Failure::new(msg).into_err());
    }
    for (i, p) in req.params.iter().enumerate() {
        let from_named = named.remove(p.name.as_str());
        let from_index = named.remove(format!("_{i}").as_str());
        if from_named.is_some() && from_index.is_some() {
            return Err(Failure::new(format!(
                "argument `{}` of `{}` given twice",
                p.name, req.name
            ))
            .into_err());
        }
        let value = match positional
            .get_mut(i)
            .and_then(Option::take)
            .or(from_named)
            .or(from_index)
        {
            Some(v) => v,
            None => match p.default {
                Some(unit) => shared.eval(engine, unit, &mut scope)?,
                None => {
                    return Err(Failure::new(format!(
                        "missing argument `{}` for `{}`",
                        p.name, req.name
                    ))
                    .into_err());
                }
            },
        };
        scope.push_dynamic(p.name.clone(), value);
    }
    Ok(scope)
}

/// Render and send a request, then put the response into the scope.
pub fn perform(
    shared: &Rc<Shared>,
    engine: &Engine,
    body: &BodyRt,
    scope: &mut Scope,
) -> R<(Map, Exchange)> {
    let mut prepared = build(shared, engine, body, scope)?;
    apply_middleware(shared, engine, &mut prepared)?;
    let mut exchange = Exchange {
        method: prepared.method.clone(),
        url: prepared.url.clone(),
        request_headers: prepared.headers.clone(),
        request_body: prepared
            .body
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned()),
        ..Default::default()
    };
    let own_timeout = match body.timeout {
        Some(unit) => Some(
            crate::testing::parse_duration(&shared.eval(engine, unit, scope)?)
                .map_err(|e| shared.to_failure(&e, Some(unit)).into_err())?,
        ),
        None => None,
    };
    let timeout =
        check_limits(shared, own_timeout).map_err(|f| fail_with(f, &exchange, body.span))?;
    if shared.config.trace {
        eprintln!(
            "  {} {} {}",
            style::dim("→"),
            style::bold(&prepared.method),
            prepared.url
        );
    }
    let started = Instant::now();
    let received = match http::send(&shared.agent, &prepared, timeout) {
        Ok(r) => r,
        Err(e) => {
            {
                let mut state = shared.state.borrow_mut();
                state.last_http_transient = e.transient;
                state.last_http_timed_out = e.timed_out;
                state.last_http_failed = true;
            }
            exchange.error = Some(e.message.clone());
            exchange.duration = Some(started.elapsed());
            if shared.config.trace {
                eprintln!("  {} {}", style::red("✗"), style::red(&e.message));
            }
            let message = if e.timed_out {
                format!("timed out after {}", crate::testing::humantime(timeout))
            } else {
                e.message.clone()
            };
            let f = Failure::new(format!(
                "{} {} failed: {message}",
                prepared.method, prepared.url
            ));
            return Err(fail_with(f, &exchange, body.span));
        }
    };
    shared.state.borrow_mut().last_http_transient = http::is_transient_status(received.status);
    exchange.status = Some(received.status);
    exchange.response_headers = received.headers.clone();
    exchange.response_body = Some(String::from_utf8_lossy(&received.body).into_owned());
    exchange.duration = Some(received.duration);
    if shared.config.trace {
        let status = format!("{} {}", received.status, reason(received.status));
        let status = if received.status < 400 {
            style::green(status)
        } else {
            style::red(status)
        };
        eprintln!(
            "  {} {status} {}",
            style::dim("←"),
            style::dim(format!("({} ms)", received.duration.as_millis()))
        );
    }
    let result = response_map(&prepared, &received);
    for (k, v) in &result {
        if k != "request" {
            scope.push_dynamic(k.to_string(), v.clone());
        }
    }
    Ok((result, exchange))
}

pub fn reason(status: u16) -> &'static str {
    ureq::http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("")
}

/// Before sending: honour interrupts and test timeouts. Returns the request timeout to use.
fn check_limits(
    shared: &Shared,
    own: Option<std::time::Duration>,
) -> Result<std::time::Duration, Failure> {
    let mut timeout = own.unwrap_or(shared.config.request_timeout);
    if shared.flags.unwinding.get() > 0 {
        return Ok(timeout);
    }
    if shared.flags.interrupted() {
        return Err(Failure::new("interrupted"));
    }
    if let Some(deadline) = shared.flags.deadline.get() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Failure::new("timed out"));
        }
        timeout = timeout.min(left);
    }
    Ok(timeout)
}

fn fail_with(mut f: Failure, exchange: &Exchange, span: Span) -> Box<EvalAltResult> {
    f.exchange = Some(Box::new(exchange.clone()));
    if f.span.is_none() {
        f.span = Some(span);
    }
    f.into_err()
}

/// Attach the exchange to a failure raised while checking a response.
pub fn with_exchange(
    shared: &Shared,
    err: Box<EvalAltResult>,
    exchange: &Exchange,
) -> Box<EvalAltResult> {
    let mut f = shared.to_failure(&err, None);
    if f.exchange.is_none() {
        f.exchange = Some(Box::new(exchange.clone()));
    }
    f.into_err()
}

/// Run `expect` units against the response in `scope`.
pub fn check(
    shared: &Rc<Shared>,
    engine: &Engine,
    expects: &[usize],
    scope: &mut Scope,
    exchange: &Exchange,
) -> R<()> {
    for unit in expects {
        let _ = shared
            .eval(engine, *unit, scope)
            .map_err(|e| with_exchange(shared, e, exchange))?;
    }
    Ok(())
}

fn response_map(prepared: &Prepared, received: &Received) -> Map {
    let mut headers = Map::new();
    for (k, v) in &received.headers {
        let k = k.to_ascii_lowercase();
        match headers.get_mut(k.as_str()) {
            Some(existing) => *existing = format!("{existing}, {v}").into(),
            None => {
                headers.insert(k.into(), v.clone().into());
            }
        }
    }
    let text = std::str::from_utf8(&received.body).ok();
    let is_sse = headers
        .get("content-type")
        .is_some_and(|ct| ct.to_string().starts_with("text/event-stream"));
    let events = match text {
        Some(t) if is_sse => Dynamic::from_array(parse_sse(t)),
        _ => Dynamic::UNIT,
    };
    let body = match text {
        _ if received.body.is_empty() => Dynamic::UNIT,
        Some(t) => match serde_json::from_str::<serde_json::Value>(t) {
            Ok(json) => value::from_json(&json),
            Err(_) => t.into(),
        },
        None => Dynamic::from_blob(received.body.clone()),
    };
    let request = map_of([
        ("method", prepared.method.clone().into()),
        ("url", prepared.url.clone().into()),
        ("headers", header_map(&prepared.headers).into()),
        (
            "body",
            prepared.body.as_ref().map_or(Dynamic::UNIT, |b| {
                String::from_utf8_lossy(b).into_owned().into()
            }),
        ),
    ]);
    let mut result = map_of([
        ("status", (received.status as i64).into()),
        ("headers", headers.into()),
        ("body", body),
        ("raw", Dynamic::from_blob(received.body.clone() as Blob)),
        ("duration", (received.duration.as_millis() as i64).into()),
        ("request", request.into()),
    ]);
    if !events.is_unit() {
        result.insert("events".into(), events);
    }
    result
}

fn header_map(headers: &[(String, String)]) -> Map {
    headers
        .iter()
        .map(|(k, v)| (k.as_str().into(), v.clone().into()))
        .collect()
}

fn render(shared: &Rc<Shared>, engine: &Engine, t: &TemplateRt, scope: &mut Scope) -> R<String> {
    render_with(shared, engine, t, scope, &mut false)
}

/// Render a template; `saw_null` is set if any interpolation evaluated to `()`.
fn render_with(
    shared: &Rc<Shared>,
    engine: &Engine,
    t: &TemplateRt,
    scope: &mut Scope,
    saw_null: &mut bool,
) -> R<String> {
    let mut out = String::new();
    for part in &t.parts {
        match part {
            TPart::Lit(s) => out.push_str(s),
            TPart::Expr { unit, mode, span } => {
                let v = shared.eval(engine, *unit, scope)?;
                *saw_null |= v.is_unit();
                let is_raw = v.is::<RawText>();
                match mode {
                    InterpMode::Text | InterpMode::UrlRaw => out.push_str(&to_text(&v)),
                    InterpMode::UrlEncoded if is_raw => out.push_str(&to_text(&v)),
                    InterpMode::UrlEncoded => {
                        out.extend(utf8_percent_encode(&to_text(&v), COMPONENT))
                    }
                    InterpMode::JsonValue if is_raw => out.push_str(&to_text(&v)),
                    InterpMode::JsonValue => {
                        let json = to_json(&v).map_err(|m| Failure::new(m).at(*span).into_err())?;
                        out.push_str(&json.to_string());
                    }
                    InterpMode::JsonString => {
                        let s = serde_json::Value::String(to_text(&v)).to_string();
                        out.push_str(&s[1..s.len() - 1]);
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Render a header value; `None` if any interpolation is null.
fn render_header(
    shared: &Rc<Shared>,
    engine: &Engine,
    t: &TemplateRt,
    scope: &mut Scope,
) -> R<Option<String>> {
    let mut saw_null = false;
    let value = render_with(shared, engine, t, scope, &mut saw_null)?;
    Ok((!saw_null).then_some(value))
}

fn build(shared: &Rc<Shared>, engine: &Engine, body: &BodyRt, scope: &mut Scope) -> R<Prepared> {
    let url = render(shared, engine, &body.url, scope)?;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        let f = Failure::new(format!(
            "URL `{url}` doesn't start with http:// or https://"
        ))
        .at(body.url.span);
        return Err(f.into_err());
    }
    let mut headers = Vec::new();
    for (name, t) in &body.headers {
        // A header with a null interpolation is left out: `Authorization: Bearer {{jwt}}`
        // with `jwt = ()` sends no Authorization at all.
        if let Some(value) = render_header(shared, engine, t, scope)? {
            headers.push((name.clone(), value));
        }
    }
    let has = |headers: &[(String, String)], name: &str| {
        headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name))
    };
    if let Some(case) = shared.current_case() {
        for (k, v) in case.ctx.0.borrow().headers.iter() {
            if !has(&headers, k) {
                headers.push((k.to_string(), to_text(v)));
            }
        }
    }
    let mut content_type: Option<String> = None;
    let body_bytes = match &body.send {
        None => None,
        Some(SendRt::Json(t)) => {
            let text = render(shared, engine, t, scope)?;
            if let Err(e) = serde_json::from_str::<serde_json::Value>(&text) {
                let f = Failure::new(format!(
                    "the body isn't valid JSON after interpolation: {e}"
                ))
                .at(t.span);
                let mut f = f;
                f.details.push(("body".into(), value::truncate(&text, 500)));
                return Err(f.into_err());
            }
            content_type = Some("application/json".into());
            Some(text.into_bytes())
        }
        Some(SendRt::Raw(t)) => Some(render(shared, engine, t, scope)?.into_bytes()),
        Some(SendRt::Expr(unit)) => {
            let v = shared.eval(engine, *unit, scope)?;
            if v.is_unit() {
                None
            } else if let Some(m) = v.read_lock::<Multipart>() {
                let boundary = format!(
                    "klausur-{}",
                    crate::testing::generators::uuid(&shared.rng())
                );
                let (bytes, ct) = m.encode(&boundary);
                content_type = Some(ct);
                Some(bytes)
            } else if let Some(f) = v.read_lock::<FileRef>() {
                content_type = Some(f.content_type.clone());
                Some(f.bytes.clone())
            } else if v.is_blob() {
                Some(v.cast::<Blob>())
            } else if v.is_map() || v.is_array() {
                content_type = Some("application/json".into());
                let json = to_json(&v).map_err(|m| Failure::new(m).into_err())?;
                Some(json.to_string().into_bytes())
            } else {
                Some(to_text(&v).into_bytes())
            }
        }
    };
    if let Some(ct) = content_type
        && !has(&headers, "content-type")
    {
        headers.push(("Content-Type".into(), ct));
    }
    Ok(Prepared {
        method: body.method.clone(),
        url,
        headers,
        body: body_bytes,
    })
}

/// Run the case's `on_request` hooks, which may return a modified request map.
fn apply_middleware(shared: &Rc<Shared>, engine: &Engine, req: &mut Prepared) -> R<()> {
    let Some(case) = shared.current_case() else {
        return Ok(());
    };
    for hook in &case.on_request {
        let map = map_of([
            ("method", req.method.clone().into()),
            ("url", req.url.clone().into()),
            ("headers", header_map(&req.headers).into()),
            (
                "body",
                req.body.as_ref().map_or(Dynamic::UNIT, |b| {
                    String::from_utf8_lossy(b).into_owned().into()
                }),
            ),
        ]);
        let out = call_script_fn(shared, engine, hook, (map,))?;
        let Some(out) = out.try_cast::<Map>() else {
            continue;
        };
        if let Some(m) = out.get("method") {
            req.method = m.to_string();
        }
        if let Some(u) = out.get("url") {
            req.url = u.to_string();
        }
        if let Some(h) = out.get("headers").and_then(|h| h.clone().try_cast::<Map>()) {
            req.headers = h.iter().map(|(k, v)| (k.to_string(), to_text(v))).collect();
        }
        if let Some(b) = out.get("body") {
            req.body = if b.is_unit() {
                None
            } else {
                Some(to_text(b).into_bytes())
            };
        }
    }
    Ok(())
}

pub fn call_script_fn(
    shared: &Rc<Shared>,
    engine: &Engine,
    f: &ScriptFn,
    args: impl rhai::FuncArgs,
) -> R<Dynamic> {
    let unit = shared.unit(f.unit);
    f.fn_ptr
        .call::<Dynamic>(engine, &unit.ast, args)
        .map_err(|e| shared.to_failure(&e, Some(f.unit)).into_err())
}

/// Returns whether the undo was registered (a scope was open).
fn register_undo(
    shared: &Rc<Shared>,
    req: &RequestRt,
    undo: &UndoRt,
    span: Span,
    scope: &Scope,
    exchange: &Exchange,
    result: &Map,
) -> bool {
    if shared.flags.unwinding.get() > 0 {
        return false;
    }
    let mut state = shared.state.borrow_mut();
    let Some(top) = state.scopes.last_mut() else {
        state.skipped_undos.push(req.name.clone());
        return false;
    };
    let env = scope
        .iter()
        .map(|(name, _, v)| (name.to_string(), v))
        .collect();
    let captures = req
        .body
        .captures
        .iter()
        .filter_map(|(name, _)| {
            result
                .get(name.as_str())
                .map(|v| (name.clone(), value::display(v)))
        })
        .collect();
    top.actions.push(UndoAction {
        label: req.name.clone(),
        origin: match exchange.status {
            Some(status) => format!("{} {} → {status}", exchange.method, exchange.url),
            None => format!(
                "{} {} → timed out (outcome unknown)",
                exchange.method, exchange.url
            ),
        },
        captures,
        kind: undo.clone(),
        env,
        span,
    });
    true
}
