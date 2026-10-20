//! Building the Rhai engine: custom syntax, globals, built-in functions and requests.

use super::explain;
use super::request::{self, Args, RequestRt};
use super::undo::{self, ScopeKind};
use super::value::{self, RawText, TYPE_NAMES, TypeTag, display, type_matches, type_name};
use super::{Ctx, CtxData, Failure, Shared};
use crate::testing::generators;
use rhai::{
    AST, Array, Dynamic, Engine, EvalAltResult, EvalContext, Expr, Expression, FnPtr,
    ImmutableString, Map, Position, Scope,
};
use std::any::TypeId;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

type R<T> = Result<T, Box<EvalAltResult>>;

const COMPARISONS: &[&str] = &["==", "!=", "<", "<=", ">", ">="];

pub fn build(shared: &Rc<Shared>) -> Engine {
    let mut engine = Engine::new();
    // Constant folding formats `${[1, "a"]}` with Rhai's own `to_string`, not the JSON override.
    engine.set_optimization_level(rhai::OptimizationLevel::None);
    engine.set_max_call_levels(128);
    engine.set_max_expr_depths(256, 256);
    register_expect(&mut engine);
    register_is(&mut engine);
    register_resolver(&mut engine, shared);
    register_events(&mut engine, shared);
    register_types(&mut engine, shared);
    register_functions(&mut engine, shared);
    super::builtins::register(&mut engine, shared);
    super::query::register(&mut engine);
    let s = shared.clone();
    generators::register(&mut engine, Rc::new(move || s.rng()));
    engine
}

// ---------------------------------------------------------------------------------------------
// expect / assert / null / is

fn register_expect(engine: &mut Engine) {
    let comparisons: Rc<HashMap<&'static str, AST>> = Rc::new(
        COMPARISONS
            .iter()
            .map(|op| {
                (
                    *op,
                    Engine::new_raw()
                        .compile_expression(format!("__l {op} __r"))
                        .unwrap(),
                )
            })
            .collect(),
    );
    for keyword in ["expect", "assert"] {
        let comparisons = comparisons.clone();
        engine
            .register_custom_syntax([keyword, "$expr$"], false, move |ctx, inputs| match check(
                ctx,
                &comparisons,
                &inputs[0],
            )? {
                None => Ok(Dynamic::UNIT),
                Some((failure, pos)) => Err(Box::new(EvalAltResult::ErrorRuntime(
                    Dynamic::from(failure),
                    pos,
                ))),
            })
            .expect("register expect");
    }
    engine
        .register_custom_syntax(["null"], false, |_, _| Ok(Dynamic::UNIT))
        .expect("register null");
}

/// Evaluate an assertion; on failure, explain which part failed and with which values.
fn check(
    ctx: &mut EvalContext,
    comparisons: &HashMap<&'static str, AST>,
    expr: &Expr,
) -> R<Option<(Failure, Position)>> {
    match expr {
        Expr::And(parts, _) => {
            for part in parts.iter() {
                if let Some(f) = check(ctx, comparisons, part)? {
                    return Ok(Some(f));
                }
            }
            Ok(None)
        }
        Expr::FnCall(call, pos)
            if call.args.len() == 2 && comparisons.contains_key(call.name.as_str()) =>
        {
            let mut rec = explain::Recorded::default();
            let left = explain::eval(ctx, &call.args[0], &mut rec)?;
            let right = explain::eval(ctx, &call.args[1], &mut rec)?;
            let len = ctx.scope().len();
            ctx.scope_mut()
                .push_dynamic("__l", left.clone())
                .push_dynamic("__r", right.clone());
            let result = ctx
                .engine()
                .eval_ast_with_scope::<Dynamic>(ctx.scope_mut(), &comparisons[call.name.as_str()]);
            ctx.scope_mut().rewind(len);
            if result?.as_bool().unwrap_or(false) {
                return Ok(None);
            }
            let (l, r) = (display(&left), display(&right));
            let mut f = if l.len() + r.len() <= 80 {
                Failure::new(format!("expectation failed: {l} {} {r}", call.name))
            } else {
                Failure::new(format!("expectation failed: left {} right", call.name))
            };
            f.details.push(("left".into(), l));
            f.details.push(("right".into(), r));
            f.values = rec.0.into();
            Ok(Some((f, start_of(&call.args[0], *pos))))
        }
        Expr::FnCall(call, pos) if call.args.len() == 2 && call.name == "is" => {
            let v = ctx.eval_expression_tree(&Expression::from(&call.args[0]))?;
            let t = ctx.eval_expression_tree(&Expression::from(&call.args[1]))?;
            let tag = type_tag(&t);
            if type_matches(&v, &tag) {
                return Ok(None);
            }
            let mut f = Failure::new(format!(
                "expected a value of type {tag}, got {}",
                type_name(&v)
            ));
            f.details
                .push(("value".into(), value::truncate(&display(&v), 200)));
            Ok(Some((f, start_of(&call.args[0], *pos))))
        }
        _ => {
            let mut rec = explain::Recorded::default();
            let v = explain::eval(ctx, expr, &mut rec)?;
            match v.as_bool() {
                Ok(true) => Ok(None),
                Ok(false) => {
                    let mut f = Failure::new("expectation failed: the expression is false");
                    f.values = rec.0.into();
                    Ok(Some((f, start_of(expr, expr.position()))))
                }
                Err(_) => Ok(Some((
                    Failure::new(format!(
                        "an expectation must be a boolean, got {} {}",
                        type_name(&v),
                        display(&v)
                    )),
                    expr.position(),
                ))),
            }
        }
    }
}

