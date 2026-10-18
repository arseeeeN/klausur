//! Tests: definitions built by modifiers, case expansion, and running cases through hooks.

pub mod generators;

use crate::runtime::undo::{self, DirtyEntry, ScopeKind};
use crate::runtime::value::{self, display, map_of};
use crate::runtime::{CaseFrame, Ctx, CtxData, Failure, ScriptFn, Shared};
use crate::source::FileId;
use generators::{Gen, Rng, derive_seed};
use rhai::{Array, Dynamic, Engine, EvalAltResult, FnPtr, ImmutableString, Map, Scope};
use std::cell::RefCell;
use std::ops::{Range, RangeInclusive};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Modifiers implemented natively, plus the hook API usable directly as modifiers.
pub const BUILTIN_MODIFIERS: &[&str] = &[
    "rollback",
    "timeout",
    "retries",
    "runs",
    "seed",
    "tags",
    "skip",
    "only",
    "before",
    "after",
    "around",
    "on_request",
    "on_dirty",
    "meta",
];

/// Runs per case when a test has generator parameters and no `runs(n)`.
pub const DEFAULT_RUNS: u64 = 25;

pub struct TestParamRt {
    pub name: String,
    pub source: Option<usize>,
}

pub struct TestRt {
    pub name: String,
    pub file: FileId,
    pub params: Vec<TestParamRt>,
    pub cases: Option<usize>,
    /// The generated modifier script.
    pub modifiers: Option<usize>,
    pub body: usize,
}

#[derive(Clone)]
pub enum Hook {
    Script(FnPtr),
    Rollback,
    Retries(u32),
}

#[derive(Clone, Default)]
pub struct Layer {
    pub before: Vec<FnPtr>,
    pub after: Vec<FnPtr>,
    pub around: Vec<Hook>,
}

/// The test definition that modifiers receive as `this`.
#[derive(Clone)]
pub struct TestDef(pub Rc<RefCell<DefData>>);

#[derive(Default)]
pub struct DefData {
    pub name: String,
    pub layers: Vec<Layer>,
    pub current: usize,
    pub meta: Map,
    pub on_request: Vec<FnPtr>,
    pub on_dirty: Vec<FnPtr>,
    pub runs: Option<u64>,
    pub seed: Option<u64>,
    pub timeout: Option<Duration>,
    pub skip: Option<String>,
    pub only: bool,
    pub tags: Vec<String>,
}

impl TestDef {
    fn layer(&self) -> std::cell::RefMut<'_, Layer> {
        std::cell::RefMut::map(self.0.borrow_mut(), |d| {
            let i = d.current;
            if d.layers.len() <= i {
                d.layers.resize(i + 1, Layer::default());
            }
            &mut d.layers[i]
        })
    }
}

type R<T> = Result<T, Box<EvalAltResult>>;

pub fn parse_duration(v: &Dynamic) -> R<Duration> {
    if let Ok(ms) = v.as_int() {
        return Ok(Duration::from_millis(ms.max(0) as u64));
    }
    let s = v.to_string();
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(s.len()),
    );
    let n: f64 = num.parse().map_err(|_| {
        Failure::new(format!(
            "invalid duration {s:?}; use e.g. \"5s\" or \"500ms\""
        ))
        .into_err()
    })?;
    let secs = match unit.trim() {
        "ms" => n / 1000.0,
        "s" | "" => n,
        "m" | "min" => n * 60.0,
        _ => {
            return Err(Failure::new(format!(
                "invalid duration {s:?}; use e.g. \"5s\" or \"500ms\""
            ))
            .into_err());
        }
    };
    Ok(Duration::from_secs_f64(secs))
}

