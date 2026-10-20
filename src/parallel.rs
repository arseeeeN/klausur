//! Worker threads for `parallel` tests.
//!
//! Script values aren't thread-safe, so each worker loads its own copy of the program and
//! rebuilds a test's cases from the same seed. The main thread only sends `(test, case)`
//! indices and gets finished [`CaseResult`]s back.

use crate::load::{self, LoadOptions, Program};
use crate::runtime::Failure;
use crate::testing::{self, CaseResult, CaseSpec, TestDef};
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// A case to run: its position in the run, the test's index and the case's index.
pub struct Job {
    pub seq: usize,
    pub test: usize,
    pub case: usize,
}

pub struct Pool {
    jobs: Option<Sender<Job>>,
    results: Receiver<(usize, CaseResult)>,
    workers: Vec<JoinHandle<()>>,
}

impl Pool {
    pub fn start(n: usize, paths: &[PathBuf], mut opts: LoadOptions) -> Pool {
        opts.config.trace_in_log = true;
        let (job_tx, job_rx) = channel::<Job>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, results) = channel();
        let workers = (0..n)
            .map(|_| {
                let jobs = job_rx.clone();
                let results = result_tx.clone();
                let paths = paths.to_vec();
                let opts = opts.clone();
                std::thread::spawn(move || worker(&paths, opts, &jobs, &results))
            })
            .collect();
        Pool {
            jobs: Some(job_tx),
            results,
            workers,
        }
    }

    pub fn send(&self, job: Job) {
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send(job);
        }
    }

    /// The next finished case, in completion order.
    pub fn recv(&self) -> Option<(usize, CaseResult)> {
        self.results.recv().ok()
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.jobs = None;
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

struct Test {
    def: TestDef,
    cases: Vec<Option<CaseSpec>>,
}

fn worker(
    paths: &[PathBuf],
    opts: LoadOptions,
    jobs: &Mutex<Receiver<Job>>,
    results: &Sender<(usize, CaseResult)>,
) {
    let seed = opts.seed;
    let program = load::load(paths, opts).ok();
    let mut tests: HashMap<usize, Result<Test, Failure>> = HashMap::new();
    loop {
        let job = match jobs.lock() {
            Ok(rx) => match rx.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
            Err(_) => return,
        };
        let result = match &program {
            Some(program) => {
                std::panic::catch_unwind(AssertUnwindSafe(|| {
                    run(program, &mut tests, &job, seed)
                }))
                .unwrap_or_else(|panic| {
                    let why = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_default();
                    failed(
                        program,
                        job.test,
                        Failure::new(format!("klausur crashed running this case: {why}")),
                    )
                })
            }
            None => CaseResult::error(
                String::new(),
                Failure::new("a worker couldn't load the test files"),
            ),
        };
        if results.send((job.seq, result)).is_err() {
            return;
        }
    }
}

fn run(
    program: &Program,
    tests: &mut HashMap<usize, Result<Test, Failure>>,
    job: &Job,
    seed: u64,
) -> CaseResult {
    let shared = &program.shared;
    let engine = &program.engine;
    let test = &program.tests[job.test];
    let entry = tests.entry(job.test).or_insert_with(|| {
        let def = testing::build_def(shared, engine, test)?;
        let cases = testing::expand_cases(shared, engine, test, &def.0.borrow(), seed)?;
        Ok(Test {
            def,
            cases: cases.into_iter().map(Some).collect(),
        })
    });
    let t = match entry {
        Ok(t) => t,
        Err(f) => return failed(program, job.test, f.clone()),
    };
    let Some(spec) = t.cases.get_mut(job.case).and_then(Option::take) else {
        return failed(
            program,
            job.test,
            Failure::new("a worker built different cases for this test"),
        );
    };
    testing::run_case(shared, engine, test, &t.def.0.borrow(), spec)
}

fn failed(program: &Program, test: usize, f: Failure) -> CaseResult {
    CaseResult::error(program.tests[test].name.clone(), f)
}