/// The leftmost position of an expression (binary operators report the operator's position).
fn start_of(expr: &Expr, fallback: Position) -> Position {
    let mut e = expr;
    loop {
        match e {
            Expr::FnCall(call, _) if call.op_token.is_some() && call.args.len() == 2 => {
                e = &call.args[0]
            }
            Expr::Dot(x, ..) | Expr::Index(x, ..) => e = &x.lhs,
            Expr::And(x, _) | Expr::Or(x, _) | Expr::Coalesce(x, _) if !x.is_empty() => e = &x[0],
            _ => {
                let p = e.position();
                return if p.is_none() { fallback } else { p };
            }
        }
    }
}

fn type_tag(t: &Dynamic) -> String {
    if let Some(tag) = t.read_lock::<TypeTag>() {
        return tag.0.clone();
    }
    if t.is_unit() {
        return "null".into();
    }
    t.to_string()
}

fn register_is(engine: &mut Engine) {
    engine
        .register_custom_operator("is", 100)
        .expect("register is");
    engine.register_fn("is", |v: Dynamic, t: TypeTag| type_matches(&v, &t.0));
    engine.register_fn("is", |v: Dynamic, _: ()| v.is_unit());
    engine.register_fn("is", |v: Dynamic, t: ImmutableString| type_matches(&v, &t));
}

// ---------------------------------------------------------------------------------------------
// Globals and events

#[allow(deprecated)]
fn register_resolver(engine: &mut Engine, shared: &Rc<Shared>) {
    let shared = shared.clone();
    engine.on_var(move |name, index, ctx| {
        if index > 0 || ctx.scope().contains(name) {
            return Ok(None);
        }
        if let Some(v) = global(&shared, ctx.engine(), name)? {
            return Ok(Some(v));
        }
        if TYPE_NAMES.contains(&name) {
            return Ok(Some(Dynamic::from(TypeTag(name.to_string()))));
        }
        Ok(None)
    });
}

/// Look up a top-level `let`, evaluating it on first use.
pub fn global(shared: &Rc<Shared>, engine: &Engine, name: &str) -> R<Option<Dynamic>> {
    let unit = {
        let globals = shared.globals.borrow();
        let Some(g) = globals.get(name) else {
            return Ok(None);
        };
        if let Some(v) = &g.value {
            return Ok(Some(v.clone()));
        }
        if g.evaluating {
            return Err(Failure::new(format!("`{name}` depends on itself"))
                .at(g.span)
                .into_err());
        }
        g.unit
    };
    shared
        .globals
        .borrow_mut()
        .get_mut(name)
        .unwrap()
        .evaluating = true;
    let result = shared.eval(engine, unit, &mut Scope::new());
    let mut globals = shared.globals.borrow_mut();
    let g = globals.get_mut(name).unwrap();
    g.evaluating = false;
    let v = result?;
    g.value = Some(v.clone());
    Ok(Some(v))
}

