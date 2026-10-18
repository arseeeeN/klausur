//! Loading `.kl` files into a runnable program.

use crate::diag::{Diag, PResult, suggest};
use crate::rewrite::Rewriter;
use crate::runtime::request::{
    BodyRt, ParamRt, PollRt, RequestRt, SendRt, TPart, TemplateRt, UndoRt,
};
use crate::runtime::{Config, Env, Flags, GlobalVar, Shared, State, Unit, engine};
use crate::source::{CodeBuilder, FileId, Sources, Span};
use crate::syntax::parser::{parse_file, split_top_level};
use crate::syntax::scan::{ident_at, skip_trivia};
use crate::syntax::{self, File, FnDecl, Item, Param, Part, RequestBody, Send, Template, Undo};
use crate::testing::generators::Rng;
use crate::testing::{self, BUILTIN_MODIFIERS, TestParamRt, TestRt};
use rhai::Engine;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub struct Program {
    pub shared: Rc<Shared>,
    pub engine: Engine,
    pub tests: Vec<Rc<TestRt>>,
    pub requests: Vec<Rc<RequestRt>>,
    pub fns: Vec<FnInfo>,
    pub roots: Vec<FileId>,
}

pub struct FnInfo {
    pub name: String,
    pub file: FileId,
    pub params: Vec<(String, Option<String>)>,
}

pub struct LoadError {
    pub sources: Sources,
    pub diags: Vec<Diag>,
}

pub struct LoadOptions {
    pub config: Config,
    pub env_name: Option<String>,
    pub interrupted: Arc<AtomicBool>,
    pub seed: u64,
}

struct Loaded {
    sources: Sources,
    files: Vec<File>,
    aliases: Vec<HashMap<String, FileId>>,
    roots: Vec<FileId>,
}

