mod diag;
mod load;
mod report;
mod rewrite;
mod runtime;
mod source;
mod style;
mod syntax;
mod testing;

use clap::{Parser, Subcommand, ValueEnum};
use load::{LoadError, LoadOptions, Program};
use report::{Reporter, render_failure};
use rhai::{Dynamic, Map, Scope};
use runtime::Config;
use runtime::value::{self, display};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use style::{bold, cyan, dim, green, red, yellow};

#[derive(Parser)]
#[command(
    name = "klausur",
    version,
    about = "Send API requests and run API tests"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run one request or function and show the result.
    Run {
        /// `file.kl` or `file.kl#name`.
        target: String,
        /// Arguments as `name=value` (JSON values are parsed, anything else is a string).
        #[arg(short, long = "arg", value_name = "NAME=VALUE")]
        args: Vec<String>,
        /// Load `.env.<NAME>` on top of `.env`.
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// Don't print the request trace.
        #[arg(short, long)]
        quiet: bool,
        /// Per-request timeout in seconds.
        #[arg(long, value_name = "SECS", default_value_t = 30)]
        timeout: u64,
    },
    /// Run tests. Arguments that are existing paths select files; others filter test names.
    Test {
        #[arg(value_name = "PATH_OR_FILTER")]
        targets: Vec<String>,
        /// Seed for generated values; failures print the seed that reproduces them.
        #[arg(long)]
        seed: Option<u64>,
        /// Only run cases whose argument `NAME` displays as `VALUE`, or whose `label(...)` is
        /// exactly `LABEL`.
        #[arg(long = "case", value_name = "NAME=VALUE|LABEL")]
        cases: Vec<String>,
        /// Only run tests with this tag.
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// What to do after a test leaves the environment dirty.
        #[arg(long, value_enum, default_value_t = OnDirty::Abort)]
        on_dirty: OnDirty,
        #[arg(long, value_name = "NAME")]
        env: Option<String>,
        /// Show logs of passing tests and every request.
        #[arg(short, long)]
        verbose: bool,
        /// Per-request timeout in seconds.
        #[arg(long, value_name = "SECS", default_value_t = 30)]
        timeout: u64,
    },
    /// List requests, functions and tests.
    List {
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
    },
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
enum OnDirty {
    Abort,
    Continue,
}

fn main() -> ExitCode {
    style::init();
    let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
    let _ = miette::set_hook(Box::new(move |_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .color(color)
                .unicode(color)
                .build(),
        )
    }));
    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = interrupted.clone();
    let _ = ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            eprintln!("\n{}", red("interrupted again: exiting without cleanup"));
            std::process::exit(130);
        }
        eprintln!(
            "\n{}",
            yellow("interrupting: finishing cleanup (press Ctrl-C again to exit immediately)")
        );
    });
    let cli = Cli::parse();
    let code = match cli.command {
        Command::Run {
            target,
            args,
            env,
            quiet,
            timeout,
        } => run(&target, &args, env, quiet, timeout, interrupted),
        Command::Test {
            targets,
            seed,
            cases,
            tags,
            on_dirty,
            env,
            verbose,
            timeout,
        } => test(
            targets,
            seed,
            cases,
            tags,
            on_dirty,
            env,
            verbose,
            timeout,
            interrupted,
        ),
        Command::List { paths } => list(&paths),
    };
    ExitCode::from(code as u8)
}

fn load_or_report(paths: &[PathBuf], opts: LoadOptions) -> Result<Program, i32> {
    load::load(paths, opts).map_err(|LoadError { sources, diags }| {
        for d in &diags {
            eprint!("{}", d.render(&sources));
        }
        eprintln!(
            "{}",
            red(format!(
                "{} error{} while loading",
                diags.len(),
                if diags.len() == 1 { "" } else { "s" }
            ))
        );
        2
    })
}

fn seed_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
        % 1_000_000_000
}

fn parse_kv(s: &str) -> Result<(String, String), i32> {
    match s.split_once('=') {
        Some((k, v)) => Ok((k.trim().to_string(), v.to_string())),
        None => {
            eprintln!("{} expected NAME=VALUE, got {s:?}", red("error:"));
            Err(2)
        }
    }
}

