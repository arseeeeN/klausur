mod diag;
mod init;
mod load;
mod parallel;
mod report;
mod rewrite;
mod runtime;
mod source;
mod style;
mod syntax;
mod testing;
mod update;

use clap::{Parser, Subcommand, ValueEnum};
use load::{LoadError, LoadOptions, Program};
use report::{Reporter, render_failure};
use rhai::{Dynamic, Map, Scope};
use runtime::Config;
use runtime::value::{self, display};
use source::FileId;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
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
        /// Show the browser window for pages the code opens.
        #[arg(long)]
        headed: bool,
    },
    /// Run tests. Arguments that are existing paths select files; others filter test names.
    Test(TestArgs),
    /// List requests, functions and tests.
    List {
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
    },
    /// Replace this binary with the latest release, checked against its SHA256SUMS.
    Update,
    /// Create a test suite to start from: requests, helpers, an example test and a README.
    /// Files that already exist are kept.
    Init {
        /// Where to put it.
        #[arg(default_value = "src/test/klausur")]
        dir: PathBuf,
        /// Generate a working login modifier for basic auth or a bearer token.
        #[arg(long, value_enum)]
        auth: Option<init::Auth>,
    },
}

#[derive(clap::Args, Clone)]
struct TestArgs {
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
    /// Leave out tests with this tag, e.g. `--skip-tag ui` without a browser.
    #[arg(long = "skip-tag", value_name = "TAG")]
    skip_tags: Vec<String>,
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
    /// How many tests marked `parallel` run at once. Other tests always run alone.
    #[arg(short, long, value_name = "N", default_value_t = 4)]
    jobs: usize,
    /// Show the browser window for pages the tests open.
    #[arg(long)]
    headed: bool,
    /// Stop scheduling tests after the first failure.
    #[arg(long)]
    fail_fast: bool,
    /// Also write the results as a JUnit XML report (for GitLab, Jenkins, …).
    #[arg(long, value_name = "FILE")]
    junit: Option<PathBuf>,
    /// Run again whenever a `.kl` file (or anything else in the test folders) changes.
    #[arg(short, long)]
    watch: bool,
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
            runtime::browser::shutdown();
            std::process::exit(130);
        }
        eprintln!(
            "\n{}",
            yellow("interrupting: finishing cleanup (press Ctrl-C again to exit immediately)")
        );
    });
    let cli = Cli::parse();
    // `test` and `run` mention a newer release at the end; the check never delays them.
    let check = match cli.command {
        Command::Run { .. } | Command::Test(_) => update::start_check(),
        _ => update::Check::Off,
    };
    let code = match cli.command {
        Command::Run {
            target,
            args,
            env,
            quiet,
            timeout,
            headed,
        } => run(&target, &args, env, quiet, timeout, headed, interrupted),
        Command::Test(args) if args.watch => watch(&args, interrupted),
        Command::Test(args) => test(&args, interrupted),
        Command::List { paths } => list(&paths),
        Command::Update => update::update(),
        Command::Init { dir, auth } => init::init(&dir, auth),
    };
    runtime::browser::shutdown();
    update::finish_check(check);
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
    headed: bool,
    interrupted: Arc<AtomicBool>,
) -> i32 {
    let (path, name) = match target.rsplit_once('#') {
        Some((p, n)) => (p, Some(n)),
        None => (target, None),
    };
    let config = Config {
        trace: !quiet,
        request_timeout: std::time::Duration::from_secs(timeout),
        headed,
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
            .map(|d| (String::new(), 0, runtime::undo::DirtyReport::from(d)))
            .collect();
        let entries: Vec<_> = entries.iter().map(|(c, s, d)| (c.clone(), *s, d)).collect();
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

fn test(args: &TestArgs, interrupted: Arc<AtomicBool>) -> i32 {
    let TestArgs {
        seed,
        cases: raw_cases,
        tags,
        on_dirty,
        env,
        verbose,
        timeout,
        headed,
        ..
    } = args.clone();
    let jobs = args.jobs.max(1);
    let (paths, filters) = targets(args);
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
        headed,
        ..Config::default()
    };
    let opts = LoadOptions {
        config,
        env_name: env,
        interrupted: interrupted.clone(),
        seed,
    };
    let program = match load_or_report(&paths, opts.clone()) {
        Ok(p) => p,
        Err(code) => return code,
    };
    // Like Playwright's `test-results/`: only this run's failure screenshots.
    runtime::browser::clear_results();
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
    // Plan the run in declaration order. Cases of `parallel` tests go to worker threads;
    // every other case waits until no worker is busy and runs alone on this thread.
    let mut steps: Vec<Step> = Vec::new();
    for (test, def) in selected {
        let file = shared.sources.get(test.file).name.clone();
        let test_idx = program
            .tests
            .iter()
            .position(|t| Rc::ptr_eq(t, &test))
            .unwrap_or(0);
        let def = match def {
            Ok(d) => d,
            Err(f) => {
                steps.push(Step::Error(file, test.name.clone(), f));
                continue;
            }
        };
        let test_def = def.clone();
        let def = def.0.borrow();
        if any_only && !def.only {
            continue;
        }
        if !tags.is_empty() && !def.tags.iter().any(|t| tags.contains(t)) {
            continue;
        }
        if def.tags.iter().any(|t| args.skip_tags.contains(t)) {
            continue;
        }
        if let Some(reason) = &def.skip {
            steps.push(Step::Skipped(file, test.name.clone(), reason.clone()));
            continue;
        }
        let cases = match testing::expand_cases(shared, engine, &test, &def, seed) {
            Ok(c) => c,
            Err(f) => {
                steps.push(Step::Error(file, test.name.clone(), f));
                continue;
            }
        };
        let total = cases.len();
        let mut ran = 0;
        for (case_idx, case) in cases.into_iter().enumerate() {
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
            ran += 1;
            steps.push(Step::Case {
                file: file.clone(),
                test: test.clone(),
                test_idx,
                case_idx,
                parallel: def.parallel && jobs > 1,
                group: def.group.clone(),
                def: test_def.clone(),
                spec: Some(case),
            });
        }
        if ran == 0 && total > 0 {
            steps.push(Step::Skipped(
                file,
                test.name.clone(),
                "no case matches --case".into(),
            ));
        }
    }

    // Files with `setup`/`teardown`: how many of their cases will run, and the last one.
    let mut fixtures: HashMap<FileId, FileFixture> = HashMap::new();
    for (seq, step) in steps.iter().enumerate() {
        if let Step::Case { test, .. } = step
            && program.setups.contains_key(&test.file)
        {
            let f = fixtures.entry(test.file).or_default();
            f.pending += 1;
            f.last_seq = seq;
        }
    }
    let pool = steps
        .iter()
        .any(|s| matches!(s, Step::Case { parallel: true, .. }))
        .then(|| parallel::Pool::start(jobs, &paths, opts.clone()));
    let mut run = Schedule {
        program: &program,
        seed,
        fixtures,
        pool,
        reporter: &mut reporter,
        stop_on_dirty: on_dirty == OnDirty::Abort,
        fail_fast: args.fail_fast,
        done: BTreeMap::new(),
        files: HashMap::new(),
        groups: HashMap::new(),
        running: HashMap::new(),
        next: 0,
        in_flight: 0,
        stopped: false,
        failed: false,
    };
    for (seq, step) in steps.into_iter().enumerate() {
        if run.stopped || interrupted.load(Ordering::SeqCst) {
            break;
        }
        match step {
            Step::Skipped(file, name, reason) => run.finish(seq, Done::Skipped(file, name, reason)),
            Step::Error(file, name, f) => run.finish(seq, Done::Error(file, name, f)),
            Step::Case {
                file,
                test,
                test_idx,
                case_idx,
                parallel: true,
                group,
                ..
            } => {
                while run.in_flight >= jobs || run.group_full(&group) {
                    run.receive();
                }
                if run.stopped || interrupted.load(Ordering::SeqCst) {
                    break;
                }
                if !run.set_up(seq, &file, &test) {
                    continue;
                }
                let values = run.values(test.file);
                run.send(seq, file, test.file, test_idx, case_idx, group, values);
            }
            Step::Case {
                file,
                test,
                def,
                spec,
                ..
            } => {
                while run.in_flight > 0 {
                    run.receive();
                }
                if run.stopped || interrupted.load(Ordering::SeqCst) {
                    break;
                }
                if !run.set_up(seq, &file, &test) {
                    continue;
                }
                let spec = spec.expect("serial cases keep their spec");
                let result = testing::run_case(shared, engine, &test, &def.0.borrow(), spec);
                run.case_done(seq, file, test.file, result);
            }
        }
    }
    while run.in_flight > 0 {
        run.receive();
    }
    run.end();
    if interrupted.load(Ordering::SeqCst) {
        reporter.summary.interrupted = true;
    }
    reporter.finish(started.elapsed());
    if let Some(path) = &args.junit
        && let Err(e) = std::fs::write(path, reporter.junit())
    {
        eprintln!("{}", red(format!("couldn't write {}: {e}", path.display())));
    }
    reporter.summary.exit_code()
}

/// The paths (existing files and directories; the current directory if none) and the
/// name filters among `klausur test`'s arguments.
fn targets(args: &TestArgs) -> (Vec<PathBuf>, Vec<String>) {
    let (paths, filters): (Vec<String>, Vec<String>) = args
        .targets
        .iter()
        .cloned()
        .partition(|t| PathBuf::from(t).exists());
    let paths = if paths.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        paths.into_iter().map(PathBuf::from).collect()
    };
    (paths, filters)
}

