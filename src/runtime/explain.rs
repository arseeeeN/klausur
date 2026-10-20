//! Explaining failed expectations. An expression is evaluated part by part, each part once, and
//! the values of the interesting parts are recorded, so `expect !xs.contains(x)` can show what
//! `xs` and `x` were. Parts are never evaluated twice: they may send requests.
//!
//! Each step evaluates the original sub-expressions first, then binds their values to temporary
//! variables and evaluates a small expression over those (`__k0.contains(__k1)`). That runs in the
//! test's own context, so closures defined in the test resolve as usual.

use super::value::{self, TypeTag, display};
use rhai::{
    ASTFlags, Array, BinaryExpr, Dynamic, EvalAltResult, EvalContext, Expr, Expression,
    FnCallExpr, FnPtr, Position, Stmt,
};

type R<T> = Result<T, Box<EvalAltResult>>;

/// Labelled values of the parts of an expression, in evaluation order.
#[derive(Default)]
pub struct Recorded(pub Vec<(String, String)>);

impl Recorded {
    fn add(&mut self, label: String, v: &Dynamic) {
        if v.is::<TypeTag>() || self.0.iter().any(|(l, _)| *l == label) {
            return;
        }
        self.0.push((label, value::truncate(&display(v), 300)));
    }
}

thread_local! {
    /// `__k0`, `__k1`, …: variable expressions compiled once.
    static TEMPS: Vec<Expr> = (0..16)
        .map(|i| only_expr(&format!("__k{i}")))
        .collect();
    /// `__k0.call(__k1)`, for calling a predicate on one element.
    static CALL: Expr = only_expr("__k0.call(__k1)");
}

fn only_expr(src: &str) -> Expr {
    let mut engine = rhai::Engine::new_raw();
    engine.set_optimization_level(rhai::OptimizationLevel::None);
    let ast = engine.compile_expression(src).expect("temp expression");
    match &ast.statements()[0] {
        Stmt::Expr(e) => (**e).clone(),
        _ => unreachable!(),
    }
}

fn temp(i: usize, pos: Position) -> Expr {
    let mut e = TEMPS.with(|t| t[i].clone());
    if let Expr::Variable(_, _, p) = &mut e {
        *p = pos;
    }
    e
}

/// Evaluate `expr` with `values` bound to `__k0`, `__k1`, ….
fn run(ctx: &mut EvalContext, expr: &Expr, values: Vec<Dynamic>) -> R<Dynamic> {
    let len = ctx.scope().len();
    for (i, v) in values.into_iter().enumerate() {
        ctx.scope_mut().push_dynamic(format!("__k{i}"), v);
    }
    let result = ctx.eval_expression_tree(&Expression::from(expr));
    ctx.scope_mut().rewind(len);
    result
}

fn plain(ctx: &mut EvalContext, expr: &Expr) -> R<Dynamic> {
    ctx.eval_expression_tree(&Expression::from(expr))
}

/// Evaluate `expr`, recording the values of its parts.
pub fn eval(ctx: &mut EvalContext, expr: &Expr, rec: &mut Recorded) -> R<Dynamic> {
    match expr {
        Expr::Dot(..) | Expr::Index(..) => chain(ctx, expr, rec),
        Expr::FnCall(call, pos) if explainable_call(call) => {
            // `!x` failing means `x` was true, and named arguments are a generated map.
            let quiet = call.name.ends_with("__kw") || call.name == "!";
            let mut values = Vec::with_capacity(call.args.len());
            for arg in call.args.iter() {
                let v = eval(ctx, arg, rec)?;
                if !quiet && interesting(arg) {
                    rec.add(render(arg), &v);
                }
                values.push(v);
            }
            let mut c = (**call).clone();
            c.args = (0..values.len())
                .map(|i| temp(i, call.args[i].start_position()))
                .collect();
            run(ctx, &Expr::FnCall(Box::new(c), *pos), values)
        }
        Expr::And(parts, _) | Expr::Or(parts, _) => {
            let and = matches!(expr, Expr::And(..));
            for part in parts.iter() {
                let v = eval(ctx, part, rec)?;
                let Ok(b) = v.as_bool() else {
                    // Let Rhai report the type error.
                    return plain(ctx, part);
                };
                if b != and {
                    return Ok(b.into());
                }
            }
            Ok(and.into())
        }
        _ => plain(ctx, expr),
    }
}

