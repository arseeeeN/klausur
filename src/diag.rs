//! Diagnostics: errors with labelled source spans, rendered through miette.

use crate::source::{Sources, Span};
use miette::{LabeledSpan, MietteDiagnostic, NamedSource, Report};

#[derive(Debug, Clone)]
pub struct Diag {
    pub message: String,
    pub labels: Vec<(Span, String)>,
    pub help: Option<String>,
    pub notes: Vec<String>,
}

impl Diag {
    pub fn new(message: impl Into<String>) -> Self {
        Diag {
            message: message.into(),
            labels: Vec::new(),
            help: None,
            notes: Vec::new(),
        }
    }

    pub fn at(span: Span, message: impl Into<String>) -> Self {
        Diag::new(message).label(span, "")
    }

    pub fn label(mut self, span: Span, text: impl Into<String>) -> Self {
        self.labels.push((span, text.into()));
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    pub fn render(&self, sources: &Sources) -> String {
        let mut diag = MietteDiagnostic::new(self.message.clone());
        if let Some(help) = &self.help {
            diag = diag.with_help(help.clone());
        }
        // miette shows one source per diagnostic; labels in other files become notes.
        let file = self.labels.first().map(|(s, _)| s.file);
        let mut extra = Vec::new();
        let mut labels = Vec::new();
        for (span, text) in &self.labels {
            if Some(span.file) == file {
                let text = (!text.is_empty()).then(|| text.clone());
                let len = span.end.saturating_sub(span.start).max(1);
                labels.push(LabeledSpan::new(text, span.start, len));
            } else {
                let (line, col) = sources.line_col(span.file, span.start);
                extra.push(format!(
                    "{}:{line}:{col}: {text}",
                    sources.get(span.file).name
                ));
            }
        }
        diag = diag.with_labels(labels);
        let mut report = Report::new(diag);
        if let Some(file) = file {
            let f = sources.get(file);
            report = report.with_source_code(NamedSource::new(f.name.clone(), f.text.clone()));
        }
        let mut out = format!("{report:?}");
        for note in self.notes.iter().chain(&extra) {
            out.push_str(&format!("  note: {note}\n"));
        }
        out
    }
}

pub type PResult<T> = Result<T, Diag>;

pub fn suggest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<String> {
    candidates
        .into_iter()
        .map(|c| (strsim::jaro_winkler(name, c), c))
        .filter(|(score, _)| *score > 0.8)
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(_, c)| c.to_string())
}