/// CLI values: JSON if it parses, a Rhai map literal (`#{…}`), otherwise a string.
fn parse_value(program: &Program, s: &str) -> Dynamic {
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(s) {
        return value::from_json(&json);
    }
    if s.trim_start().starts_with("#{")
        && let Ok(v) = program.engine.eval_expression::<Dynamic>(s)
    {
        return v;
    }
    s.into()
}

fn run(
    target: &str,
    raw_args: &[String],
    env: Option<String>,
    quiet: bool,
    timeout: u64,
    interrupted: Arc<AtomicBool>,
) -> i32 {
    let (path, name) = match target.rsplit_once('#') {
        Some((p, n)) => (p, Some(n)),
        None => (target, None),
    };
    let config = Config {
        trace: !quiet,
        request_timeout: std::time::Duration::from_secs(timeout),
        ..Config::default()
    };
    let opts = LoadOptions {
        config,
        env_name: env,
        interrupted,
        seed: seed_now(),
    };
    let program = match load_or_report(&[PathBuf::from(path)], opts) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let root = program.roots.first().copied();
    let in_root: Vec<_> = program
        .requests
        .iter()
        .filter(|r| Some(r.span.file) == root)
        .collect();
    let name = match name {
        Some(n) => n.to_string(),
        None if in_root.len() == 1 => in_root[0].name.clone(),
        None => {
            eprintln!(
                "{} pick a request with {}",
                red("error:"),
                bold(format!("{path}#<name>"))
            );
            print_listing(&program, root);
            return 2;
        }
    };
    // Which parameters does the target take?
    let params: Vec<(String, bool)> =
        if let Some(r) = program.requests.iter().find(|r| r.name == name) {
            r.params
                .iter()
                .map(|p| (p.name.clone(), p.default.is_some()))
                .collect()
        } else if let Some(f) = program.fns.iter().find(|f| f.name == name) {
            f.params
                .iter()
                .map(|(n, d)| (n.clone(), d.is_some()))
                .collect()
        } else {
            let names = program
                .requests
                .iter()
                .map(|r| r.name.as_str())
                .chain(program.fns.iter().map(|f| f.name.as_str()));
            let mut msg = format!("{} no request or function named `{name}`", red("error:"));
            if let Some(s) = diag::suggest(&name, names) {
                msg.push_str(&format!(" (did you mean `{s}`?)"));
            }
            eprintln!("{msg}");
            return 2;
        };
    let mut args = Map::new();
    for raw in raw_args {
        let (k, v) = match parse_kv(raw) {
            Ok(kv) => kv,
            Err(code) => return code,
        };
        args.insert(k.as_str().into(), parse_value(&program, &v));
    }
    for (p, has_default) in &params {
        if args.contains_key(p.as_str()) || *has_default {
            continue;
        }
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "{} missing argument `{p}` (pass it with --arg {p}=…)",
                red("error:")
            );
            return 2;
        }
        eprint!("{} ", cyan(format!("{p}:")));
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return 2;
        }
        args.insert(
            p.as_str().into(),
            parse_value(&program, line.trim_end_matches(['\n', '\r'])),
        );
    }
    let shared = &program.shared;
    let mut scope = Scope::new();
    scope.push("__args", args);
    let result = program
        .engine
        .compile(format!("{name}__kw(__args)"))
        .map_err(|e| e.to_string())
        .and_then(|ast| {
            program
                .engine
                .eval_ast_with_scope::<Dynamic>(&mut scope, &ast)
                .map_err(|e| render_failure(&shared.to_failure(&e, None), &shared.sources))
        });
    let mut code = 0;
    match result {
        Ok(v) => print_result(&v),
        Err(rendered) => {
            eprint!("{rendered}");
            code = 1;
        }
    }
    let state = shared.state.borrow();
    for skipped in &state.skipped_undos {
        eprintln!(
            "{} {}",
            dim("hint:"),
            dim(format!(
                "the `undo` of `{skipped}` wasn't registered because no scope was open; wrap the call in scoped(...) or transaction(...) to clean up automatically"
            ))
        );
    }
    if !state.orphan_dirty.is_empty() {
        let entries: Vec<_> = state
            .orphan_dirty
            .iter()
            .map(|d| (String::new(), 0, d))
            .collect();
        eprint!("{}", report::render_dirty(&entries, &shared.sources));
        code = 3;
    }
    code
}