pub fn load(paths: &[PathBuf], opts: LoadOptions) -> Result<Program, LoadError> {
    let mut diags = Vec::new();
    let loaded = parse_all(paths, &mut diags);
    if !diags.is_empty() {
        return Err(LoadError {
            sources: loaded.sources,
            diags,
        });
    }
    let Loaded {
        sources,
        files,
        aliases,
        roots,
    } = loaded;

    // Symbols: functions and requests share one namespace.
    let mut defs: HashMap<FileId, HashSet<String>> = HashMap::new();
    let mut symbols: HashMap<String, Span> = HashMap::new();
    let mut lets: HashMap<String, Span> = HashMap::new();
    let mut fn_decls: HashMap<String, FnDecl> = HashMap::new();
    for (file, f) in files.iter().enumerate() {
        let mut test_names: HashMap<&str, Span> = HashMap::new();
        for item in &f.items {
            let (name, span) = match item {
                Item::Fn(d) => {
                    fn_decls.insert(d.name.clone(), d.clone());
                    (&d.name, d.name_span)
                }
                Item::Request(r) => (&r.name, r.name_span),
                Item::Let(l) => {
                    if let Some(prev) = lets.insert(l.name.clone(), l.name_span) {
                        diags.push(
                            Diag::at(l.name_span, format!("`{}` is defined twice", l.name))
                                .label(prev, "first defined here"),
                        );
                    }
                    continue;
                }
                Item::Test(t) => {
                    if let Some(prev) = test_names.insert(&t.name, t.name_span) {
                        diags.push(
                            Diag::at(
                                t.name_span,
                                format!("test {:?} is defined twice in this file", t.name),
                            )
                            .label(prev, "first defined here"),
                        );
                    }
                    continue;
                }
                Item::Import(_) => continue,
            };
            defs.entry(file).or_default().insert(name.clone());
            if let Some(prev) = symbols.insert(name.clone(), span) {
                diags.push(
                    Diag::at(span, format!("`{name}` is defined twice"))
                        .label(prev, "first defined here")
                        .help(
                            "requests and functions share one namespace, across all loaded files",
                        ),
                );
            }
        }
    }
    if !diags.is_empty() {
        return Err(LoadError { sources, diags });
    }

    let mut env = Env::default();
    if let Err(d) = load_env(&mut env, opts.env_name.as_deref()) {
        return Err(LoadError {
            sources,
            diags: vec![d],
        });
    }
    let shared = Rc::new(Shared {
        sources,
        units: RefCell::default(),
        lib_code: RefCell::default(),
        globals: RefCell::default(),
        requests: RefCell::default(),
        state: RefCell::new(State::default()),
        flags: Flags {
            interrupted: opts.interrupted,
            deadline: Cell::new(None),
            unwinding: Cell::new(0),
        },
        agent: crate::runtime::http::agent(),
        env,
        config: opts.config,
        cur_dir: RefCell::new(std::env::current_dir().unwrap_or_default()),
        rng: Rng::new(opts.seed),
        callables: RefCell::new(symbols.keys().cloned().collect()),
    });
    let mut engine = engine::build(&shared);
    testing::register(&mut engine, &shared);

    let mut c = Compiler {
        shared: &shared,
        aliases: &aliases,
        defs: &defs,
        diags: Vec::new(),
    };

    // All functions go into one library, registered as a global module so every unit can call them.
    let mut fns = Vec::new();
    let mut lib = CodeBuilder::default();
    let mut fn_starts = Vec::new();
    for (file, f) in files.iter().enumerate() {
        for item in &f.items {
            if let Item::Fn(d) = item {
                if !lib.text.is_empty() && !lib.text.ends_with('\n') {
                    lib.push_gen("\n");
                }
                fn_starts.push(lib.text.len());
                if let Err(e) = c.gen_fn(file, d, &mut lib) {
                    c.diags.push(e);
                }
                let src = &c.shared.sources.get(file).text;
                fns.push(FnInfo {
                    name: d.name.clone(),
                    file,
                    params: d
                        .params
                        .iter()
                        .map(|p| {
                            (
                                p.name.clone(),
                                p.default.map(|s| src[s.range()].to_string()),
                            )
                        })
                        .collect(),
                });
            }
        }
    }
    if !c.diags.is_empty() {
        let diags = c.diags;
        return Err(LoadError {
            sources: take_sources(shared),
            diags,
        });
    }
    match compile_lib(&engine, &lib.text, &fn_starts) {
        Ok(mut ast) => {
            ast.set_source("lib");
            engine.register_global_module(ast.shared_lib().clone());
        }
        Err(e) => {
            let span = lib.map_position(e.1);
            let diag = span.map_or_else(
                || Diag::new(e.0.to_string()),
                |s| Diag::at(s, e.0.to_string()),
            );
            return Err(LoadError {
                sources: take_sources(shared),
                diags: vec![diag],
            });
        }
    }
    *shared.lib_code.borrow_mut() = lib;

    // Requests, globals and tests.
    let mut requests = Vec::new();
    let mut tests = Vec::new();
    for (file, f) in files.iter().enumerate() {
        for item in &f.items {
            match item {
                Item::Request(r) => {
                    let params = r.params.iter().map(|p| c.param(&engine, file, p)).collect();
                    let summary = {
                        let src = &c.shared.sources.get(file).text;
                        format!(
                            "{} {}",
                            r.body.connection.verb,
                            r.body.connection.url.display(src)
                        )
                    };
                    let mods = c.request_modifiers(&engine, file, &r.modifiers);
                    if let Some(mut body) = c.body(&engine, file, &r.body) {
                        body.timeout = mods.timeout;
                        body.poll = mods.poll;
                        requests.push(Rc::new(RequestRt {
                            name: r.name.clone(),
                            span: r.name_span,
                            params,
                            body: Rc::new(body),
                            summary,
                        }));
                    }
                }
                Item::Let(l) => {
                    if let Some(unit) = c.expr_unit(&engine, file, l.expr) {
                        shared.globals.borrow_mut().insert(
                            l.name.clone(),
                            GlobalVar {
                                unit,
                                span: l.name_span,
                                value: None,
                                evaluating: false,
                            },
                        );
                    }
                }
                Item::Test(t) if roots.contains(&file) => {
                    if let Some(test) = c.test(&engine, file, t, &fn_decls) {
                        tests.push(Rc::new(test));
                    }
                }
                _ => {}
            }
        }
    }
    if !c.diags.is_empty() {
        let diags = c.diags;
        return Err(LoadError {
            sources: take_sources(shared),
            diags,
        });
    }
    for r in &requests {
        engine::register_request(&mut engine, &shared, r.clone());
        shared
            .requests
            .borrow_mut()
            .insert(r.name.clone(), r.clone());
    }
    Ok(Program {
        shared,
        engine,
        tests,
        requests,
        fns,
        roots,
    })
}