/// Calls whose arguments can be evaluated up front and passed as values.
fn explainable_call(call: &FnCallExpr) -> bool {
    !call.capture_parent_scope
        && call.args.len() <= 15
        && !matches!(
            call.name.as_str(),
            "Fn" | "eval" | "is_def_var" | "is_def_fn" | "is_shared" | "curry" | "call"
        )
}

/// Whether a part's value is worth showing: not a literal or a closure.
fn interesting(e: &Expr) -> bool {
    match e {
        Expr::Variable(..) | Expr::Dot(..) | Expr::Index(..) | Expr::MethodCall(..) => true,
        Expr::InterpolatedString(..) => true,
        Expr::FnCall(call, _) => !is_closure(e) && call.name != "curry",
        _ => false,
    }
}

fn is_closure(e: &Expr) -> bool {
    match e {
        Expr::DynamicConstant(v, _) => v.is::<FnPtr>(),
        Expr::FnCall(call, _) => {
            call.name == "curry" && call.args.first().is_some_and(is_closure)
        }
        _ => false,
    }
}

// --- Chains: `a.b(c)[d].e` --------------------------------------------------------------

enum Seg<'a> {
    /// `.prop` or `.method(…)`, with the flags of the link (`?.`).
    Dot(&'a Expr, ASTFlags, Position),
    /// `[index]`.
    Index(&'a Expr, ASTFlags, Position),
}

/// Rhai nests chains to the right: `a.b[0].c` is `Dot(a, Index(b, Dot(0, c)))`, where an
/// `Index` without `BREAK` has the index as the first part of its right side.
fn flatten(expr: &Expr) -> Option<(&Expr, Vec<Seg<'_>>)> {
    let mut segs = Vec::new();
    match expr {
        Expr::Dot(b, flags, pos) => {
            after_dot(&b.rhs, *flags, *pos, &mut segs);
            Some((&b.lhs, segs))
        }
        Expr::Index(b, flags, pos) => {
            after_index(&b.rhs, *flags, *pos, &mut segs);
            Some((&b.lhs, segs))
        }
        _ => None,
    }
}

fn after_dot<'a>(r: &'a Expr, flags: ASTFlags, pos: Position, out: &mut Vec<Seg<'a>>) {
    match r {
        Expr::Dot(b, f, p) => {
            out.push(Seg::Dot(&b.lhs, flags, pos));
            after_dot(&b.rhs, *f, *p, out);
        }
        Expr::Index(b, f, p) => {
            out.push(Seg::Dot(&b.lhs, flags, pos));
            after_index(&b.rhs, *f, *p, out);
        }
        _ => out.push(Seg::Dot(r, flags, pos)),
    }
}

fn after_index<'a>(r: &'a Expr, flags: ASTFlags, pos: Position, out: &mut Vec<Seg<'a>>) {
    if flags.intersects(ASTFlags::BREAK) {
        out.push(Seg::Index(r, flags, pos));
        return;
    }
    match r {
        Expr::Dot(b, f, p) => {
            out.push(Seg::Index(&b.lhs, flags, pos));
            after_dot(&b.rhs, *f, *p, out);
        }
        Expr::Index(b, f, p) => {
            out.push(Seg::Index(&b.lhs, flags, pos));
            after_index(&b.rhs, *f, *p, out);
        }
        _ => out.push(Seg::Index(r, flags, pos)),
    }
}

fn dot(lhs: Expr, rhs: Expr, flags: ASTFlags, pos: Position) -> Expr {
    Expr::Dot(Box::new(BinaryExpr { lhs, rhs }), flags & !ASTFlags::BREAK, pos)
}