/// One planned entry of a test run.
enum Step {
    Skipped(String, String, String),
    Error(String, String, runtime::Failure),
    Case {
        file: String,
        test: Rc<testing::TestRt>,
        test_idx: usize,
        case_idx: usize,
        parallel: bool,
        /// `parallel("llm", 1)`: the group and how many of its cases may run at once.
        group: Option<(String, usize)>,
        def: testing::TestDef,
        /// The case itself, for running it on this thread.
        spec: Option<testing::CaseSpec>,
    },
}

/// A finished step, waiting for the steps before it to be printed.
enum Done {
    Skipped(String, String, String),
    Error(String, String, runtime::Failure),
    Case(String, testing::CaseResult),
    /// A file's `setup` or `teardown`.
    Fixture(String, testing::CaseResult),
    /// A case that didn't run because its file's setup failed (its test is reported once).
    Silent,
}

fn report_done(reporter: &mut Reporter, d: Done) {
    match d {
        Done::Skipped(file, name, reason) => reporter.skipped(&file, &name, &reason),
        Done::Error(file, name, f) => reporter.test_error(&file, &name, f),
        Done::Case(file, r) => reporter.case(&file, r),
        Done::Fixture(file, r) => reporter.fixture(&file, r),
        Done::Silent => {}
    }
}