fn take_sources(shared: Rc<Shared>) -> Sources {
    match Rc::try_unwrap(shared) {
        Ok(s) => s.sources,
        Err(rc) => {
            // Still referenced by engine closures; copy the sources out.
            let mut copy = Sources::default();
            for f in &rc.sources.files {
                copy.add(&f.path, f.text.clone());
            }
            copy
        }
    }
}

fn load_env(env: &mut Env, name: Option<&str>) -> Result<(), Diag> {
    let read = |path: &Path| -> Result<HashMap<String, String>, Diag> {
        let iter = dotenvy::from_path_iter(path)
            .map_err(|e| Diag::new(format!("can't read {}: {e}", path.display())))?;
        iter.map(|r| r.map_err(|e| Diag::new(format!("{}: {e}", path.display()))))
            .collect()
    };
    if let Some(name) = name {
        let path = PathBuf::from(format!(".env.{name}"));
        if !path.exists() {
            return Err(
                Diag::new(format!("environment file `{}` not found", path.display())).help(
                    format!("`--env {name}` reads `.env.{name}` from the current directory"),
                ),
            );
        }
        env.layers.push(read(&path)?);
    }
    let dotenv = Path::new(".env");
    if dotenv.exists() {
        env.layers.push(read(dotenv)?);
    }
    Ok(())
}

/// Expand directories to the `.kl` files inside them.
pub fn expand(paths: &[PathBuf]) -> Result<Vec<PathBuf>, Diag> {
    let mut out = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_dir(path, &mut out);
        } else if path.is_file() {
            out.push(path.clone());
        } else {
            return Err(Diag::new(format!(
                "no such file or directory: {}",
                path.display()
            )));
        }
    }
    Ok(out)
}

fn collect_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        let hidden = p
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
        if p.is_dir()
            && !hidden
            && p.file_name()
                .is_none_or(|n| n != "target" && n != "node_modules")
        {
            collect_dir(&p, out);
        } else if p.extension().is_some_and(|e| e == "kl") {
            out.push(p);
        }
    }
}

fn parse_all(paths: &[PathBuf], diags: &mut Vec<Diag>) -> Loaded {
    let mut l = Loaded {
        sources: Sources::default(),
        files: Vec::new(),
        aliases: Vec::new(),
        roots: Vec::new(),
    };
    let mut by_path: HashMap<PathBuf, FileId> = HashMap::new();
    let mut add = |l: &mut Loaded, path: &Path, diags: &mut Vec<Diag>| -> Option<FileId> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if let Some(id) = by_path.get(&canon) {
            return Some(*id);
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                diags.push(Diag::new(format!("can't read {}: {e}", path.display())));
                return None;
            }
        };
        let id = l.sources.add(&canon, text);
        let (file, errs) = parse_file(id, &l.sources.get(id).text);
        diags.extend(errs);
        l.files.push(file);
        l.aliases.push(HashMap::new());
        by_path.insert(canon, id);
        Some(id)
    };
    match expand(paths) {
        Ok(files) => {
            for p in files {
                if let Some(id) = add(&mut l, &p, diags) {
                    l.roots.push(id);
                }
            }
        }
        Err(d) => diags.push(d),
    }
    let mut i = 0;
    while i < l.files.len() {
        let imports: Vec<syntax::ImportDecl> = l.files[i]
            .items
            .iter()
            .filter_map(|it| {
                if let Item::Import(imp) = it {
                    Some(imp.clone())
                } else {
                    None
                }
            })
            .collect();
        let dir = l
            .sources
            .get(i)
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        for imp in imports {
            let target = dir.join(&imp.path);
            if !target.is_file() {
                diags.push(
                    Diag::at(imp.path_span, format!("can't find `{}`", imp.path))
                        .help("imports are resolved relative to the importing file"),
                );
                continue;
            }
            let Some(id) = add(&mut l, &target, diags) else {
                continue;
            };
            if let Some((alias, span)) = imp.alias
                && l.aliases[i].insert(alias.clone(), id).is_some()
            {
                diags.push(Diag::at(span, format!("the alias `{alias}` is used twice")));
            }
        }
        i += 1;
    }
    l
}