/// Register the test-definition type, the built-in modifiers and the `run` callback.
pub fn register(engine: &mut Engine, shared: &Rc<Shared>) {
    engine.register_type_with_name::<TestDef>("Test");
    engine.register_fn("__layer", |t: &mut TestDef, i: i64| {
        let mut d = t.0.borrow_mut();
        d.current = i as usize;
        let n = d.current + 1;
        if d.layers.len() < n {
            d.layers.resize(n, Layer::default());
        }
    });
    engine.register_get("name", |t: &mut TestDef| t.0.borrow().name.clone());
    engine.register_fn("before", |t: &mut TestDef, f: FnPtr| {
        t.layer().before.push(f)
    });
    engine.register_fn("after", |t: &mut TestDef, f: FnPtr| t.layer().after.push(f));
    engine.register_fn("around", |t: &mut TestDef, f: FnPtr| {
        t.layer().around.push(Hook::Script(f))
    });
    engine.register_fn("on_request", |t: &mut TestDef, f: FnPtr| {
        t.0.borrow_mut().on_request.push(f)
    });
    engine.register_fn("on_dirty", |t: &mut TestDef, f: FnPtr| {
        t.0.borrow_mut().on_dirty.push(f)
    });
    engine.register_fn("meta", |t: &mut TestDef, k: ImmutableString, v: Dynamic| {
        t.0.borrow_mut().meta.insert(k.as_str().into(), v);
    });
    engine.register_fn("meta", |t: &mut TestDef, k: ImmutableString| {
        t.0.borrow()
            .meta
            .get(k.as_str())
            .cloned()
            .unwrap_or(Dynamic::UNIT)
    });
    engine.register_fn("rollback", |t: &mut TestDef| {
        t.layer().around.push(Hook::Rollback)
    });
    engine.register_fn("retries", |t: &mut TestDef, n: i64| {
        t.layer().around.push(Hook::Retries(n.max(0) as u32))
    });
    engine.register_fn("runs", |t: &mut TestDef, n: i64| {
        t.0.borrow_mut().runs = Some(n.max(1) as u64)
    });
    engine.register_fn("seed", |t: &mut TestDef, n: i64| {
        t.0.borrow_mut().seed = Some(n as u64)
    });
    engine.register_fn("timeout", |t: &mut TestDef, v: Dynamic| -> R<()> {
        t.0.borrow_mut().timeout = Some(parse_duration(&v)?);
        Ok(())
    });
    engine.register_fn("skip", |t: &mut TestDef| {
        t.0.borrow_mut().skip = Some(String::new())
    });
    engine.register_fn("skip", |t: &mut TestDef, reason: Dynamic| {
        t.0.borrow_mut().skip = Some(value::display_plain(&reason))
    });
    engine.register_fn("only", |t: &mut TestDef| t.0.borrow_mut().only = true);
    engine.register_fn("tags", |t: &mut TestDef, tags: Array| {
        t.0.borrow_mut()
            .tags
            .extend(tags.iter().map(value::display_plain));
    });
    for arity in 1..=8 {
        engine.register_raw_fn(
            "tags",
            {
                let mut types = vec![std::any::TypeId::of::<TestDef>()];
                types.extend(std::iter::repeat_n(
                    std::any::TypeId::of::<Dynamic>(),
                    arity,
                ));
                types
            },
            |_, args| {
                let (first, rest) = args.split_first_mut().unwrap();
                let t = first.write_lock::<TestDef>().unwrap().clone();
                t.0.borrow_mut()
                    .tags
                    .extend(rest.iter().map(|v| value::display_plain(v)));
                Ok(())
            },
        );
    }
    let s = shared.clone();
    engine.register_raw_fn(
        "__klausur_run",
        [std::any::TypeId::of::<i64>(), std::any::TypeId::of::<i64>()],
        move |ncx, args| {
            let idx = args[0].as_int().unwrap_or(0) as usize;
            let k = args[1].as_int().unwrap_or(0) as usize;
            let Some(run) = s.current_case().and_then(|c| c.run.clone()) else {
                return Err(
                    Failure::new("`run` can only be called while a test case runs").into_err(),
                );
            };
            let outcome = run_around(&s, ncx.engine(), &run, idx, k);
            Ok(result_map(&run, outcome))
        },
    );
}

// ---------------------------------------------------------------------------------------------
// Running

pub type Outcome = Result<(), Failure>;

pub struct CaseRun {
    pub test: Rc<TestRt>,
    pub layers: Vec<Layer>,
    pub args: Vec<(String, Dynamic)>,
    /// The unit whose AST holds the hook closures.
    pub hook_unit: usize,
    pub outcomes: RefCell<Vec<Outcome>>,
    pub ctx: Ctx,
}

pub struct CaseResult {
    pub test: String,
    pub args: Vec<(String, String)>,
    pub outcome: Outcome,
    pub dirty: Vec<DirtyEntry>,
    pub duration: Duration,
    pub logs: Vec<String>,
    pub generated: bool,
}

impl CaseResult {
    pub fn label(&self) -> String {
        if self.args.is_empty() {
            return self.test.clone();
        }
        let args: Vec<String> = self
            .args
            .iter()
            .map(|(k, v)| format!("{k}={}", value::truncate(v, 40)))
            .collect();
        format!("{} [{}]", self.test, args.join(", "))
    }
}