/// The `setup`/`teardown` state of one file in a run.
#[derive(Default)]
struct FileFixture {
    /// Cases of the file that haven't finished yet.
    pending: usize,
    /// The file's last case: the teardown is reported after it.
    last_seq: usize,
    started: bool,
    torn_down: bool,
    failed: bool,
    /// Tests already reported as not run because the setup failed.
    not_run: HashSet<String>,
    fixture: testing::fixture::Fixture,
    values: Arc<Vec<(String, serde_json::Value)>>,
}

/// Runs planned steps: `parallel` cases on the worker pool, everything else on this thread,
/// and prints results in declaration order.
struct Schedule<'r, 'a> {
    program: &'r load::Program,
    seed: u64,
    fixtures: HashMap<FileId, FileFixture>,
    pool: Option<parallel::Pool>,
    reporter: &'r mut Reporter<'a>,
    stop_on_dirty: bool,
    /// `--fail-fast`: stop scheduling after the first failure.
    fail_fast: bool,
    /// Finished steps that wait for earlier ones before they're printed; a step can come with
    /// its file's setup or teardown.
    done: BTreeMap<usize, Vec<Done>>,
    /// The file of each case on the pool.
    files: HashMap<usize, (String, FileId)>,
    /// The group of each case on the pool, and how many of each group are running.
    groups: HashMap<usize, String>,
    running: HashMap<String, usize>,
    next: usize,
    in_flight: usize,
    stopped: bool,
    /// Stopped by `--fail-fast` (rather than by a dirty case).
    failed: bool,
}