/// Request results: status and headers to stderr, the body to stdout. Other values: printed as JSON.
fn print_result(v: &Dynamic) {
    let Some(map) = v
        .clone()
        .try_cast::<Map>()
        .filter(|m| m.contains_key("status") && m.contains_key("request"))
    else {
        if !v.is_unit() {
            println!("{}", pretty_value(v));
        }
        return;
    };
    let status = map["status"].as_int().unwrap_or(0) as u16;
    let line = format!("{status} {}", runtime::request::reason(status));
    let ms = map
        .get("duration")
        .and_then(|d| d.as_int().ok())
        .unwrap_or(0);
    eprintln!(
        "{} {}",
        if status < 400 {
            green(bold(line))
        } else {
            red(bold(line))
        },
        dim(format!("({ms} ms)"))
    );
    if let Some(headers) = map.get("headers").and_then(|h| h.clone().try_cast::<Map>()) {
        for (k, v) in headers {
            eprintln!(
                "{}",
                dim(format!("{k}: {}", report::redact(&k, &v.to_string())))
            );
        }
    }
    let captures: Vec<_> = map
        .iter()
        .filter(|(k, _)| {
            !["status", "headers", "body", "raw", "duration", "request"].contains(&k.as_str())
        })
        .collect();
    for (k, v) in captures {
        eprintln!("{} {}", cyan(format!("let {k} =")), display(v));
    }
    let body = &map["body"];
    if !body.is_unit() {
        if std::io::stderr().is_terminal() {
            eprintln!();
        }
        println!("{}", pretty_value(body));
    }
}

fn pretty_value(v: &Dynamic) -> String {
    if v.is_string() {
        return v.to_string();
    }
    value::to_json(v)
        .ok()
        .and_then(|j| serde_json::to_string_pretty(&j).ok())
        .unwrap_or_else(|| display(v))
}

#[allow(clippy::too_many_arguments)]
fn test(
    targets: Vec<String>,
    seed: Option<u64>,
    raw_cases: Vec<String>,
    tags: Vec<String>,
    on_dirty: OnDirty,
    env: Option<String>,
    verbose: bool,
    timeout: u64,
    interrupted: Arc<AtomicBool>,
) -> i32 {
    let (paths, filters): (Vec<String>, Vec<String>) =
        targets.into_iter().partition(|t| PathBuf::from(t).exists());
    let paths: Vec<PathBuf> = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.into_iter().map(PathBuf::from).collect()
    };
    // `--case NAME=VALUE` matches an argument; any `--case` also matches a `label(...)` exactly.
    let case_filters: Vec<(String, Option<(String, String)>)> = raw_cases
        .iter()
        .map(|raw| {
            (
                raw.clone(),
                raw.split_once('=')
                    .map(|(k, v)| (k.trim().to_string(), v.to_string())),
            )
        })
        .collect();
    let seed = seed.unwrap_or_else(seed_now);
    let config = Config {
        trace: verbose,
        request_timeout: std::time::Duration::from_secs(timeout),
        ..Config::default()
    };
    let opts = LoadOptions {
        config,
        env_name: env,
        interrupted: interrupted.clone(),
        seed,
    };
    let program = match load_or_report(&paths, opts) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let shared = &program.shared;
    let engine = &program.engine;
    let mut reporter = Reporter::new(&shared.sources, verbose, seed);
    let started = Instant::now();

    // Build definitions first: `only` on any test narrows the run.
    let selected: Vec<_> = program
        .tests
        .iter()
        .filter(|t| {
            filters.is_empty()
                || filters
                    .iter()
                    .any(|f| t.name.to_lowercase().contains(&f.to_lowercase()))
        })
        .map(|t| (t.clone(), testing::build_def(shared, engine, t)))
        .collect();
    let any_only = selected
        .iter()
        .any(|(_, d)| d.as_ref().is_ok_and(|d| d.0.borrow().only));
    if program.tests.is_empty() {
        println!("{}", yellow("no tests found"));
        return 0;
    }
    if selected.is_empty() {
        println!("{}", yellow("no tests match the filter"));
        return 0;
    }
    'tests: for (test, def) in selected {
        let file = shared.sources.get(test.file).name.clone();
        if interrupted.load(Ordering::SeqCst) {
            reporter.summary.interrupted = true;
            break;
        }
        let def = match def {
            Ok(d) => d,
            Err(f) => {
                reporter.test_error(&file, &test.name, f);
                continue;
            }
        };
        let def = def.0.borrow();
        if any_only && !def.only {
            continue;
        }
        if !tags.is_empty() && !def.tags.iter().any(|t| tags.contains(t)) {
            continue;
        }
        if let Some(reason) = &def.skip {
            reporter.skipped(&file, &test.name, reason);
            continue;
        }
        let cases = match testing::expand_cases(shared, engine, &test, &def, seed) {
            Ok(c) => c,
            Err(f) => {
                reporter.test_error(&file, &test.name, f);
                continue;
            }
        };
        let total = cases.len();
        let mut ran = 0;
        for case in cases {
            let matches = case_filters.iter().all(|(raw, kv)| {
                case.label.as_deref() == Some(raw.as_str())
                    || kv.as_ref().is_some_and(|(k, v)| {
                        case.args
                            .iter()
                            .any(|(name, val)| name == k && value::display_plain(val) == *v)
                    })
            });
            if !matches {
                continue;
            }
            if interrupted.load(Ordering::SeqCst) {
                reporter.summary.interrupted = true;
                break 'tests;
            }
            ran += 1;
            let result = testing::run_case(shared, engine, &test, &def, case);
            let dirty = !result.dirty.is_empty();
            reporter.case(&file, result);
            if dirty && on_dirty == OnDirty::Abort {
                reporter.summary.stopped_dirty = true;
                break 'tests;
            }
        }
        if ran == 0 && total > 0 {
            reporter.skipped(&file, &test.name, "no case matches --case");
        }
    }
    if interrupted.load(Ordering::SeqCst) {
        reporter.summary.interrupted = true;
    }
    reporter.finish(started.elapsed());
    reporter.summary.exit_code()
}