fn result_map(run: &CaseRun, outcome: Outcome) -> Dynamic {
    let mut outcomes = run.outcomes.borrow_mut();
    let id = outcomes.len() as i64;
    let error = match &outcome {
        Ok(()) => Dynamic::UNIT,
        Err(f) => f.to_string().into(),
    };
    let passed = outcome.is_ok();
    outcomes.push(outcome);
    map_of([
        ("passed", passed.into()),
        ("error", error),
        ("id", id.into()),
    ])
    .into()
}

fn call_hook(
    shared: &Rc<Shared>,
    engine: &Engine,
    run: &CaseRun,
    f: &FnPtr,
    args: impl rhai::FuncArgs,
) -> Result<Dynamic, Failure> {
    let unit = shared.unit(run.hook_unit);
    shared
        .in_dir_of(run.test.file, || f.call::<Dynamic>(engine, &unit.ast, args))
        .map_err(|e| shared.to_failure(&e, Some(run.hook_unit)))
}

fn run_layers(shared: &Rc<Shared>, engine: &Engine, run: &CaseRun, idx: usize) -> Outcome {
    if idx < run.layers.len() {
        return run_around(shared, engine, run, idx, 0);
    }
    let mut scope = Scope::new();
    scope.push("ctx", run.ctx.clone());
    for (k, v) in &run.args {
        scope.push_dynamic(k.clone(), v.clone());
    }
    shared
        .eval(engine, run.test.body, &mut scope)
        .map(|_| ())
        .map_err(|e| shared.to_failure(&e, None))
}

/// Run layer `idx` starting at its `k`-th around hook.
pub fn run_around(
    shared: &Rc<Shared>,
    engine: &Engine,
    run: &CaseRun,
    idx: usize,
    k: usize,
) -> Outcome {
    let layer = &run.layers[idx];
    if let Some(hook) = layer.around.get(k) {
        return match hook {
            Hook::Rollback => {
                undo::open(shared, ScopeKind::Scoped);
                let outcome = run_around(shared, engine, run, idx, k + 1);
                undo::close(shared, engine, outcome.is_ok());
                outcome
            }
            Hook::Retries(n) => {
                let mut outcome = run_around(shared, engine, run, idx, k + 1);
                for attempt in 1..=*n {
                    if outcome.is_ok() || shared.flags.interrupted() {
                        break;
                    }
                    run.ctx
                        .0
                        .borrow_mut()
                        .logs
                        .push(format!("attempt {attempt} failed, retrying"));
                    outcome = run_around(shared, engine, run, idx, k + 1);
                }
                outcome
            }
            Hook::Script(f) => {
                let mut next = FnPtr::new("__klausur_run").expect("valid name");
                next.add_curry(Dynamic::from(idx as i64))
                    .add_curry(Dynamic::from((k + 1) as i64));
                let before = run.outcomes.borrow().len();
                let ret = call_hook(shared, engine, run, f, (run.ctx.clone(), next))?;
                interpret_around(run, ret, before)
            }
        };
    }
    let mut outcome = Ok(());
    for f in &layer.before {
        if let Err(e) = call_hook(shared, engine, run, f, (run.ctx.clone(),)) {
            outcome = Err(e);
            break;
        }
    }
    if outcome.is_ok() {
        outcome = run_layers(shared, engine, run, idx + 1);
    }
    for f in &layer.after {
        let result = map_of([
            ("passed", outcome.is_ok().into()),
            (
                "error",
                outcome
                    .as_ref()
                    .err()
                    .map_or(Dynamic::UNIT, |f| f.to_string().into()),
            ),
        ]);
        if let Err(e) = call_hook(
            shared,
            engine,
            run,
            f,
            (run.ctx.clone(), Dynamic::from(result)),
        ) && outcome.is_ok()
        {
            outcome = Err(e);
        }
    }
    outcome
}

fn interpret_around(run: &CaseRun, ret: Dynamic, before: usize) -> Outcome {
    let outcomes = run.outcomes.borrow();
    if let Some(map) = ret.try_cast::<Map>()
        && let Some(passed) = map.get("passed").and_then(|p| p.as_bool().ok())
    {
        let base = map
            .get("id")
            .and_then(|i| i.as_int().ok())
            .and_then(|i| outcomes.get(i as usize))
            .cloned();
        return match (passed, base) {
            (true, _) => Ok(()),
            (false, Some(Err(f))) => Err(f),
            (false, _) => Err(Failure::new(
                map.get("error")
                    .filter(|e| !e.is_unit())
                    .map_or("failed by an `around` hook".into(), |e| e.to_string()),
            )),
        };
    }
    // Not a result: use the last inner outcome if `run` was called, otherwise the hook skipped the case.
    if outcomes.len() > before {
        outcomes.last().cloned().unwrap()
    } else {
        Ok(())
    }
}