fn chain(ctx: &mut EvalContext, expr: &Expr, rec: &mut Recorded) -> R<Dynamic> {
    let Some((root, segs)) = flatten(expr) else {
        return plain(ctx, expr);
    };
    let mut cur = eval(ctx, root, rec)?;
    let mut label = render(root);
    let last = segs.len() - 1;
    for (i, seg) in segs.iter().enumerate() {
        let receiver = temp(0, root.start_position());
        let (next, part) = match seg {
            Seg::Dot(Expr::MethodCall(call, mpos), flags, pos) => {
                if i == last && interesting(root) {
                    rec.add(label.clone(), &cur);
                }
                let mut values = vec![cur.clone()];
                for arg in call.args.iter() {
                    let v = eval(ctx, arg, rec)?;
                    if interesting(arg) {
                        rec.add(render(arg), &v);
                    }
                    values.push(v);
                }
                let part = format!("{}{}", link(*flags), render_call(call));
                if let Some(v) = predicate(ctx, call, &values, &format!("{label}{part}"), rec)? {
                    (v, part)
                } else {
                    let mut c = (**call).clone();
                    c.args = (1..values.len())
                        .map(|i| temp(i, call.args[i - 1].start_position()))
                        .collect();
                    let e = dot(receiver, Expr::MethodCall(Box::new(c), *mpos), *flags, *pos);
                    (run(ctx, &e, values)?, part)
                }
            }
            Seg::Dot(prop, flags, pos) => {
                let e = dot(receiver, (*prop).clone(), *flags, *pos);
                let v = run(ctx, &e, vec![cur.clone()])?;
                // `expect hit.ready` is false: show `hit`.
                if i == last && (v.is_bool() || v.is_unit()) && interesting(root) {
                    rec.add(label.clone(), &cur);
                }
                (v, format!("{}{}", link(*flags), render(prop)))
            }
            Seg::Index(index, flags, pos) => {
                let iv = eval(ctx, index, rec)?;
                if interesting(index) {
                    rec.add(render(index), &iv);
                }
                let e = Expr::Index(
                    Box::new(BinaryExpr {
                        lhs: receiver,
                        rhs: temp(1, index.start_position()),
                    }),
                    (*flags & ASTFlags::NEGATED) | ASTFlags::BREAK,
                    *pos,
                );
                let q = if flags.intersects(ASTFlags::NEGATED) { "?" } else { "" };
                (run(ctx, &e, vec![cur.clone(), iv])?, format!("{q}[{}]", render(index)))
            }
        };
        cur = next;
        label.push_str(&part);
    }
    Ok(cur)
}

fn link(flags: ASTFlags) -> &'static str {
    if flags.intersects(ASTFlags::NEGATED) { "?." } else { "." }
}

/// `xs.all(f)` and `xs.some(f)`: call the predicate on every element and record which ones
/// broke `all` or matched `some`. `None` when this isn't such a call.
fn predicate(
    ctx: &mut EvalContext,
    call: &FnCallExpr,
    values: &[Dynamic],
    label: &str,
    rec: &mut Recorded,
) -> R<Option<Dynamic>> {
    let all = match call.name.as_str() {
        "all" => true,
        "some" => false,
        _ => return Ok(None),
    };
    let [items, f] = values else {
        return Ok(None);
    };
    if !f.is::<FnPtr>() || !items.is_array() {
        return Ok(None);
    }
    let items = items.clone().cast::<Array>();
    let call_expr = CALL.with(|c| c.clone());
    let mut picked = Array::new();
    for item in &items {
        let v = run(ctx, &call_expr, vec![f.clone(), item.clone()])?;
        let Ok(b) = v.as_bool() else {
            return Err(super::Failure::new(format!(
                "the closure passed to `{}` must give true or false, got {}",
                call.name,
                display(&v)
            ))
            .into_err());
        };
        if b != all {
            picked.push(item.clone());
        }
    }
    if !picked.is_empty() {
        let what = if all { "fails for" } else { "holds for" };
        rec.add(format!("{label} {what}"), &picked.clone().into());
    }
    Ok(Some((picked.is_empty() == all).into()))
}

// --- Rendering expressions back to source ------------------------------------------------