/// Compile each function on its own and merge them. In one compilation, Rhai doesn't capture a
/// variable in a closure when a function of that name was parsed earlier, so a parameter
/// `writer` would break in a closure if any file defined `fn writer()`. Each chunk is padded
/// with newlines so positions match `text` and its source map.
fn compile_lib(
    engine: &Engine,
    text: &str,
    starts: &[usize],
) -> Result<rhai::AST, rhai::ParseError> {
    let mut lib = rhai::AST::empty();
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(text.len());
        let lines = text[..start].matches('\n').count();
        let chunk = format!("{}{}", "\n".repeat(lines), &text[start..end]);
        lib += engine.compile(&chunk)?;
    }
    Ok(lib)
}

#[derive(Default)]
struct RequestMods {
    timeout: Option<usize>,
    poll: Option<PollRt>,
}

struct Compiler<'a> {
    shared: &'a Rc<Shared>,
    aliases: &'a [HashMap<String, FileId>],
    defs: &'a HashMap<FileId, HashSet<String>>,
    diags: Vec<Diag>,
}

impl Compiler<'_> {
    fn src(&self, file: FileId) -> &str {
        &self.shared.sources.get(file).text
    }

    fn rewriter(&self, file: FileId) -> Rewriter<'_> {
        Rewriter {
            src: self.src(file),
            file,
            aliases: &self.aliases[file],
            defs: self.defs,
        }
    }

    /// Compile generated code into a unit.
    fn compile(&mut self, engine: &Engine, file: FileId, code: CodeBuilder) -> Option<usize> {
        match engine.compile(&code.text) {
            Ok(mut ast) => {
                let mut units = self.shared.units.borrow_mut();
                let id = units.len();
                ast.set_source(format!("u{id}"));
                units.push(Rc::new(Unit { ast, code, file }));
                Some(id)
            }
            Err(e) => {
                let mut message = e.0.to_string();
                if message.contains("'null'") || message.contains("'nil'") {
                    message.push_str(" (use `()` or `null` only as a value)");
                }
                let diag = match code.map_position(e.1) {
                    Some(s) => Diag::at(s, message),
                    None => Diag::new(message),
                };
                self.diags.push(diag);
                None
            }
        }
    }

    fn build(
        &mut self,
        file: FileId,
        anchor: Span,
        f: impl FnOnce(&Rewriter, &mut CodeBuilder) -> PResult<()>,
    ) -> Option<CodeBuilder> {
        let mut code = CodeBuilder::new(anchor);
        let result = f(&self.rewriter(file), &mut code);
        match result {
            Ok(()) => Some(code),
            Err(d) => {
                self.diags.push(d);
                None
            }
        }
    }

    fn expr_unit(&mut self, engine: &Engine, file: FileId, span: Span) -> Option<usize> {
        let code = self.build(file, span, |r, out| r.rewrite(span.start, span.end, out))?;
        self.compile(engine, file, code)
    }

    fn expect_unit(&mut self, engine: &Engine, file: FileId, span: Span) -> Option<usize> {
        let code = self.build(file, span, |r, out| {
            out.push_gen("expect ");
            r.rewrite(span.start, span.end, out)
        })?;
        self.compile(engine, file, code)
    }

    fn param(&mut self, engine: &Engine, file: FileId, p: &Param) -> ParamRt {
        ParamRt {
            name: p.name.clone(),
            default: p.default.and_then(|d| self.expr_unit(engine, file, d)),
        }
    }

    fn template(&mut self, engine: &Engine, file: FileId, t: &Template) -> TemplateRt {
        let parts = t
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Lit(s) => Some(TPart::Lit(s.clone())),
                Part::Expr { span, mode } => {
                    self.expr_unit(engine, file, *span).map(|unit| TPart::Expr {
                        unit,
                        mode: *mode,
                        span: *span,
                    })
                }
            })
            .collect();
        TemplateRt {
            parts,
            span: t.span,
        }
    }

    /// Request modifiers: `timeout(...)` and `poll(until: ..., every: ..., within: ...)`.
    fn request_modifiers(&mut self, engine: &Engine, file: FileId, mods: &[Span]) -> RequestMods {
        const NAMES: [&str; 2] = ["timeout", "poll"];
        let src = self.src(file).to_string();
        let mut out = RequestMods::default();
        let mut seen: Vec<&str> = Vec::new();
        for m in mods {
            let Some((ns, ne)) = ident_at(&src, m.start) else {
                continue;
            };
            let name = &src[ns..ne];
            let Some(name) = NAMES.iter().copied().find(|n| *n == name) else {
                let mut d = Diag::at(Span::new(file, ns, ne), format!("`{name}` can't be used on a request"))
                    .help("requests support `timeout(\"5s\")` and `poll(until: ...)`; other modifiers work on tests");
                if let Some(s) = suggest(name, NAMES) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                self.diags.push(d);
                continue;
            };
            if seen.contains(&name) {
                self.diags
                    .push(Diag::at(*m, format!("`{name}` is given twice")));
                continue;
            }
            seen.push(name);
            let usage = match name {
                "timeout" => "`timeout` needs a duration, e.g. `timeout(\"5s\")`",
                _ => {
                    "`poll` needs a condition, e.g. `poll(until: body.state == \"done\", every: \"1s\")`"
                }
            };
            if src.as_bytes().get(ne) != Some(&b'(') || m.end < ne + 2 {
                self.diags.push(Diag::at(*m, usage));
                continue;
            }
            let (s, e) = crate::syntax::scan::trim_range(&src, ne + 1, m.end - 1);
            if s == e {
                self.diags.push(Diag::at(*m, usage));
                continue;
            }
            if name == "timeout" {
                out.timeout = self.expr_unit(engine, file, Span::new(file, s, e));
            } else {
                out.poll = self.poll(engine, file, &src, *m, s, e);
            }
        }
        out
    }

    fn poll(
        &mut self,
        engine: &Engine,
        file: FileId,
        src: &str,
        m: Span,
        s: usize,
        e: usize,
    ) -> Option<PollRt> {
        const ARGS: [&str; 3] = ["until", "every", "within"];
        let parts = match split_top_level(src, s, e) {
            Ok(p) => p,
            Err(err) => {
                self.diags
                    .push(Diag::at(Span::new(file, err.pos, err.pos + 1), err.message));
                return None;
            }
        };
        let mut units: [Option<usize>; 3] = [None; 3];
        let mut ok = true;
        for (ps, pe) in parts {
            let (ps, pe) = crate::syntax::scan::trim_range(src, ps, pe);
            if ps == pe {
                continue;
            }
            let named = ident_at(src, ps).and_then(|(is, ie)| {
                let colon = skip_trivia(src, ie).ok()?;
                (src.as_bytes().get(colon) == Some(&b':')
                    && src.as_bytes().get(colon + 1) != Some(&b':'))
                .then(|| (&src[is..ie], Span::new(file, is, ie), colon + 1))
            });
            let Some((arg, arg_span, value_start)) = named else {
                self.diags.push(
                    Diag::at(Span::new(file, ps, pe), "`poll` arguments need names")
                        .help("write `poll(until: body.state == \"done\", every: \"1s\", within: \"60s\")`"),
                );
                ok = false;
                continue;
            };
            let Some(i) = ARGS.iter().position(|a| *a == arg) else {
                let mut d = Diag::at(arg_span, format!("`poll` has no argument `{arg}`"))
                    .help("`poll` takes `until`, `every` and `within`");
                if let Some(s) = suggest(arg, ARGS) {
                    d = d.help(format!("did you mean `{s}`?"));
                }
                self.diags.push(d);
                ok = false;
                continue;
            };
            if units[i].is_some() {
                self.diags
                    .push(Diag::at(arg_span, format!("`{arg}` is given twice")));
                ok = false;
                continue;
            }
            let (vs, ve) = crate::syntax::scan::trim_range(src, value_start, pe);
            if vs == ve {
                self.diags
                    .push(Diag::at(arg_span, format!("`{arg}` needs a value")));
                ok = false;
                continue;
            }
            units[i] = self.expr_unit(engine, file, Span::new(file, vs, ve));
            ok &= units[i].is_some();
        }
        let Some(until) = units[0] else {
            if ok {
                self.diags.push(
                    Diag::at(m, "`poll` needs an `until` condition")
                        .help("e.g. `poll(until: body.state == \"done\")`"),
                );
            }
            return None;
        };
        ok.then_some(PollRt {
            until,
            every: units[1],
            within: units[2],
            span: m,
        })
    }

    fn body(&mut self, engine: &Engine, file: FileId, b: &RequestBody) -> Option<BodyRt> {
        let errors = self.diags.len();
        let url = self.template(engine, file, &b.connection.url);
        let headers = b
            .connection
            .headers
            .iter()
            .map(|h| (h.name.clone(), self.template(engine, file, &h.value)))
            .collect();
        let send = b.send.as_ref().and_then(|s| match s {
            Send::Json(t) => Some(SendRt::Json(self.template(engine, file, t))),
            Send::Raw(t) => Some(SendRt::Raw(self.template(engine, file, t))),
            Send::Expr(span) => self.expr_unit(engine, file, *span).map(SendRt::Expr),
        });
        let expect = b
            .expect
            .iter()
            .filter_map(|s| self.expect_unit(engine, file, *s))
            .collect();
        let captures = b
            .captures
            .iter()
            .filter_map(|c| {
                self.expr_unit(engine, file, c.expr)
                    .map(|u| (c.name.clone(), u))
            })
            .collect();
        let undo = match b.undo.as_deref() {
            None => None,
            Some(Undo::Request(body)) => self
                .body(engine, file, body)
                .map(|ub| (UndoRt::Request(Rc::new(ub)), body.span)),
            Some(Undo::Call(span)) => self
                .expr_unit(engine, file, *span)
                .map(|u| (UndoRt::Call(u), *span)),
        };
        if self.diags.len() > errors {
            return None;
        }
        let method_template = b
            .connection
            .verb_template
            .as_ref()
            .map(|t| self.template(engine, file, t));
        if self.diags.len() > errors {
            return None;
        }
        Some(BodyRt {
            method: b.connection.verb.clone(),
            method_template,
            url,
            headers,
            send,
            expect,
            captures,
            undo,
            span: b.connection.span,
            timeout: None,
            poll: None,
        })
    }

    /// Emit a function plus its default-argument overloads and `__kw` variant into the library.
    fn gen_fn(&self, file: FileId, d: &FnDecl, lib: &mut CodeBuilder) -> PResult<()> {
        let r = self.rewriter(file);
        lib.set_anchor(d.name_span);
        let names: Vec<&str> = d.params.iter().map(|p| p.name.as_str()).collect();
        lib.push_gen("fn ");
        lib.push_orig(file, d.name_span.start, &d.name);
        lib.push_gen("(");
        for (i, p) in d.params.iter().enumerate() {
            if i > 0 {
                lib.push_gen(", ");
            }
            lib.push_orig(file, p.span.start, &p.name);
        }
        lib.push_gen(") {");
        r.rewrite_block(d.body.start + 1, d.body.end - 1, lib)?;
        lib.push_gen("}\n");
        let required = d.params.iter().take_while(|p| p.default.is_none()).count();
        for n in required..d.params.len() {
            lib.push_gen(&format!("fn {}({}) {{ ", d.name, names[..n].join(", ")));
            for p in &d.params[n..] {
                lib.push_gen(&format!("let {} = ", p.name));
                let def = p.default.unwrap();
                r.rewrite(def.start, def.end, lib)?;
                lib.push_gen("; ");
            }
            lib.push_gen(&format!("{}({}) }}\n", d.name, names.join(", ")));
        }
        // fn name__kw(m): named and mixed arguments.
        let allowed: Vec<String> = names
            .iter()
            .enumerate()
            .flat_map(|(i, n)| [format!("{n:?}"), format!("\"_{i}\"")])
            .collect();
        lib.push_gen(&format!(
            "fn {name}__kw(m) {{ for k in m.keys() {{ if !(k in [{allowed}]) {{ throw \"`{name}` has no parameter `\" + k + \"`\"; }} }} ",
            name = d.name,
            allowed = allowed.join(", ")
        ));
        for (i, p) in d.params.iter().enumerate() {
            lib.push_gen(&format!(
                "let {n} = if {n:?} in m {{ m[{n:?}] }} else if \"_{i}\" in m {{ m[\"_{i}\"] }} else {{ ",
                n = p.name
            ));
            match p.default {
                Some(def) => r.rewrite(def.start, def.end, lib)?,
                None => lib.push_gen(&format!(
                    "throw \"missing argument `{}` for `{}`\"",
                    p.name, d.name
                )),
            }
            lib.push_gen(" }; ");
        }
        lib.push_gen(&format!("{}({}) }}\n", d.name, names.join(", ")));
        Ok(())
    }

    fn test(
        &mut self,
        engine: &Engine,
        file: FileId,
        t: &syntax::TestDecl,
        fns: &HashMap<String, FnDecl>,
    ) -> Option<TestRt> {
        let errors = self.diags.len();
        let params = t
            .params
            .iter()
            .map(|p| TestParamRt {
                name: p.name.clone(),
                source: p.source.and_then(|s| self.expr_unit(engine, file, s)),
            })
            .collect();
        let cases = t.cases.and_then(|s| self.expr_unit(engine, file, s));
        let (modifiers, label) = if t.modifiers.is_empty() {
            (None, None)
        } else {
            self.modifier_unit(engine, file, t, fns)
        };
        let body = self.build(file, t.body, |r, out| {
            r.rewrite_block(t.body.start + 1, t.body.end - 1, out)
        })?;
        let body_unit = self.compile(engine, file, body);
        if self.diags.len() > errors {
            return None;
        }
        Some(TestRt {
            name: t.name.clone(),
            file,
            params,
            cases,
            modifiers,
            label,
            body: body_unit?,
        })
    }

    /// `rollback runs(200) as_user("admin")` → `t.__layer(0); t.rollback(); t.__layer(1); …`
    ///
    /// `label(...)` isn't part of the script: its argument is compiled on its own and evaluated
    /// per case, with the case's arguments in scope. Returns (script, label).
    fn modifier_unit(
        &mut self,
        engine: &Engine,
        file: FileId,
        t: &syntax::TestDecl,
        fns: &HashMap<String, FnDecl>,
    ) -> (Option<usize>, Option<usize>) {
        let src = self.src(file).to_string();
        let mut code = CodeBuilder::new(t.name_span);
        let mut label = None;
        let mut has_script = false;
        for (i, m) in t.modifiers.iter().enumerate() {
            let Some((ns, ne)) = ident_at(&src, m.start) else {
                self.diags.push(Diag::at(*m, "expected a modifier name"));
                continue;
            };
            let name = &src[ns..ne];
            let name_span = Span::new(file, ns, ne);
            if name == "label" {
                if label.is_some() {
                    self.diags.push(Diag::at(*m, "`label` is given twice"));
                    continue;
                }
                let usage = "`label` needs an expression, e.g. `label(name)` or ``label(`${role} on ${endpoint}`)``";
                if src.as_bytes().get(ne) != Some(&b'(') || m.end < ne + 2 {
                    self.diags.push(Diag::at(*m, usage));
                    continue;
                }
                let (s, e) = crate::syntax::scan::trim_range(&src, ne + 1, m.end - 1);
                if s == e {
                    self.diags.push(Diag::at(*m, usage));
                    continue;
                }
                label = self.expr_unit(engine, file, Span::new(file, s, e));
                continue;
            }
            has_script = true;
            let user_fn = fns.get(name);
            if user_fn.is_none() && !BUILTIN_MODIFIERS.contains(&name) {
                let candidates = BUILTIN_MODIFIERS
                    .iter()
                    .copied()
                    .chain(fns.keys().map(String::as_str));
                let mut d = Diag::at(name_span, format!("unknown modifier `{name}`"));
                d = match suggest(name, candidates) {
                    Some(s) => d.help(format!("did you mean `{s}`?")),
                    None => d.help("modifiers are built-ins (rollback, timeout, retries, runs, seed, tags, skip, only, label) or functions you define"),
                };
                self.diags.push(d);
                continue;
            }
            code.push_gen(&format!("t.__layer({i}); t."));
            code.push_orig(file, ns, name);
            let rest = skip_trivia(&src, ne).unwrap_or(ne);
            if rest >= m.end || src.as_bytes().get(ne) != Some(&b'(') {
                code.push_gen("();\n");
                continue;
            }
            let result = match user_fn {
                Some(decl) => self.static_args(file, &src, ne, m.end, decl, &mut code),
                None => self.rewriter(file).rewrite(ne, m.end, &mut code),
            };
            if let Err(d) = result {
                self.diags.push(d);
            }
            code.push_gen(";\n");
        }
        let script = if has_script {
            self.compile(engine, file, code)
        } else {
            None
        };
        (script, label)
    }

    /// Resolve named arguments of a user modifier call statically: `(role: "x")` → `("x")`.
    fn static_args(
        &self,
        file: FileId,
        src: &str,
        open: usize,
        end: usize,
        decl: &FnDecl,
        out: &mut CodeBuilder,
    ) -> PResult<()> {
        let close = end - 1;
        let r = self.rewriter(file);
        let args = split_top_level(src, open + 1, close)
            .map_err(|e| Diag::at(Span::new(file, e.pos, e.pos + 1), e.message))?;
        let mut slots: Vec<Option<(usize, usize)>> = vec![None; decl.params.len()];
        for (i, &(s, e)) in args.iter().enumerate() {
            let (s, e) = crate::syntax::scan::trim_range(src, s, e);
            if s == e {
                continue;
            }
            let named = ident_at(src, s).and_then(|(is, ie)| {
                let after = skip_trivia(src, ie).ok()?;
                (src.as_bytes().get(after) == Some(&b':')
                    && src.as_bytes().get(after + 1) != Some(&b':'))
                .then(|| {
                    (
                        &src[is..ie],
                        skip_trivia(src, after + 1).ok().unwrap_or(after + 1),
                    )
                })
            });
            let (index, value_start) = match named {
                Some((name, vs)) => match decl.params.iter().position(|p| p.name == name) {
                    Some(idx) => (idx, vs),
                    None => {
                        return Err(Diag::at(
                            Span::new(file, s, e),
                            format!("`{}` has no parameter `{name}`", decl.name),
                        ));
                    }
                },
                None => (i, s),
            };
            if index >= slots.len() {
                return Err(Diag::at(
                    Span::new(file, s, e),
                    format!("too many arguments for `{}`", decl.name),
                ));
            }
            slots[index] = Some((value_start, e));
        }
        out.push_gen("(");
        for (i, (slot, p)) in slots.iter().zip(&decl.params).enumerate() {
            if i > 0 {
                out.push_gen(", ");
            }
            match (slot, p.default) {
                (Some((s, e)), _) => r.rewrite(*s, *e, out)?,
                (None, Some(def)) => self.rewriter(def.file).rewrite(def.start, def.end, out)?,
                (None, None) => {
                    return Err(Diag::at(
                        Span::new(file, open, end),
                        format!("missing argument `{}` for `{}`", p.name, decl.name),
                    ));
                }
            }
        }
        out.push_gen(")");
        Ok(())
    }
}