fn print_listing(program: &Program, only_file: Option<usize>) {
    let shared = &program.shared;
    for (file_id, file) in shared.sources.files.iter().enumerate() {
        if only_file.is_some_and(|f| f != file_id) {
            continue;
        }
        let requests: Vec<_> = program
            .requests
            .iter()
            .filter(|r| r.span.file == file_id)
            .collect();
        let fns: Vec<_> = program.fns.iter().filter(|f| f.file == file_id).collect();
        let tests: Vec<_> = program.tests.iter().filter(|t| t.file == file_id).collect();
        if requests.is_empty() && fns.is_empty() && tests.is_empty() {
            continue;
        }
        println!("{}", bold(&file.name));
        let sig = |params: Vec<(String, Option<String>)>| {
            params
                .into_iter()
                .map(|(n, d)| match d {
                    Some(d) => format!("{n} = {d}"),
                    None => n,
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        for r in requests {
            let params = r
                .params
                .iter()
                .map(|p| {
                    let d = p.default.map(|u| {
                        let unit = shared.unit(u);
                        unit.code.text.clone()
                    });
                    (p.name.clone(), d)
                })
                .collect();
            println!(
                "  {}  {}",
                cyan(format!("@{}({})", r.name, sig(params))),
                dim(&r.summary)
            );
        }
        for f in fns {
            println!("  fn {}({})", f.name, sig(f.params.clone()));
        }
        for t in tests {
            let params: Vec<&str> = t.params.iter().map(|p| p.name.as_str()).collect();
            let params = if params.is_empty() {
                String::new()
            } else {
                format!(" ({})", params.join(", "))
            };
            println!("  {}", green(format!("test {:?}{params}", t.name)));
        }
    }
}

fn list(paths: &[PathBuf]) -> i32 {
    let opts = LoadOptions {
        config: Config::default(),
        env_name: None,
        interrupted: Arc::default(),
        seed: 0,
    };
    match load_or_report(paths, opts) {
        Ok(program) => {
            print_listing(&program, None);
            0
        }
        Err(code) => code,
    }
}