/// Render an expression the way it was likely written, for labels.
pub fn render(e: &Expr) -> String {
    match e {
        Expr::Variable(v, ..) => v.1.to_string(),
        Expr::Property(p, _) => p.2.to_string(),
        Expr::StringConstant(s, _) => display(&Dynamic::from(s.clone())),
        Expr::IntegerConstant(i, _) => i.to_string(),
        Expr::FloatConstant(f, _) => f.to_string(),
        Expr::BoolConstant(b, _) => b.to_string(),
        Expr::CharConstant(c, _) => format!("'{c}'"),
        Expr::Unit(_) => "()".into(),
        Expr::DynamicConstant(v, _) if v.is::<FnPtr>() => "|…| …".into(),
        Expr::DynamicConstant(v, _) => display(v),
        Expr::InterpolatedString(..) => "`…`".into(),
        Expr::Array(items, _) => format!(
            "[{}]",
            items.iter().map(render).collect::<Vec<_>>().join(", ")
        ),
        Expr::Map(m, _) => format!(
            "#{{{}}}",
            m.0.iter()
                .map(|(k, v)| format!("{}: {}", k.name, render(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Dot(..) | Expr::Index(..) => match flatten(e) {
            Some((root, segs)) => {
                let mut s = render_operand(root);
                for seg in segs {
                    match seg {
                        Seg::Dot(Expr::MethodCall(c, _), f, _) => {
                            s.push_str(link(f));
                            s.push_str(&render_call(c));
                        }
                        Seg::Dot(p, f, _) => {
                            s.push_str(link(f));
                            s.push_str(&render(p));
                        }
                        Seg::Index(i, f, _) => {
                            let q = if f.intersects(ASTFlags::NEGATED) { "?" } else { "" };
                            s.push_str(&format!("{q}[{}]", render(i)));
                        }
                    }
                }
                s
            }
            None => "…".into(),
        },
        Expr::MethodCall(c, _) => render_call(c),
        Expr::FnCall(..) if is_closure(e) => "|…| …".into(),
        Expr::FnCall(c, _) if c.op_token.is_some() && c.args.len() == 2 => format!(
            "{} {} {}",
            render_operand(&c.args[0]),
            c.name,
            render_operand(&c.args[1])
        ),
        Expr::FnCall(c, _) if c.op_token.is_some() && c.args.len() == 1 => {
            format!("{}{}", c.name, render_operand(&c.args[0]))
        }
        Expr::FnCall(c, _) if c.name == "is" && c.args.len() == 2 => {
            format!("{} is {}", render(&c.args[0]), render(&c.args[1]))
        }
        Expr::FnCall(c, _) => render_call(c),
        Expr::And(parts, _) => join(parts, " && "),
        Expr::Or(parts, _) => join(parts, " || "),
        Expr::Coalesce(parts, _) => join(parts, " ?? "),
        _ => "…".into(),
    }
}

/// Parenthesize operators inside other expressions: `(a + b).len()`, `!(a && b)`.
fn render_operand(e: &Expr) -> String {
    let compound = match e {
        Expr::And(..) | Expr::Or(..) | Expr::Coalesce(..) => true,
        Expr::FnCall(c, _) => c.op_token.is_some() || c.name == "is",
        _ => false,
    };
    if compound {
        format!("({})", render(e))
    } else {
        render(e)
    }
}

fn join(parts: &[Expr], sep: &str) -> String {
    parts
        .iter()
        .map(render_operand)
        .collect::<Vec<_>>()
        .join(sep)
}

/// `f(a, b)`; calls with named arguments (`f__kw(#{a: 1, "_0": x})`) as `f(x, a: 1)`.
fn render_call(c: &FnCallExpr) -> String {
    if let Some(name) = c.name.strip_suffix("__kw")
        && let [Expr::Map(m, _)] = c.args.as_slice()
    {
        let (positional, named): (Vec<_>, Vec<_>) =
            m.0.iter().partition(|(k, _)| k.name.starts_with('_'));
        let args: Vec<String> = positional
            .iter()
            .map(|(_, v)| render(v))
            .chain(named.iter().map(|(k, v)| format!("{}: {}", k.name, render(v))))
            .collect();
        return format!("{name}({})", args.join(", "));
    }
    format!(
        "{}({})",
        c.name,
        c.args.iter().map(render).collect::<Vec<_>>().join(", ")
    )
}
