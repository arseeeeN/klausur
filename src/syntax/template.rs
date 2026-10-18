//! `{{ expr }}` templates in URLs, headers and bodies.

use super::scan::{ScanResult, scan_interp, trim_range};
use crate::source::{FileId, Span};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterpMode {
    /// Inserted as plain text (headers, raw bodies).
    Text,
    /// Inserted as a JSON value (`{"a": {{x}}}`).
    JsonValue,
    /// Inserted into a JSON string literal, escaped but without quotes (`{"a": "id-{{x}}"}`).
    JsonString,
    /// At the start of a URL: inserted verbatim (`{{base}}/users`).
    UrlRaw,
    /// Anywhere else in a URL: percent-encoded.
    UrlEncoded,
}

#[derive(Debug, Clone)]
pub enum Part {
    Lit(String),
    Expr { span: Span, mode: InterpMode },
}

#[derive(Debug, Clone)]
pub struct Template {
    pub parts: Vec<Part>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateKind {
    Text,
    Url,
    Json,
}

impl Template {
    /// The template text with interpolations shown as `{{…}}`, for display.
    pub fn display(&self, src: &str) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Lit(s) => out.push_str(s),
                Part::Expr { span, .. } => {
                    out.push_str("{{");
                    out.push_str(&src[span.range()]);
                    out.push_str("}}");
                }
            }
        }
        out
    }
}

/// Parse the template in `src[start..end]`, appending to `parts`.
///
/// `in_string` carries the JSON string state across calls, so multi-line JSON bodies can be
/// parsed line by line.
pub fn parse_into(
    src: &str,
    file: FileId,
    start: usize,
    end: usize,
    kind: TemplateKind,
    parts: &mut Vec<Part>,
    in_string: &mut bool,
) -> ScanResult<()> {
    let mut lit_start = start;
    let mut pos = start;
    let bytes = src.as_bytes();
    while pos < end {
        if bytes[pos] == b'{' && bytes.get(pos + 1) == Some(&b'{') {
            push_lit(parts, &src[lit_start..pos]);
            let close = scan_interp(src, pos + 2)?;
            let (es, ee) = trim_range(src, pos + 2, close);
            if es == ee {
                return Err(super::scan::ScanError {
                    pos,
                    message: "empty `{{ }}` interpolation".into(),
                });
            }
            let mode = match kind {
                TemplateKind::Text => InterpMode::Text,
                TemplateKind::Url if parts.is_empty() => InterpMode::UrlRaw,
                TemplateKind::Url => InterpMode::UrlEncoded,
                TemplateKind::Json if *in_string => InterpMode::JsonString,
                TemplateKind::Json => InterpMode::JsonValue,
            };
            parts.push(Part::Expr {
                span: Span::new(file, es, ee),
                mode,
            });
            pos = close + 2;
            lit_start = pos;
            continue;
        }
        if kind == TemplateKind::Json {
            match bytes[pos] {
                b'\\' if *in_string => pos += 1,
                b'"' => *in_string = !*in_string,
                _ => {}
            }
        }
        pos += 1;
    }
    push_lit(parts, &src[lit_start..end.min(src.len())]);
    Ok(())
}

pub fn parse(
    src: &str,
    file: FileId,
    start: usize,
    end: usize,
    kind: TemplateKind,
) -> ScanResult<Template> {
    let mut parts = Vec::new();
    parse_into(src, file, start, end, kind, &mut parts, &mut false)?;
    Ok(Template {
        parts,
        span: Span::new(file, start, end),
    })
}

pub fn push_lit(parts: &mut Vec<Part>, s: &str) {
    if s.is_empty() {
        return;
    }
    if let Some(Part::Lit(prev)) = parts.last_mut() {
        prev.push_str(s);
    } else {
        parts.push(Part::Lit(s.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(t: &Template) -> Vec<InterpMode> {
        t.parts
            .iter()
            .filter_map(|p| match p {
                Part::Expr { mode, .. } => Some(*mode),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn url_modes() {
        let src = "{{base}}/users/{{id}}";
        let t = parse(src, 0, 0, src.len(), TemplateKind::Url).unwrap();
        assert_eq!(modes(&t), [InterpMode::UrlRaw, InterpMode::UrlEncoded]);
    }

    #[test]
    fn json_modes() {
        let src = r#"{ "a": {{x}}, "b": "id-{{y}}", "c\"": {{z}} }"#;
        let t = parse(src, 0, 0, src.len(), TemplateKind::Json).unwrap();
        assert_eq!(
            modes(&t),
            [
                InterpMode::JsonValue,
                InterpMode::JsonString,
                InterpMode::JsonValue
            ]
        );
    }
}