impl Schedule<'_, '_> {
    #[allow(clippy::too_many_arguments)]
    fn send(
        &mut self,
        seq: usize,
        file: String,
        file_id: FileId,
        test: usize,
        case: usize,
        group: Option<(String, usize)>,
        values: Arc<Vec<(String, serde_json::Value)>>,
    ) {
        if let Some(pool) = &self.pool {
            self.files.insert(seq, (file, file_id));
            if let Some((name, _)) = group {
                *self.running.entry(name.clone()).or_default() += 1;
                self.groups.insert(seq, name);
            }
            pool.send(parallel::Job {
                seq,
                test,
                case,
                values,
            });
            self.in_flight += 1;
        }
    }

    /// Before a case: run its file's setup if that hasn't happened yet. Returns whether the
    /// case may run; if the setup failed, the case is reported as not run.
    fn set_up(&mut self, seq: usize, file: &str, test: &testing::TestRt) -> bool {
        let Some(f) = self.fixtures.get_mut(&test.file) else {
            return true;
        };
        if !f.started {
            f.started = true;
            // Earlier files' cases may still run on the pool, and their fixtures could clash with
            // this one (two files that both write the same config): let them finish first.
            while self.in_flight > 0 {
                self.receive();
            }
            let f = self.fixtures.get_mut(&test.file).expect("checked above");
            let steps = &self.program.setups[&test.file];
            let (result, fixture) = testing::fixture::setup(
                &self.program.shared,
                &self.program.engine,
                test.file,
                steps,
                self.seed,
            );
            f.failed = result.outcome.is_err();
            f.values = Arc::new(fixture.values.clone());
            f.fixture = fixture;
            let failed = f.failed;
            self.record(seq, Done::Fixture(file.to_string(), result));
            if failed {
                // Clean up what the setup did before it failed.
                self.tear_down(test.file, file, Some(seq));
            }
        }
        let f = self.fixtures.get_mut(&test.file).expect("checked above");
        if !f.failed {
            return true;
        }
        let d = if f.not_run.insert(test.name.clone()) {
            Done::Skipped(file.to_string(), test.name.clone(), "setup failed".into())
        } else {
            Done::Silent
        };
        self.case_finished(seq, file, test.file, d);
        false
    }

    fn values(&self, file: FileId) -> Arc<Vec<(String, serde_json::Value)>> {
        self.fixtures
            .get(&file)
            .map(|f| f.values.clone())
            .unwrap_or_default()
    }

    /// A case finished: record it, and run its file's teardown after the file's last case.
    fn case_done(&mut self, seq: usize, file: String, file_id: FileId, r: testing::CaseResult) {
        let name = file.clone();
        self.case_finished(seq, &name, file_id, Done::Case(file, r));
    }

    fn case_finished(&mut self, seq: usize, file: &str, file_id: FileId, d: Done) {
        self.record(seq, d);
        let teardown = match self.fixtures.get_mut(&file_id) {
            Some(f) => {
                f.pending = f.pending.saturating_sub(1);
                f.pending == 0
            }
            None => false,
        };
        if teardown {
            self.tear_down(file_id, file, None);
        }
        self.flush();
    }

    /// Run a file's teardown, reported after the step `at` (by default the file's last case).
    fn tear_down(&mut self, file_id: FileId, file: &str, at: Option<usize>) {
        let Some(f) = self.fixtures.get_mut(&file_id) else {
            return;
        };
        if !f.started || f.torn_down {
            return;
        }
        f.torn_down = true;
        let actions = std::mem::take(&mut f.fixture.actions);
        let last = at.unwrap_or(f.last_seq);
        if actions.is_empty() {
            return;
        }
        let result = testing::fixture::teardown(
            &self.program.shared,
            &self.program.engine,
            actions,
            self.seed,
        );
        self.record(last, Done::Fixture(file.to_string(), result));
    }

    /// Whether the group already runs as many cases as it may.
    fn group_full(&self, group: &Option<(String, usize)>) -> bool {
        group
            .as_ref()
            .is_some_and(|(name, limit)| self.running.get(name).copied().unwrap_or(0) >= *limit)
    }

    /// Wait for one case on the pool to finish.
    fn receive(&mut self) {
        let Some((seq, result)) = self.pool.as_ref().and_then(|p| p.recv()) else {
            self.in_flight = 0;
            return;
        };
        self.in_flight -= 1;
        if let Some(name) = self.groups.remove(&seq)
            && let Some(n) = self.running.get_mut(&name)
        {
            *n = n.saturating_sub(1);
        }
        let (file, file_id) = self.files.remove(&seq).unwrap_or_default();
        self.case_done(seq, file, file_id, result);
    }