fn register_events(engine: &mut Engine, shared: &Rc<Shared>) {
    let s = shared.clone();
    engine.on_progress(move |ops| {
        if ops % 256 != 0 || s.flags.unwinding.get() > 0 {
            return None;
        }
        if s.flags.interrupted() {
            return Some("interrupted".into());
        }
        match s.flags.deadline.get() {
            Some(d) if Instant::now() >= d => Some("timed out".into()),
            _ => None,
        }
    });
    let s = shared.clone();
    engine.on_print(move |text| log(&s, text.to_string()));
    let s = shared.clone();
    engine.on_debug(move |text, _, _| log(&s, text.to_string()));
}

pub fn log(shared: &Shared, text: String) {
    match shared.current_case() {
        Some(case) => case.ctx.0.borrow_mut().logs.push(text),
        None => eprintln!("{text}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Types

fn register_types(engine: &mut Engine, shared: &Rc<Shared>) {
    // `print(m)` and `${m}` show maps and arrays as JSON, the same as `log` does.
    engine
        .register_fn("to_string", |m: &mut Map| value::display(&Dynamic::from(m.clone())))
        .register_fn("to_string", |a: &mut Array| value::display(&Dynamic::from(a.clone())))
        .register_fn("print", |m: &mut Map| value::display(&Dynamic::from(m.clone())))
        .register_fn("print", |a: &mut Array| value::display(&Dynamic::from(a.clone())));
    engine
        .register_type_with_name::<Failure>("Failure")
        .register_get("message", |f: &mut Failure| f.message.clone())
        .register_fn("to_string", |f: &mut Failure| f.to_string())
        .register_fn("to_debug", |f: &mut Failure| f.to_string());
    engine
        .register_type_with_name::<RawText>("Raw")
        .register_fn("to_string", |r: &mut RawText| r.0.clone());
    engine
        .register_type_with_name::<TypeTag>("Type")
        .register_fn("to_string", |t: &mut TypeTag| t.0.clone());
    engine
        .register_type_with_name::<Ctx>("Context")
        .register_get_set(
            "headers",
            |c: &mut Ctx| c.0.borrow().headers.clone(),
            |c: &mut Ctx, m: Map| c.0.borrow_mut().headers = m,
        )
        .register_get_set(
            "vars",
            |c: &mut Ctx| c.0.borrow().vars.clone(),
            |c: &mut Ctx, m: Map| c.0.borrow_mut().vars = m,
        )
        .register_get("test", |c: &mut Ctx| c.0.borrow().test.clone())
        .register_get("args", |c: &mut Ctx| c.0.borrow().args.clone())
        .register_indexer_get(|c: &mut Ctx, key: &str| {
            c.0.borrow().vars.get(key).cloned().unwrap_or(Dynamic::UNIT)
        })
        .register_indexer_set(|c: &mut Ctx, key: &str, v: Dynamic| {
            c.0.borrow_mut().vars.insert(key.into(), v);
        })
        .register_fn("to_string", |c: &mut Ctx| {
            format!("<context of {:?}>", c.0.borrow().test)
        });
    let s = shared.clone();
    engine.register_fn("log", move |c: &mut Ctx, v: Dynamic| {
        let text = value::display_plain(&v);
        c.0.borrow_mut().logs.push(text.clone());
        if s.current_case().is_none() {
            eprintln!("{text}");
        }
    });
}

// ---------------------------------------------------------------------------------------------
// Functions

fn read(shared: &Shared, path: &str) -> R<String> {
    let full = shared.resolve_path(path);
    std::fs::read_to_string(&full)
        .map_err(|e| Failure::new(format!("can't read {}: {e}", full.display())).into_err())
}

fn register_functions(engine: &mut Engine, shared: &Rc<Shared>) {
    let s = shared.clone();
    engine.register_fn("env", move |name: &str| -> R<String> {
        s.env.get(name).ok_or_else(|| {
            Failure::new(format!("environment variable `{name}` isn't set")).into_err()
        })
    });
    let s = shared.clone();
    engine.register_fn("env", move |name: &str, default: Dynamic| -> Dynamic {
        s.env.get(name).map_or(default, Dynamic::from)
    });
    engine.register_fn("raw", |v: Dynamic| RawText(value::to_text(&v)));
    let s = shared.clone();
    engine.register_fn("read_file", move |path: &str| read(&s, path));
    let s = shared.clone();
    engine.register_fn("json", move |path: &str| -> R<Dynamic> {
        let text = read(&s, path)?;
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| Failure::new(format!("{path} isn't valid JSON: {e}")).into_err())?;
        Ok(value::from_json(&json))
    });
    let s = shared.clone();
    engine.register_fn("csv", move |path: &str| -> R<Array> {
        let text = read(&s, path)?;
        let mut reader = csv::Reader::from_reader(text.as_bytes());
        let headers = reader
            .headers()
            .map_err(|e| Failure::new(format!("{path}: {e}")).into_err())?
            .clone();
        let mut rows = Array::new();
        for record in reader.records() {
            let record = record.map_err(|e| Failure::new(format!("{path}: {e}")).into_err())?;
            let row: Map = headers
                .iter()
                .zip(record.iter())
                .map(|(k, v)| (k.into(), csv_value(v)))
                .collect();
            rows.push(row.into());
        }
        Ok(rows)
    });
    for (name, kind) in [
        ("scoped", ScopeKind::Scoped),
        ("transaction", ScopeKind::Transaction),
    ] {
        let s = shared.clone();
        engine.register_raw_fn(name, [TypeId::of::<FnPtr>()], move |ncx, args| {
            let f = args[0].clone().cast::<FnPtr>();
            undo::open(&s, kind);
            let result = f.call_within_context::<Dynamic>(&ncx, ());
            undo::close(&s, ncx.engine(), result.is_ok());
            result
        });
    }
    for with_label in [false, true] {
        let s = shared.clone();
        let types: &[TypeId] = if with_label {
            &[TypeId::of::<ImmutableString>(), TypeId::of::<FnPtr>()]
        } else {
            &[TypeId::of::<FnPtr>()]
        };
        engine.register_raw_fn("undo", types, move |ncx, args| {
            let f = (*args[args.len() - 1]).clone().cast::<FnPtr>();
            let label = if with_label {
                args[0].clone().cast::<ImmutableString>().to_string()
            } else {
                "script cleanup".to_string()
            };
            let source = ncx.call_source();
            let unit = source.and_then(|src| src.strip_prefix('u')?.parse().ok());
            let span = source.and_then(|src| s.map_pos(src, ncx.call_position()));
            Ok(undo::register_script(&s, label, f, unit, span))
        });
    }
    let s = shared.clone();
    engine.register_fn("ctx", move || -> Ctx {
        s.current_case()
            .map(|c| c.ctx.clone())
            .unwrap_or_else(|| Ctx(Rc::new(std::cell::RefCell::new(CtxData::default()))))
    });
    let s = shared.clone();
    engine.register_fn("uuid", move || generators::uuid(&s.rng()));
    let s = shared.clone();
    engine.register_fn("sleep", move |ms: i64| -> R<()> {
        super::request::wait(&s, std::time::Duration::from_millis(ms.max(0) as u64))
            .map_err(|why| Failure::new(format!("`sleep` stopped: {why}")).into_err())
    });
    engine.register_fn("fail", |msg: Dynamic| -> R<()> {
        Err(Failure::new(value::display_plain(&msg)).into_err())
    });
    let s = shared.clone();
    engine.register_fn("log", move |v: Dynamic| log(&s, value::display_plain(&v)));
}

/// CSV cells are strings; numbers and booleans are converted.
fn csv_value(v: &str) -> Dynamic {
    if let Ok(i) = v.parse::<i64>() {
        return i.into();
    }
    if let Ok(f) = v.parse::<f64>() {
        return f.into();
    }
    match v {
        "true" => true.into(),
        "false" => false.into(),
        _ => v.into(),
    }
}

/// Register the native functions behind a request block: one per accepted arity, plus `name__kw`.
pub fn register_request(engine: &mut Engine, shared: &Rc<Shared>, req: Rc<RequestRt>) {
    for arity in req.required()..=req.params.len() {
        let (s, r) = (shared.clone(), req.clone());
        engine.register_raw_fn(
            req.name.clone(),
            vec![TypeId::of::<Dynamic>(); arity],
            move |ncx, args| {
                let args = args.iter().map(|a| (**a).clone()).collect();
                request::call(&s, ncx.engine(), &r, Args::Positional(args))
            },
        );
    }
    let (s, r) = (shared.clone(), req.clone());
    engine.register_raw_fn(
        format!("{}__kw", req.name),
        [TypeId::of::<Map>()],
        move |ncx, args| {
            let map = args[0].clone().cast::<Map>();
            request::call(&s, ncx.engine(), &r, Args::Named(map))
        },
    );
}
