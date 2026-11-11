//! `setup` and `teardown`: run once for the tests of a file.
//!
//! The setup of a file runs before its first case, in an undo scope of its own that stays open
//! until after its last case. Undos registered during setup, and every `teardown`, go onto that
//! scope, so they run at the end in reverse order, like any other cleanup.

use super::CaseResult;
use super::generators::Rng;
use crate::load::SetupStep;
use crate::runtime::request::UndoRt;
use crate::runtime::undo::{self, ScopeKind, UndoAction};
use crate::runtime::{CaseFrame, Ctx, CtxData, Failure, Shared};
use crate::source::{FileId, Span};
use rhai::{Dynamic, Engine, Scope};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

/// What a file's setup leaves behind for its tests and its teardown.
#[derive(Default)]
pub struct Fixture {
    /// Cleanup to run after the file's last case.
    pub actions: Vec<UndoAction>,
    /// `setup let` values, as data for the worker threads.
    pub values: Vec<(String, serde_json::Value)>,
}

/// Run a file's `setup` items. On failure, the cleanup registered so far is in the fixture too,
/// for [`teardown`] to run right away.
pub fn setup(
    shared: &Rc<Shared>,
    engine: &Engine,
    file: FileId,
    steps: &[(Span, SetupStep)],
    seed: u64,
) -> (CaseResult, Fixture) {
    let started = Instant::now();
    let frame = frame("setup", seed);
    shared.state.borrow_mut().cases.push(frame.clone());
    undo::open(shared, ScopeKind::Scoped);
    let mut fixture = Fixture::default();
    let outcome = run_steps(shared, engine, file, steps, &mut fixture).map_err(|mut f| {
        if f.message == "Variable not found: ctx" {
            f.message = "there's no `ctx` in `setup`: it runs once for the file, outside any \
                         test; hand values to the tests with `setup let`"
                .into();
        }
        f
    });
    if let Some(scope) = shared.state.borrow_mut().scopes.pop() {
        fixture.actions = scope.actions;
    }
    if let Some(session) = frame.browser.take() {
        session.close();
    }
    shared.state.borrow_mut().cases.pop();
    let result = finished("setup", &frame, outcome, started);
    (result, fixture)
}

fn run_steps(
    shared: &Rc<Shared>,
    engine: &Engine,
    file: FileId,
    steps: &[(Span, SetupStep)],
    fixture: &mut Fixture,
) -> Result<(), Failure> {
    for (span, step) in steps {
        match step {
            SetupStep::Run(unit) => {
                let _ = shared
                    .eval(engine, *unit, &mut Scope::new())
                    .map_err(|e| shared.to_failure(&e, None))?;
            }
            SetupStep::Let(name, unit) => {
                let value = shared
                    .eval(engine, *unit, &mut Scope::new())
                    .map_err(|e| shared.to_failure(&e, None))?;
                let data =
                    rhai::serde::from_dynamic::<serde_json::Value>(&value).map_err(|_| {
                        Failure::new(format!(
                        "`{name}` must be data (a number, string, array or map), not {}, so that \
                         parallel tests get it too",
                        value.type_name()
                    ))
                    .at(*span)
                    })?;
                set_value(shared, name, value);
                fixture.values.push((name.clone(), data));
            }
            SetupStep::Teardown(unit) => {
                let dir = shared.in_dir_of(file, || shared.cur_dir.borrow().clone());
                if let Some(scope) = shared.state.borrow_mut().scopes.last_mut() {
                    scope.actions.push(UndoAction {
                        label: "teardown".into(),
                        origin: format!("teardown in {}", shared.sources.get(file).name),
                        captures: Vec::new(),
                        kind: UndoRt::Call(*unit),
                        env: Vec::new(),
                        span: *span,
                        dir,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Run what a file's setup left to clean up. Failures show up as dirty, like a failed undo.
pub fn teardown(
    shared: &Rc<Shared>,
    engine: &Engine,
    actions: Vec<UndoAction>,
    seed: u64,
) -> CaseResult {
    let started = Instant::now();
    let frame = frame("teardown", seed);
    shared.state.borrow_mut().cases.push(frame.clone());
    undo::unwind(shared, engine, actions);
    if let Some(session) = frame.browser.take() {
        session.close();
    }
    shared.state.borrow_mut().cases.pop();
    finished("teardown", &frame, Ok(()), started)
}

/// Make `setup let` values from the main thread available on a worker.
pub fn receive_values(shared: &Shared, values: &[(String, serde_json::Value)]) {
    for (name, data) in values {
        if let Ok(value) = rhai::serde::to_dynamic(data) {
            set_value(shared, name, value);
        }
    }
}

fn set_value(shared: &Shared, name: &str, value: Dynamic) {
    if let Some(g) = shared.globals.borrow_mut().get_mut(name) {
        g.value = Some(value);
    }
}

fn frame(name: &str, seed: u64) -> Rc<CaseFrame> {
    Rc::new(CaseFrame {
        ctx: Ctx(Rc::new(RefCell::new(CtxData {
            test: name.into(),
            ..Default::default()
        }))),
        dirty: RefCell::default(),
        on_request: Vec::new(),
        on_dirty: Vec::new(),
        rng: Rng::new(seed),
        run: None,
        browser: Rc::default(),
    })
}

fn finished(
    name: &str,
    frame: &CaseFrame,
    outcome: Result<(), Failure>,
    started: Instant,
) -> CaseResult {
    CaseResult {
        test: name.into(),
        label: None,
        args: Vec::new(),
        outcome,
        dirty: frame
            .dirty
            .take()
            .iter()
            .map(undo::DirtyReport::from)
            .collect(),
        duration: started.elapsed(),
        logs: std::mem::take(&mut frame.ctx.0.borrow_mut().logs),
        generated: false,
    }
}