    /// Record a finished step and print whatever is ready. A dirty case stops the run unless
    /// `--on-dirty=continue`; cases already on the pool still finish.
    fn finish(&mut self, seq: usize, d: Done) {
        self.record(seq, d);
        self.flush();
    }

    fn record(&mut self, seq: usize, d: Done) {
        if let Done::Case(_, r) | Done::Fixture(_, r) = &d
            && !r.dirty.is_empty()
            && self.stop_on_dirty
        {
            self.stopped = true;
        }
        let failed = match &d {
            Done::Case(_, r) | Done::Fixture(_, r) => r.outcome.is_err(),
            Done::Error(..) => true,
            Done::Skipped(..) | Done::Silent => false,
        };
        if failed && self.fail_fast && !self.stopped {
            self.stopped = true;
            self.failed = true;
        }
        self.done.entry(seq).or_default().push(d);
    }

    /// Print the finished steps that are next in order. A step is complete once its case (or
    /// what replaces it) is there; a setup recorded ahead of it waits.
    fn flush(&mut self) {
        while self
            .done
            .get(&self.next)
            .is_some_and(|ds| ds.iter().any(|d| !matches!(d, Done::Fixture(..))))
        {
            for d in self.done.remove(&self.next).unwrap_or_default() {
                report_done(self.reporter, d);
            }
            self.next += 1;
        }
    }

    /// Print what finished after a gap left by steps that never ran.
    fn end(mut self) {
        self.pool = None;
        // Files stopped before their last case still get their teardown.
        let open: Vec<FileId> = self
            .fixtures
            .iter()
            .filter(|(_, f)| f.started && !f.torn_down)
            .map(|(id, _)| *id)
            .collect();
        for id in open {
            let name = self.program.shared.sources.get(id).name.clone();
            self.tear_down(id, &name, None);
        }
        for (_, ds) in std::mem::take(&mut self.done) {
            for d in ds {
                report_done(self.reporter, d);
            }
        }
        if self.stopped && self.failed {
            self.reporter.summary.stopped_failed = true;
        } else if self.stopped {
            self.reporter.summary.stopped_dirty = true;
        }
    }
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

/// `klausur test --watch`: run, then run again whenever something in the test folders changes,
/// until Ctrl-C.
fn watch(args: &TestArgs, interrupted: Arc<AtomicBool>) -> i32 {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |e: notify::Result<notify::Event>| {
        if let Ok(e) = e
            && !matches!(e.kind, notify::EventKind::Access(_))
            && e.paths.iter().any(|p| relevant_change(p))
        {
            let _ = tx.send(());
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{}", red(format!("can't watch for changes: {e}")));
            return 2;
        }
    };
    // A file argument is watched through its folder: editors often save by replacing the file.
    let (paths, _) = targets(args);
    for path in &paths {
        let dir = if path.is_file() {
            path.parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        } else {
            path.clone()
        };
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        if let Err(e) = watcher.watch(&dir, RecursiveMode::Recursive) {
            eprintln!("{}", red(format!("can't watch {}: {e}", dir.display())));
            return 2;
        }
    }
    loop {
        let code = test(args, interrupted.clone());
        if interrupted.load(Ordering::SeqCst) {
            return code;
        }
        // Files the run wrote itself (fixtures, screenshots, undo) don't count as changes.
        while rx.try_recv().is_ok() {}
        println!("{}", dim("watching for changes (Ctrl-C to stop) …"));
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(()) => break,
                Err(RecvTimeoutError::Timeout) if interrupted.load(Ordering::SeqCst) => {
                    return code;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return code,
            }
        }
        // One save often touches several files; wait for the burst to end.
        std::thread::sleep(Duration::from_millis(150));
        while rx.try_recv().is_ok() {}
        println!("\n{}\n", dim("── change detected, running again ──"));
    }
}

/// Changes worth a rerun: not klausur's own results, hidden files, or editor swap files.
fn relevant_change(path: &std::path::Path) -> bool {
    if path
        .components()
        .any(|c| c.as_os_str() == runtime::browser::RESULTS_DIR)
    {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    !(name.starts_with('.')
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name == "4913")
}
