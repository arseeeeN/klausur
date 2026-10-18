//! Undo scopes: `scoped`, `transaction`, the `rollback` modifier, and unwinding.

use super::request::{self, UndoRt};
use super::{Exchange, Failure, Shared, http};
use crate::source::Span;
use rhai::{Dynamic, Engine, Scope};
use std::rc::Rc;

#[derive(Clone)]
pub struct UndoAction {
    /// The request that registered it.
    pub label: String,
    /// `POST https://… → 201`
    pub origin: String,
    pub captures: Vec<(String, String)>,
    pub kind: UndoRt,
    /// Parameters, captures and response values of the original request.
    pub env: Vec<(String, Dynamic)>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    /// Always unwinds when it ends.
    Scoped,
    /// Unwinds on failure, commits to the enclosing scope on success.
    Transaction,
}

pub struct UndoScope {
    pub kind: ScopeKind,
    pub actions: Vec<UndoAction>,
}

/// An undo action that couldn't be completed: the environment may contain leftover state.
#[derive(Clone)]
pub struct DirtyEntry {
    pub action: UndoAction,
    pub attempts: Vec<String>,
    pub exchange: Option<Box<Exchange>>,
    /// An earlier undo in the same unwind failed too; this may be a consequence.
    pub consequent: bool,
}

pub fn open(shared: &Shared, kind: ScopeKind) {
    shared.state.borrow_mut().scopes.push(UndoScope {
        kind,
        actions: Vec::new(),
    });
}

/// Close the innermost scope: commit or unwind depending on its kind and the outcome.
pub fn close(shared: &Rc<Shared>, engine: &Engine, success: bool) {
    let Some(scope) = shared.state.borrow_mut().scopes.pop() else {
        return;
    };
    if scope.kind == ScopeKind::Transaction && success {
        // Commit: the enclosing scope takes over; without one, the state stays.
        if let Some(parent) = shared.state.borrow_mut().scopes.last_mut() {
            parent.actions.extend(scope.actions);
        }
        return;
    }
    unwind(shared, engine, scope.actions);
}

/// `undo(|| ...)` / `undo(label, || ...)`: register a script action in the innermost scope.
/// Returns whether it was registered (a scope was open).
pub fn register_script(
    shared: &Shared,
    label: String,
    f: rhai::FnPtr,
    unit: Option<usize>,
    span: Option<Span>,
) -> bool {
    if shared.flags.unwinding.get() > 0 {
        return false;
    }
    let mut state = shared.state.borrow_mut();
    let Some(top) = state.scopes.last_mut() else {
        state.skipped_undos.push(label);
        return false;
    };
    top.actions.push(UndoAction {
        label,
        origin: "registered by `undo(...)`".into(),
        captures: Vec::new(),
        kind: UndoRt::Script(f, unit),
        env: Vec::new(),
        span: span.unwrap_or(Span::new(0, 0, 0)),
    });
    true
}

pub fn unwind(shared: &Rc<Shared>, engine: &Engine, actions: Vec<UndoAction>) {
    if actions.is_empty() {
        return;
    }
    shared.flags.unwinding.set(shared.flags.unwinding.get() + 1);
    let mut failed_before = false;
    for action in actions.into_iter().rev() {
        if let Err((attempts, exchange)) = run_with_retries(shared, engine, &action) {
            let entry = DirtyEntry {
                action,
                attempts,
                exchange,
                consequent: failed_before,
            };
            failed_before = true;
            match shared.current_case() {
                Some(case) => case.dirty.borrow_mut().push(entry),
                None => shared.state.borrow_mut().orphan_dirty.push(entry),
            }
        }
    }
    shared.flags.unwinding.set(shared.flags.unwinding.get() - 1);
}

type Attempts = (Vec<String>, Option<Box<Exchange>>);

fn run_with_retries(
    shared: &Rc<Shared>,
    engine: &Engine,
    action: &UndoAction,
) -> Result<(), Attempts> {
    let mut attempts = Vec::new();
    let mut last_exchange = None;
    let max = shared.config.undo_attempts.max(1);
    for attempt in 1..=max {
        match run_once(shared, engine, action) {
            Ok(()) => return Ok(()),
            Err((failure, transient)) => {
                attempts.push(failure.to_string());
                last_exchange = failure.exchange.clone();
                if !transient || attempt == max {
                    break;
                }
                std::thread::sleep(shared.config.undo_backoff * attempt);
            }
        }
    }
    Err((attempts, last_exchange))
}

/// Run an undo action once. On failure, also says whether retrying might help.
fn run_once(
    shared: &Rc<Shared>,
    engine: &Engine,
    action: &UndoAction,
) -> Result<(), (Failure, bool)> {
    let mut scope = Scope::new();
    for (name, value) in &action.env {
        scope.push_dynamic(name.clone(), value.clone());
    }
    match &action.kind {
        UndoRt::Request(body) => {
            let (result, exchange) =
                request::perform(shared, engine, body, &mut scope).map_err(|e| {
                    (
                        shared.to_failure(&e, None),
                        shared.state.borrow().last_http_transient,
                    )
                })?;
            let status = result
                .get("status")
                .and_then(|s| s.as_int().ok())
                .unwrap_or(0) as u16;
            if body.expect.is_empty() {
                if (200..300).contains(&status) || status == 404 || status == 410 {
                    return Ok(());
                }
                let mut f =
                    Failure::new(format!("{} {} → {status}", exchange.method, exchange.url));
                f.exchange = Some(Box::new(exchange));
                return Err((f, http::is_transient_status(status)));
            }
            request::check(shared, engine, &body.expect, &mut scope, &exchange).map_err(|e| {
                (
                    shared.to_failure(&e, None),
                    http::is_transient_status(status),
                )
            })
        }
        UndoRt::Script(f, unit) => f
            .call::<Dynamic>(
                engine,
                &unit.map_or_else(rhai::AST::empty, |u| shared.unit(u).ast.clone()),
                (),
            )
            .map(|_| ())
            .map_err(|e| {
                (
                    shared.to_failure(&e, None),
                    shared.state.borrow().last_http_transient,
                )
            }),
        UndoRt::Call(unit) => shared
            .eval(engine, *unit, &mut scope)
            .map(|_| ())
            .map_err(|e| {
                (
                    shared.to_failure(&e, None),
                    shared.state.borrow().last_http_transient,
                )
            }),
    }
}
