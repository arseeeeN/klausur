//! Runtime: shared state, compiled units, failures and error mapping.

pub mod builtins;
pub mod query;
pub mod engine;
pub mod http;
pub mod request;
pub mod undo;
pub mod value;

use crate::source::{CodeBuilder, FileId, Sources, Span};
use crate::testing::generators::Rng;
use request::RequestRt;
use rhai::{AST, Dynamic, Engine, EvalAltResult, Map, Position, Scope};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use undo::{DirtyEntry, UndoScope};

/// A compiled piece of Rhai code and the map back to its source.
pub struct Unit {
    pub ast: AST,
    pub code: CodeBuilder,
    pub file: FileId,
}

pub struct GlobalVar {
    pub unit: usize,
    pub span: Span,
    pub value: Option<Dynamic>,
    pub evaluating: bool,
}

pub struct Config {
    /// Print every request and response summary as it happens.
    pub trace: bool,
    pub request_timeout: Duration,
    pub undo_attempts: u32,
    pub undo_backoff: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            trace: false,
            request_timeout: Duration::from_secs(30),
            undo_attempts: 3,
            undo_backoff: Duration::from_millis(250),
        }
    }
}

/// Environment variables: the process environment wins, then `.env.<name>`, then `.env`.
#[derive(Default)]
pub struct Env {
    pub layers: Vec<HashMap<String, String>>,
}

impl Env {
    pub fn get(&self, name: &str) -> Option<String> {
        std::env::var(name)
            .ok()
            .or_else(|| self.layers.iter().find_map(|l| l.get(name).cloned()))
    }
}

pub struct Flags {
    pub interrupted: Arc<AtomicBool>,
    pub deadline: Cell<Option<Instant>>,
    /// Non-zero while undo actions run: interrupts and timeouts don't apply, and nothing new is registered.
    pub unwinding: Cell<u32>,
}

impl Flags {
    pub fn interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }
}

/// The context object of one running test case (`ctx` in hooks).
#[derive(Clone)]
pub struct Ctx(pub Rc<RefCell<CtxData>>);

#[derive(Default)]
pub struct CtxData {
    pub test: String,
    pub args: Map,
    /// Default headers added to every request in the case.
    pub headers: Map,
    /// Free-form values hooks can share with the test body (`ctx.user = …`).
    pub vars: Map,
    pub logs: Vec<String>,
}

/// A hook closure plus the unit it was defined in (needed to call it).
#[derive(Clone)]
pub struct ScriptFn {
    pub fn_ptr: rhai::FnPtr,
    pub unit: usize,
}

/// State of one running test case, visible to requests and native functions.
pub struct CaseFrame {
    pub ctx: Ctx,
    pub dirty: RefCell<Vec<DirtyEntry>>,
    pub on_request: Vec<ScriptFn>,
    pub on_dirty: Vec<ScriptFn>,
    pub rng: Rng,
    pub run: Option<Rc<crate::testing::CaseRun>>,
}

#[derive(Default)]
pub struct State {
    pub cases: Vec<Rc<CaseFrame>>,
    pub scopes: Vec<UndoScope>,
    /// Dirty entries from code that runs outside any test case (`klausur run`).
    pub orphan_dirty: Vec<DirtyEntry>,
    /// Requests whose `undo` wasn't registered because no scope was open.
    pub skipped_undos: Vec<String>,
    pub last_http_transient: bool,
    /// The last request timed out after it may have reached the server.
    pub last_http_timed_out: bool,
    /// The last request got no response (connection failure or timeout).
    pub last_http_failed: bool,
}

pub struct Shared {
    pub sources: Sources,
    pub units: RefCell<Vec<Rc<Unit>>>,
    pub lib_code: RefCell<CodeBuilder>,
    pub globals: RefCell<HashMap<String, GlobalVar>>,
    pub requests: RefCell<HashMap<String, Rc<RequestRt>>>,
    pub state: RefCell<State>,
    pub flags: Flags,
    pub agent: ureq::Agent,
    pub env: Env,
    pub config: Config,
    pub cur_dir: RefCell<PathBuf>,
    /// Randomness for code outside test cases.
    pub rng: Rng,
    /// Names of all requests and functions, for suggestions.
    pub callables: RefCell<Vec<String>>,
}

