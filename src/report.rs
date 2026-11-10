//! Human-readable output: test progress, failures, dirty reports and summaries.

use crate::diag::Diag;
use crate::runtime::undo::DirtyReport;
use crate::runtime::value::truncate;
use crate::runtime::{Exchange, Failure, PageReport};
use crate::source::Sources;
use crate::style::{bold, cyan, dim, green, red, yellow};
use crate::testing::{CaseResult, humantime};
use std::time::Duration;

const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
];

pub fn is_secret_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    SECRET_HEADERS.contains(&n.as_str())
        || ["token", "secret", "password"]
            .iter()
            .any(|w| n.contains(w))
}

pub fn redact(name: &str, value: &str) -> String {
    if !is_secret_header(name) {
        return value.to_string();
    }
    match value.split_once(' ') {
        Some((scheme, _)) if name.eq_ignore_ascii_case("authorization") => format!("{scheme} ***"),
        _ => "***".into(),
    }
}

fn location(sources: &Sources, span: crate::source::Span) -> String {
    let (line, col) = sources.line_col(span.file, span.start);
    format!("{}:{line}:{col}", sources.get(span.file).name)
}

/// Extend a point span to the end of the expression it starts, within its line.
fn widen(sources: &Sources, span: crate::source::Span) -> crate::source::Span {
    if span.end > span.start + 1 {
        return span;
    }
    let text = &sources.get(span.file).text;
    let line_end = text[span.start..]
        .find('\n')
        .map_or(text.len(), |i| span.start + i);
    match crate::syntax::scan::stmt_end(text, span.start) {
        Ok(end) if end > span.start => {
            let (_, e) = crate::syntax::scan::trim_range(text, span.start, end.min(line_end));
            crate::source::Span::new(span.file, span.start, e.max(span.start + 1))
        }
        _ => span,
    }
}

/// Render a failure with its source location, values and the request/response involved.
pub fn render_failure(f: &Failure, sources: &Sources) -> String {
    let mut d = Diag::new(f.message.clone());
    if let Some(span) = f.span.map(|s| widen(sources, s)) {
        let label = f
            .details
            .iter()
            .map(|(k, v)| format!("{k}: {}", truncate(v, 120)))
            .collect::<Vec<_>>()
            .join(", ");
        d = d.label(span, label);
    } else {
        for (k, v) in &f.details {
            d = d.note(format!("{k}: {}", truncate(v, 300)));
        }
    }
    let trace: Vec<_> = f.trace.iter().filter(|s| Some(**s) != f.span).collect();
    for span in trace.iter().rev() {
        d = d.note(format!("called from {}", location(sources, **span)));
    }
    let mut out = d.render(sources);
    if !f.values.is_empty() {
        out.push_str(&format!("  {}\n", dim("where:")));
        for (k, v) in &f.values {
            let v = truncate(v, 300);
            if k.chars().count() + v.chars().count() <= 90 {
                out.push_str(&format!("    {} {} {v}\n", bold(k), dim("=")));
            } else {
                out.push_str(&format!("    {} {}\n      {v}\n", bold(k), dim("=")));
            }
        }
    }
    if let Some(x) = &f.exchange {
        out.push_str(&render_exchange(x));
    }
    if let Some(p) = &f.page {
        out.push_str(&render_page(p));
    }
    out
}

/// Console lines shown per failure; the newest are the likeliest to matter.
const CONSOLE_LINES: usize = 8;

pub fn render_page(p: &PageReport) -> String {
    let title = if p.title.is_empty() {
        String::new()
    } else {
        format!(" {}", dim(format!("\"{}\"", truncate(&p.title, 60))))
    };
    let mut out = format!("  {} {}{title}\n", dim("page:"), p.url);
    match &p.screenshot {
        Ok(path) => out.push_str(&format!("  {} {path}\n", dim("screenshot:"))),
        Err(why) => out.push_str(&format!("  {} {}\n", dim("screenshot:"), red(why))),
    }
    if !p.console.is_empty() {
        out.push_str(&format!("  {}\n", dim("console:")));
        let skip = p.console.len().saturating_sub(CONSOLE_LINES);
        if skip > 0 {
            out.push_str(&format!("    {}\n", dim(format!("… {skip} earlier"))));
        }
        for line in &p.console[skip..] {
            let first = line.lines().next().unwrap_or_default();
            out.push_str(&format!("    {}\n", truncate(first, 200)));
        }
    }
    if !p.failed_requests.is_empty() {
        out.push_str(&format!("  {}\n", dim("failed requests:")));
        let skip = p.failed_requests.len().saturating_sub(CONSOLE_LINES);
        if skip > 0 {
            out.push_str(&format!("    {}\n", dim(format!("… {skip} earlier"))));
        }
        for r in &p.failed_requests[skip..] {
            out.push_str(&format!("    {}\n", red(truncate(r, 200))));
        }
    }
    out
}