pub struct CaseSpec {
    pub args: Vec<(String, Dynamic)>,
    pub seed: u64,
    pub generated: bool,
}

enum Source {
    Values(Vec<Dynamic>),
    Gen(Gen),
}

/// Expand a test's parameter sources into concrete cases.
pub fn expand_cases(
    shared: &Rc<Shared>,
    engine: &Engine,
    test: &TestRt,
    def: &DefData,
    base_seed: u64,
) -> Result<Vec<CaseSpec>, Failure> {
    let mut sources: Vec<(usize, Source)> = Vec::new();
    for (i, p) in test.params.iter().enumerate() {
        let Some(unit) = p.source else { continue };
        let v = shared
            .eval(engine, unit, &mut Scope::new())
            .map_err(|e| shared.to_failure(&e, None))?;
        let source = if let Some(g) = v.read_lock::<Gen>() {
            Source::Gen(g.clone())
        } else if v.is_array() {
            Source::Values(v.cast::<Array>())
        } else if let Some(r) = v.read_lock::<Range<i64>>() {
            Source::Values(r.clone().map(Dynamic::from).collect())
        } else if let Some(r) = v.read_lock::<RangeInclusive<i64>>() {
            Source::Values(r.clone().map(Dynamic::from).collect())
        } else {
            let span = shared.unit(unit).code.map_offset(0);
            let mut f = Failure::new(format!(
                "`{}` needs a list, range or generator, got {}",
                p.name,
                value::type_name(&v)
            ));
            f.span = span;
            return Err(f);
        };
        sources.push((i, source));
    }
    let mut rows: Vec<Map> = vec![Map::new()];
    if let Some(unit) = test.cases {
        let v = shared
            .eval(engine, unit, &mut Scope::new())
            .map_err(|e| shared.to_failure(&e, None))?;
        let span = shared.unit(unit).code.map_offset(0);
        let Some(items) = v.clone().try_cast::<Array>() else {
            let mut f = Failure::new(format!(
                "`cases` needs a list of maps, got {}",
                value::type_name(&v)
            ));
            f.span = span;
            return Err(f);
        };
        let from_cases: Vec<&str> = test
            .params
            .iter()
            .filter(|p| p.source.is_none())
            .map(|p| p.name.as_str())
            .collect();
        rows.clear();
        for (n, item) in items.into_iter().enumerate() {
            let Some(row) = item.clone().try_cast::<Map>() else {
                let mut f = Failure::new(format!("case {} isn't a map: {}", n + 1, display(&item)));
                f.span = span;
                return Err(f);
            };
            for name in &from_cases {
                if !row.contains_key(*name) {
                    let mut f = Failure::new(format!("case {} has no value for `{name}`", n + 1));
                    f.span = span;
                    return Err(f);
                }
            }
            if let Some(extra) = row.keys().find(|k| !from_cases.contains(&k.as_str())) {
                let mut f = Failure::new(format!(
                    "case {} has `{extra}`, which isn't a parameter of the test",
                    n + 1
                ));
                f.span = span;
                return Err(f);
            }
            rows.push(row);
        }
    }
    // Cartesian product: rows × each list source.
    let mut combos: Vec<Map> = rows;
    for (i, source) in &sources {
        if let Source::Values(values) = source {
            let name = &test.params[*i].name;
            combos = combos
                .into_iter()
                .flat_map(|c| {
                    values.iter().map(move |v| {
                        let mut c = c.clone();
                        c.insert(name.as_str().into(), v.clone());
                        c
                    })
                })
                .collect();
        }
    }
    let generated = sources.iter().any(|(_, s)| matches!(s, Source::Gen(_)));
    let runs = def.runs.unwrap_or(if generated { DEFAULT_RUNS } else { 1 });
    let base = def.seed.unwrap_or(base_seed);
    let file = shared.sources.get(test.file).name.as_bytes();
    let mut cases = Vec::new();
    for (ci, combo) in combos.iter().enumerate() {
        for ri in 0..runs {
            let seed = derive_seed(
                base,
                &[
                    file,
                    test.name.as_bytes(),
                    &ci.to_le_bytes(),
                    &ri.to_le_bytes(),
                ],
            );
            let rng = Rng::new(seed);
            let mut args = Vec::new();
            for (i, p) in test.params.iter().enumerate() {
                let v = match sources.iter().find(|(si, _)| *si == i) {
                    Some((_, Source::Gen(g))) => {
                        let unit = shared.unit(p.source.unwrap());
                        g.sample(&rng, &mut |f, r| f.call::<Dynamic>(engine, &unit.ast, (r,)))
                            .map_err(|e| shared.to_failure(&e, p.source))?
                    }
                    _ => combo.get(p.name.as_str()).cloned().unwrap_or(Dynamic::UNIT),
                };
                args.push((p.name.clone(), v));
            }
            cases.push(CaseSpec {
                args,
                seed,
                generated,
            });
        }
    }
    Ok(cases)
}