impl Shared {
    pub fn unit(&self, id: usize) -> Rc<Unit> {
        self.units.borrow()[id].clone()
    }

    /// Resolve a path relative to the file whose code is running.
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.cur_dir.borrow().join(p)
        }
    }

    /// Run `f` with relative paths resolved against `file`'s directory.
    pub fn in_dir_of<T>(&self, file: FileId, f: impl FnOnce() -> T) -> T {
        let dir = self
            .sources
            .get(file)
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let prev = self.cur_dir.replace(dir);
        let out = f();
        self.cur_dir.replace(prev);
        out
    }

    pub fn current_case(&self) -> Option<Rc<CaseFrame>> {
        self.state.borrow().cases.last().cloned()
    }

    pub fn rng(&self) -> Rng {
        self.current_case()
            .map_or_else(|| self.rng.clone(), |c| c.rng.clone())
    }

    /// Evaluate a unit with the given scope, resolving relative paths against its file.
    pub fn eval(
        &self,
        engine: &Engine,
        unit: usize,
        scope: &mut Scope,
    ) -> Result<Dynamic, Box<EvalAltResult>> {
        let u = self.unit(unit);
        let dir = self
            .sources
            .get(u.file)
            .path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let prev = self.cur_dir.replace(dir);
        let result = engine.eval_ast_with_scope::<Dynamic>(scope, &u.ast);
        self.cur_dir.replace(prev);
        result.map_err(|e| self.attach_unit(e, unit))
    }

    /// Mark an error with the unit it came from, so positions can be mapped later.
    fn attach_unit(&self, err: Box<EvalAltResult>, unit: usize) -> Box<EvalAltResult> {
        self.to_failure(&err, Some(unit)).into_err()
    }

    /// Map a call site for a trace: positions in generated code are dropped.
    fn map_call(&self, source: &str, pos: Position) -> Option<Span> {
        if pos.is_none() {
            return None;
        }
        if source == "lib" {
            return self.lib_code.borrow().map_position_exact(pos);
        }
        let id: usize = source.strip_prefix('u')?.parse().ok()?;
        self.units
            .borrow()
            .get(id)
            .and_then(|u| u.code.map_position_exact(pos))
    }

    pub fn map_pos(&self, source: &str, pos: Position) -> Option<Span> {
        if pos.is_none() {
            return None;
        }
        if source == "lib" {
            return self.lib_code.borrow().map_position(pos);
        }
        let id: usize = source.strip_prefix('u')?.parse().ok()?;
        self.units
            .borrow()
            .get(id)
            .and_then(|u| u.code.map_position(pos))
    }

    /// Turn any Rhai error into a [`Failure`] with source locations.
    pub fn to_failure(&self, err: &EvalAltResult, unit: Option<usize>) -> Failure {
        let mut source = unit.map(|u| format!("u{u}")).unwrap_or_default();
        let mut trace = Vec::new();
        let mut err = err;
        loop {
            match err {
                EvalAltResult::ErrorInFunctionCall(_, src, inner, pos) => {
                    trace.extend(self.map_call(&source, *pos));
                    if !src.is_empty() {
                        source = src.clone();
                    }
                    err = inner;
                }
                EvalAltResult::ErrorRuntime(v, pos) => {
                    let mut f = match v.read_lock::<Failure>() {
                        Some(f) => f.clone(),
                        None => Failure::new(value::display_plain(v)),
                    };
                    let here = self.map_pos(&source, *pos);
                    if f.span.is_none() {
                        f.span = here;
                    } else {
                        trace.extend(here);
                    }
                    trace.extend(std::mem::take(&mut f.trace));
                    f.trace = trace;
                    return f;
                }
                EvalAltResult::ErrorTerminated(token, pos) => {
                    let mut f = Failure::new(
                        token
                            .clone()
                            .into_string()
                            .unwrap_or_else(|_| "terminated".into()),
                    );
                    f.span = self.map_pos(&source, *pos);
                    f.trace = trace;
                    return f;
                }
                other => {
                    let mut f = Failure::new(self.rhai_message(other));
                    f.span = self.map_pos(&source, other.position());
                    f.trace = trace;
                    return f;
                }
            }
        }
    }
}