pub fn render_exchange(x: &Exchange) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  {} {} {}\n",
        dim("request:"),
        bold(&x.method),
        x.url
    ));
    for (k, v) in &x.request_headers {
        out.push_str(&format!("    {}\n", dim(format!("{k}: {}", redact(k, v)))));
    }
    if let Some(body) = x.request_body.as_deref().filter(|b| !b.is_empty()) {
        out.push_str(&indent(&pretty_body(body, 20), "    "));
    }
    match (x.status, &x.error) {
        (Some(status), _) => {
            let reason = crate::runtime::request::reason(status);
            let ms = x
                .duration
                .map(|d| format!(" ({} ms)", d.as_millis()))
                .unwrap_or_default();
            let status = format!("{status} {reason}");
            let status = if x.status.unwrap_or(0) < 400 {
                green(status)
            } else {
                red(status)
            };
            out.push_str(&format!("  {} {status}{}\n", dim("response:"), dim(ms)));
            if let Some(ct) = x
                .response_headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            {
                out.push_str(&format!("    {}\n", dim(format!("content-type: {}", ct.1))));
            }
            if let Some(body) = x.response_body.as_deref().filter(|b| !b.is_empty()) {
                out.push_str(&indent(&pretty_body(body, 30), "    "));
            }
        }
        (None, Some(err)) => out.push_str(&format!("  {} {}\n", dim("response:"), red(err))),
        (None, None) => {}
    }
    out
}

/// Pretty-print JSON bodies; show at most `max_lines` lines.
pub fn pretty_body(body: &str, max_lines: usize) -> String {
    let text = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| body.to_string());
    let lines: Vec<&str> = text.lines().collect();
    let mut out: String = lines
        .iter()
        .take(max_lines)
        .map(|l| format!("{}\n", truncate(l, 200)))
        .collect();
    if lines.len() > max_lines {
        out.push_str(&dim(format!("… {} more lines\n", lines.len() - max_lines)));
    }
    out
}

fn indent(text: &str, prefix: &str) -> String {
    text.lines().map(|l| format!("{prefix}{l}\n")).collect()
}

pub fn render_dirty(entries: &[(String, u64, &DirtyReport)], sources: &Sources) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "\n{} {}\n",
        red(bold("DIRTY")),
        bold(format!(
            "{} undo action{} couldn't complete; the environment may contain leftover state",
            entries.len(),
            if entries.len() == 1 { "" } else { "s" }
        ))
    ));
    for (case, seed, e) in entries {
        let what = if e.script {
            format!("undo: {}", bold(&e.label))
        } else {
            format!("undo for {}", bold(&e.label))
        };
        out.push_str(&format!(
            "\n  {} {what} ({})\n",
            red("✗"),
            location(sources, e.span)
        ));
        if !case.is_empty() {
            out.push_str(&format!(
                "    {} {case}  {}\n",
                dim("test:"),
                dim(format!("seed {seed}"))
            ));
        }
        out.push_str(&format!("    {} {}\n", dim("created by:"), e.origin));
        if !e.captures.is_empty() {
            let caps: Vec<String> = e.captures.iter().map(|(k, v)| format!("{k}={v}")).collect();
            out.push_str(&format!("    {} {}\n", dim("captures:"), caps.join(", ")));
        }
        for (i, a) in e.attempts.iter().enumerate() {
            out.push_str(&format!(
                "    {} {}\n",
                dim(format!("attempt {}:", i + 1)),
                a
            ));
        }
        if let Some(x) = &e.exchange
            && let Some(body) = x.response_body.as_deref().filter(|b| !b.is_empty())
        {
            out.push_str(&indent(&pretty_body(body, 10), "      "));
        }
        if e.consequent {
            out.push_str(&format!(
                "    {}\n",
                yellow("an earlier undo in the same scope failed too; this may be a consequence")
            ));
        }
    }
    out.push_str(&format!(
        "\n  {}\n",
        dim("clean these up by hand before relying on this environment again.")
    ));
    out
}

#[derive(Default)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub dirty: usize,
    pub interrupted: bool,
    pub stopped_dirty: bool,
    pub stopped_failed: bool,
}

