// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! `cargo test --test js`: every JavaScript test file in `js/`, plus the Rust-side browser checks,
//! each in its own fresh browser. Every case is reported by name, libtest style:
//!
//! ```text
//! test shell-geo :: forget clears consent ... ok
//! ```
//!
//! Arguments (after `--`): substrings that select cases by full name (`suite :: case`), `--exact`,
//! `--list`, `--nocapture` (print every case's console output), and `--test-threads N` (how many
//! suites run at once; default `BROWSER_TESTS_JOBS`, else 1, as `node --test
//! --test-concurrency=1` did). Other libtest flags are accepted and ignored.

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use browser_tests::suite::{self, Suite, SuiteReport};
use browser_tests::{chrome_from_env, repo_root};

struct Args {
    filters: Vec<String>,
    exact: bool,
    list: bool,
    nocapture: bool,
    jobs: usize,
}

fn parse_args() -> Args {
    let mut args = Args {
        filters: Vec::new(),
        exact: false,
        list: false,
        nocapture: false,
        jobs: std::env::var("BROWSER_TESTS_JOBS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1),
    };
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--exact" => args.exact = true,
            "--list" => args.list = true,
            "--nocapture" | "--no-capture" => args.nocapture = true,
            "--test-threads" => {
                if let Some(value) = iter.next().and_then(|value| value.parse().ok()) {
                    args.jobs = value;
                }
            }
            other if other.starts_with("--test-threads=") => {
                if let Ok(value) = other["--test-threads=".len()..].parse() {
                    args.jobs = value;
                }
            }
            // Flags that take a value.
            "--format" | "--color" | "--skip" | "-Z" | "--logfile" | "--report-time" => {
                let _ = iter.next();
            }
            other if other.starts_with('-') => {}
            filter => args.filters.push(filter.to_owned()),
        }
    }
    args.jobs = args.jobs.max(1);
    args
}

impl Args {
    fn selects(&self, name: &str) -> bool {
        self.filters.is_empty()
            || self.filters.iter().any(|filter| {
                if self.exact {
                    name == filter
                } else {
                    name.contains(filter.as_str())
                }
            })
    }

    /// Whether any case of `suite` might be selected (a filter may name the suite alone).
    fn may_select(&self, suite: &Suite) -> bool {
        self.filters.is_empty()
            || self
                .filters
                .iter()
                .any(|filter| !self.exact || filter.starts_with(&format!("{} :: ", suite.name)))
    }
}

fn main() -> ExitCode {
    let args = parse_args();
    let Some(chrome) = chrome_from_env() else {
        println!("\nrunning 0 tests: CHROME_HEADLESS_SHELL is not set, browser tests skipped\n");
        return ExitCode::SUCCESS;
    };
    let root = repo_root();
    let suites: Vec<Suite> = match suite::discover(&root) {
        Ok(suites) => suites
            .into_iter()
            .filter(|suite| args.may_select(suite))
            .collect(),
        Err(error) => {
            eprintln!("could not list {}: {error}", suite::JS_DIR);
            return ExitCode::FAILURE;
        }
    };
    if args.list {
        return list(&chrome, &root, &suites, &args);
    }
    let started = Instant::now();
    println!(
        "\nrunning {} browser test suites, {} at a time",
        suites.len(),
        args.jobs.min(suites.len().max(1))
    );
    let args = Arc::new(args);
    let queue = Arc::new(Mutex::new(VecDeque::from(suites)));
    let (sender, receiver) = mpsc::channel::<SuiteReport>();
    let workers: Vec<_> = (0..args.jobs)
        .map(|_| {
            let queue = Arc::clone(&queue);
            let sender = sender.clone();
            let args = Arc::clone(&args);
            let chrome = chrome.clone();
            let root = root.clone();
            // Each worker launches browsers from its own thread and runtime, so the browser dies
            // (PR_SET_PDEATHSIG) if the worker does.
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                loop {
                    let next = queue
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .pop_front();
                    let Some(suite) = next else { break };
                    let selected = |name: &str| args.selects(name);
                    let report = runtime.block_on(suite::run(&chrome, &root, &suite, &selected));
                    if sender.send(report).is_err() {
                        break;
                    }
                }
            })
        })
        .collect();
    drop(sender);

    let mut reports = Vec::new();
    for report in receiver {
        print_report(&report, args.nocapture);
        reports.push(report);
    }
    for worker in workers {
        let _ = worker.join();
    }
    summarize(&reports, started.elapsed())
}

fn list(chrome: &Path, root: &Path, suites: &[Suite], args: &Args) -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut count = 0;
    for suite in suites {
        match runtime.block_on(suite::list(chrome, root, suite)) {
            Ok(names) => {
                for name in names {
                    let full = suite::full_name(&suite.name, &name);
                    if args.selects(&full) {
                        println!("{full}: test");
                        count += 1;
                    }
                }
            }
            Err(error) => {
                eprintln!("{}: {error}", suite.name);
                return ExitCode::FAILURE;
            }
        }
    }
    println!("\n{count} tests, 0 benchmarks");
    ExitCode::SUCCESS
}

fn print_report(report: &SuiteReport, nocapture: bool) {
    let mut out = std::io::stdout().lock();
    for case in &report.cases {
        let status = if case.failure.is_some() {
            "FAILED"
        } else {
            "ok"
        };
        let _ = writeln!(
            out,
            "test {} ... {status}",
            suite::full_name(&report.suite, &case.name)
        );
        if nocapture {
            for entry in &case.log {
                let _ = writeln!(out, "    {entry}");
            }
        }
    }
    for error in &report.errors {
        let _ = writeln!(
            out,
            "suite {} ... FAILED: {}",
            report.suite,
            first_line(error)
        );
    }
    let _ = out.flush();
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or("")
}

fn summarize(reports: &[SuiteReport], elapsed: Duration) -> ExitCode {
    let mut passed = 0;
    let mut failed = Vec::new();
    let mut filtered = 0;
    let mut suite_errors = Vec::new();
    for report in reports {
        filtered += report.filtered;
        for case in &report.cases {
            match &case.failure {
                None => passed += 1,
                Some(failure) => failed.push((
                    suite::full_name(&report.suite, &case.name),
                    failure.clone(),
                    &case.log,
                )),
            }
        }
        for error in &report.errors {
            suite_errors.push((report.suite.clone(), error.clone()));
        }
    }
    if !failed.is_empty() || !suite_errors.is_empty() {
        println!("\nfailures:\n");
        for (name, failure, log) in &failed {
            println!("---- {name} ----\n{failure}");
            let output: Vec<String> = log
                .iter()
                .filter(|entry| !entry.is_failure())
                .map(ToString::to_string)
                .collect();
            if !output.is_empty() {
                println!("page output:\n    {}", output.join("\n    "));
            }
            println!();
        }
        for (suite, error) in &suite_errors {
            println!("---- suite {suite} ----\n{error}\n");
        }
        println!("failures:");
        for (name, _, _) in &failed {
            println!("    {name}");
        }
        for (suite, _) in &suite_errors {
            println!("    suite {suite}");
        }
    }
    let ok = failed.is_empty() && suite_errors.is_empty();
    println!(
        "\ntest result: {}. {passed} passed; {} failed; {} suite errors; {filtered} filtered out; finished in {:.2}s\n",
        if ok { "ok" } else { "FAILED" },
        failed.len(),
        suite_errors.len(),
        elapsed.as_secs_f64()
    );
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(101)
    }
}