impl Shared {
    /// Rhai's message without the trailing position, with internal names cleaned up.
    fn rhai_message(&self, err: &EvalAltResult) -> String {
        if let EvalAltResult::ErrorFunctionNotFound(sig, _) = err {
            return self.not_found(sig);
        }
        let mut msg = err.to_string();
        if let Some(i) = msg.rfind(" (line ") {
            msg.truncate(i);
        }
        msg
    }

    fn not_found(&self, sig: &str) -> String {
        let (name, args) = sig.split_once(" (").unwrap_or((sig, ")"));
        let args: Vec<&str> = args
            .trim_end_matches(')')
            .split(", ")
            .filter(|a| !a.is_empty())
            .map(|a| match a {
                "i64" => "int",
                "f64" => "float",
                "&str | ImmutableString | String" | "ImmutableString" | "String" => "string",
                "()" => "null",
                a if a.contains("Array") => "array",
                a if a.contains("Map") => "map",
                a if a.contains("Blob") => "blob",
                a => a.rsplit("::").next().unwrap_or(a),
            })
            .collect();
        if let Some(base) = name.strip_suffix("__kw") {
            let callables = self.callables.borrow();
            if !callables.iter().any(|c| c == base) {
                return self.unknown(base);
            }
            return format!("`{base}` doesn't take named arguments");
        }
        let callables = self.callables.borrow();
        if callables.iter().any(|c| c == name) {
            let n = args.len();
            return format!(
                "`{name}` can't be called with {n} argument{}",
                if n == 1 { "" } else { "s" }
            );
        }
        if name.starts_with("get$") || name.starts_with("set$") || name.starts_with("index$") {
            return format!(
                "no property `{}` on {}",
                name.split('$').nth(1).unwrap_or(name),
                args.first().unwrap_or(&"this value")
            );
        }
        if args.is_empty() || callables.is_empty() {
            return self.unknown(name);
        }
        format!("{} (called with {})", self.unknown(name), args.join(", "))
    }

    fn unknown(&self, name: &str) -> String {
        let callables = self.callables.borrow();
        match crate::diag::suggest(name, callables.iter().map(String::as_str)) {
            Some(s) => format!("`{name}` isn't defined (did you mean `{s}`?)"),
            None => format!("`{name}` isn't defined"),
        }
    }
}

/// A recorded request/response pair, for failure reports.
#[derive(Debug, Clone, Default)]
pub struct Exchange {
    pub method: String,
    pub url: String,
    pub request_headers: Vec<(String, String)>,
    pub request_body: Option<String>,
    pub status: Option<u16>,
    pub response_headers: Vec<(String, String)>,
    pub response_body: Option<String>,
    pub duration: Option<Duration>,
    pub error: Option<String>,
}

/// Why something failed, with where and what was involved.
#[derive(Debug, Clone)]
pub struct Failure {
    pub message: String,
    pub span: Option<Span>,
    /// Labelled values, e.g. the two sides of a failed comparison.
    pub details: Vec<(String, String)>,
    pub exchange: Option<Box<Exchange>>,
    /// Call sites leading to the failure, outermost first.
    pub trace: Vec<Span>,
}

impl Failure {
    pub fn new(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            span: None,
            details: Vec::new(),
            exchange: None,
            trace: Vec::new(),
        }
    }

    pub fn at(mut self, span: Span) -> Self {
        self.span = Some(span);
        self
    }

    pub fn into_err(self) -> Box<EvalAltResult> {
        Box::new(EvalAltResult::ErrorRuntime(
            Dynamic::from(self),
            Position::NONE,
        ))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        for (k, v) in &self.details {
            write!(f, "; {k}: {v}")?;
        }
        Ok(())
    }
}