impl Summary {
    pub fn exit_code(&self) -> i32 {
        if self.dirty > 0 {
            3
        } else if self.failed > 0 || self.interrupted {
            1
        } else {
            0
        }
    }
}

pub struct Reporter<'a> {
    pub sources: &'a Sources,
    pub verbose: bool,
    pub seed: u64,
    pub generated: bool,
    current_file: Option<String>,
    failures: Vec<CaseResult>,
    dirty: Vec<(String, u64, DirtyReport)>,
    /// Every case, for `--junit`.
    junit: Vec<JunitCase>,
    pub summary: Summary,
}

struct JunitCase {
    file: String,
    name: String,
    seconds: f64,
    outcome: JunitOutcome,
    logs: Vec<String>,
}

enum JunitOutcome {
    Passed,
    Failed { message: String, text: String },
    Skipped(String),
}

impl<'a> Reporter<'a> {
    pub fn new(sources: &'a Sources, verbose: bool, seed: u64) -> Self {
        Reporter {
            sources,
            verbose,
            seed,
            generated: false,
            current_file: None,
            failures: Vec::new(),
            dirty: Vec::new(),
            junit: Vec::new(),
            summary: Summary::default(),
        }
    }

    fn file(&mut self, file: &str) {
        if self.current_file.as_deref() != Some(file) {
            println!("{}", bold(file));
            self.current_file = Some(file.to_string());
        }
    }

    pub fn case(&mut self, file: &str, result: CaseResult) {
        self.file(file);
        self.generated |= result.generated;
        let time = dim(format!("({})", humantime(result.duration)));
        let dirty = if result.dirty.is_empty() {
            String::new()
        } else {
            format!(" {}", red(bold("DIRTY")))
        };
        match &result.outcome {
            Ok(()) => {
                self.summary.passed += 1;
                println!("  {} {} {time}{dirty}", green("✓"), result.label());
            }
            Err(_) => {
                self.summary.failed += 1;
                println!("  {} {} {time}{dirty}", red("✗"), result.label());
            }
        }
        if self.verbose {
            for line in &result.logs {
                println!("      {}", dim(line));
            }
        }
        if !result.dirty.is_empty() {
            self.summary.dirty += 1;
            for d in &result.dirty {
                self.dirty.push((result.label(), self.seed, d.clone()));
            }
        }
        self.junit.push(JunitCase {
            file: file.to_string(),
            name: result.label(),
            seconds: result.duration.as_secs_f64(),
            outcome: match &result.outcome {
                Ok(()) => JunitOutcome::Passed,
                Err(f) => JunitOutcome::Failed {
                    message: f.message.clone(),
                    text: plain(&render_failure(f, self.sources)),
                },
            },
            logs: result.logs.clone(),
        });
        if !result.dirty.is_empty() {
            let entries: Vec<(String, u64, &DirtyReport)> = result
                .dirty
                .iter()
                .map(|d| (result.label(), self.seed, d))
                .collect();
            let text = plain(&render_dirty(&entries, self.sources));
            if let Some(case) = self.junit.last_mut() {
                case.outcome = JunitOutcome::Failed {
                    message: "dirty: a cleanup failed and left state behind".into(),
                    text: match &case.outcome {
                        JunitOutcome::Failed { text: t, .. } => format!("{t}\n{text}"),
                        _ => text,
                    },
                };
            }
        }
        if result.outcome.is_err() {
            self.failures.push(result);
        }
    }