/// Evaluate a test's modifiers into its definition.
pub fn build_def(shared: &Rc<Shared>, engine: &Engine, test: &TestRt) -> Result<TestDef, Failure> {
    let def = TestDef(Rc::new(RefCell::new(DefData {
        name: test.name.clone(),
        ..Default::default()
    })));
    if let Some(unit) = test.modifiers {
        let mut scope = Scope::new();
        scope.push("t", def.clone());
        let _ = shared
            .eval(engine, unit, &mut scope)
            .map_err(|e| shared.to_failure(&e, None))?;
    }
    Ok(def)
}

pub fn run_case(
    shared: &Rc<Shared>,
    engine: &Engine,
    test: &Rc<TestRt>,
    def: &DefData,
    spec: CaseSpec,
) -> CaseResult {
    let hook_unit = test.modifiers.unwrap_or(test.body);
    let script = |fs: &Vec<FnPtr>| {
        fs.iter()
            .map(|f| ScriptFn {
                fn_ptr: f.clone(),
                unit: hook_unit,
            })
            .collect::<Vec<_>>()
    };
    let ctx = Ctx(Rc::new(RefCell::new(CtxData {
        test: test.name.clone(),
        args: spec
            .args
            .iter()
            .map(|(k, v)| (k.as_str().into(), v.clone()))
            .collect(),
        ..Default::default()
    })));
    let run = Rc::new(CaseRun {
        test: test.clone(),
        layers: def.layers.clone(),
        args: spec.args.clone(),
        hook_unit,
        outcomes: RefCell::default(),
        ctx: ctx.clone(),
    });
    let frame = Rc::new(CaseFrame {
        ctx: ctx.clone(),
        dirty: RefCell::default(),
        on_request: script(&def.on_request),
        on_dirty: script(&def.on_dirty),
        rng: Rng::new(spec.seed),
        run: Some(run.clone()),
    });
    let started = Instant::now();
    let scopes_before = {
        let mut state = shared.state.borrow_mut();
        state.cases.push(frame.clone());
        state.scopes.len()
    };
    shared.flags.deadline.set(def.timeout.map(|t| started + t));
    let mut outcome = run_layers(shared, engine, &run, 0);
    if let (Err(f), Some(t)) = (&mut outcome, def.timeout)
        && started.elapsed() >= t
    {
        f.message = format!("test timed out after {}", humantime(t));
    }
    shared.flags.deadline.set(None);
    while shared.state.borrow().scopes.len() > scopes_before {
        undo::close(shared, engine, false);
    }
    let dirty = frame.dirty.take();
    if !dirty.is_empty() {
        let report: Array = dirty
            .iter()
            .map(|d| {
                map_of([
                    ("request", d.action.label.clone().into()),
                    ("origin", d.action.origin.clone().into()),
                    (
                        "attempts",
                        d.attempts
                            .iter()
                            .cloned()
                            .map(Dynamic::from)
                            .collect::<Array>()
                            .into(),
                    ),
                ])
                .into()
            })
            .collect();
        for hook in &frame.on_dirty {
            if let Err(e) = crate::runtime::request::call_script_fn(
                shared,
                engine,
                hook,
                (ctx.clone(), report.clone()),
            ) {
                ctx.0.borrow_mut().logs.push(format!(
                    "on_dirty hook failed: {}",
                    shared.to_failure(&e, None)
                ));
            }
        }
    }
    shared.state.borrow_mut().cases.pop();
    let logs = std::mem::take(&mut ctx.0.borrow_mut().logs);
    CaseResult {
        test: test.name.clone(),
        args: spec
            .args
            .iter()
            .map(|(k, v)| (k.clone(), display(v)))
            .collect(),
        outcome,
        dirty,
        duration: started.elapsed(),
        logs,
        generated: spec.generated,
    }
}

pub fn humantime(d: Duration) -> String {
    if d.as_millis() < 1000 {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}