    /// The run as a JUnit XML report: one `<testsuite>` per file, in run order.
    pub fn junit(&self) -> String {
        let mut files: Vec<&str> = Vec::new();
        for c in &self.junit {
            if !files.contains(&c.file.as_str()) {
                files.push(&c.file);
            }
        }
        let count = |cases: &[&JunitCase], f: fn(&JunitOutcome) -> bool| {
            cases.iter().filter(|c| f(&c.outcome)).count()
        };
        let failed = |o: &JunitOutcome| matches!(o, JunitOutcome::Failed { .. });
        let skipped = |o: &JunitOutcome| matches!(o, JunitOutcome::Skipped(_));
        let all: Vec<&JunitCase> = self.junit.iter().collect();
        let total: f64 = all.iter().map(|c| c.seconds).sum();
        let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        out.push_str(&format!(
            "<testsuites name=\"klausur\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{total:.3}\">\n",
            all.len(),
            count(&all, failed),
            count(&all, skipped)
        ));
        for file in files {
            let cases: Vec<&JunitCase> = all.iter().copied().filter(|c| c.file == file).collect();
            let time: f64 = cases.iter().map(|c| c.seconds).sum();
            out.push_str(&format!(
                "  <testsuite name=\"{}\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{time:.3}\">\n",
                xml(file),
                cases.len(),
                count(&cases, failed),
                count(&cases, skipped)
            ));
            for c in cases {
                out.push_str(&format!(
                    "    <testcase classname=\"{}\" name=\"{}\" file=\"{}\" time=\"{:.3}\">\n",
                    xml(file),
                    xml(&c.name),
                    xml(file),
                    c.seconds
                ));
                match &c.outcome {
                    JunitOutcome::Passed => {}
                    JunitOutcome::Failed { message, text } => out.push_str(&format!(
                        "      <failure message=\"{}\">{}</failure>\n",
                        xml(message.lines().next().unwrap_or_default()),
                        xml(text)
                    )),
                    JunitOutcome::Skipped(reason) => {
                        out.push_str(&format!("      <skipped message=\"{}\"/>\n", xml(reason)))
                    }
                }
                if !c.logs.is_empty() {
                    out.push_str(&format!(
                        "      <system-out>{}</system-out>\n",
                        xml(&c.logs.join("\n"))
                    ));
                }
                out.push_str("    </testcase>\n");
            }
            out.push_str("  </testsuite>\n");
        }
        out.push_str("</testsuites>\n");
        out
    }

    pub fn skipped(&mut self, file: &str, name: &str, reason: &str) {
        self.file(file);
        self.summary.skipped += 1;
        let shown = if reason.is_empty() {
            String::new()
        } else {
            format!(" {}", dim(format!("— {reason}")))
        };
        println!("  {} {name}{shown}", yellow("○"));
        self.junit.push(JunitCase {
            file: file.to_string(),
            name: name.to_string(),
            seconds: 0.0,
            outcome: JunitOutcome::Skipped(reason_text(reason)),
            logs: Vec::new(),
        });
    }

    /// A test that couldn't even produce cases (modifier or parameter errors).
    pub fn test_error(&mut self, file: &str, name: &str, failure: Failure) {
        self.case(file, CaseResult::error(name.to_string(), failure));
    }

    pub fn finish(&mut self, elapsed: Duration) {
        if !self.failures.is_empty() {
            println!("\n{}", bold("Failures"));
            for r in &self.failures {
                println!("\n{} {}", red("──"), bold(r.label()));
                if let Some(args) = r.args_line() {
                    println!("  {} {}", dim("args:"), dim(args));
                }
                if let Err(f) = &r.outcome {
                    print!("{}", render_failure(f, self.sources));
                }
                if r.generated {
                    println!(
                        "  {} {}",
                        dim("rerun:"),
                        cyan(format!("klausur test --seed {} {:?}", self.seed, r.test))
                    );
                }
                if !r.logs.is_empty() && !self.verbose {
                    println!("  {}", dim("log:"));
                    for line in &r.logs {
                        println!("    {}", dim(line));
                    }
                }
            }
        }
        if !self.dirty.is_empty() {
            let entries: Vec<(String, u64, &DirtyReport)> = self
                .dirty
                .iter()
                .map(|(c, s, d)| (c.clone(), *s, d))
                .collect();
            print!("{}", render_dirty(&entries, self.sources));
        }
        let s = &self.summary;
        let mut parts = vec![green(format!("{} passed", s.passed))];
        if s.failed > 0 {
            parts.push(red(format!("{} failed", s.failed)));
        }
        if s.skipped > 0 {
            parts.push(yellow(format!("{} skipped", s.skipped)));
        }
        if s.dirty > 0 {
            parts.push(red(bold(format!("{} dirty", s.dirty))));
        }
        let seed = if self.generated {
            format!(", seed {}", self.seed)
        } else {
            String::new()
        };
        println!(
            "\n{} {}",
            parts.join(", "),
            dim(format!("({}{seed})", humantime(elapsed)))
        );
        if s.interrupted {
            println!("{}", yellow("interrupted: remaining tests were not run"));
        }
        if s.stopped_dirty {
            println!(
                "{}",
                yellow("stopped after a dirty test (use --on-dirty=continue to keep going)")
            );
        }
        if s.stopped_failed {
            println!(
                "{}",
                yellow("stopped after the first failure (--fail-fast)")
            );
        }
    }
}

fn reason_text(reason: &str) -> String {
    if reason.is_empty() {
        "skipped".into()
    } else {
        reason.to_string()
    }
}

/// Terminal output without colour codes, for files.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Text for an XML attribute or element: escaped, without the control characters XML 1.0
/// doesn't allow.
fn xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' | '\t' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            c => out.push(c),
        }
    }
    out
}
