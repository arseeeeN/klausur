//! Pre-pass over Rhai code: named arguments and namespace qualification.
//!
//! - `f(a: 1, b: 2)` becomes `f__kw(#{a: 1, b: 2})`; positional arguments mixed in are passed
//!   as `_0`, `_1`, …. The `__kw` variant of every callable unpacks the map.
//! - `ns::f(…)` becomes `f(…)` when `ns` is an import alias of the file.
//!
//! - In statement blocks, a newline that ends a statement gets a `;`, so semicolons are optional.
//!
//! Everything else is copied verbatim, so the source map stays exact.

use crate::diag::{Diag, PResult, suggest};
use crate::source::{CodeBuilder, FileId, Span};
use crate::syntax::parser::split_top_level;
use crate::syntax::scan::{self, ScanError, ident_at, is_ident_char, skip_trivia};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Statements: newlines that end a statement get a `;`.
    Block,
    Paren,
    Map,
    Switch,
}

pub struct Rewriter<'a> {
    pub src: &'a str,
    pub file: FileId,
    /// Import aliases of this file.
    pub aliases: &'a HashMap<String, FileId>,
    /// Names defined (fns and requests) per file.
    pub defs: &'a HashMap<FileId, HashSet<String>>,
}

impl Rewriter<'_> {
    fn scan<T>(&self, r: Result<T, ScanError>) -> PResult<T> {
        r.map_err(|e| Diag::at(Span::new(self.file, e.pos, e.pos + 1), e.message))
    }

    /// Rewrite an expression in `src[start..end]` into `out`.
    pub fn rewrite(&self, start: usize, end: usize, out: &mut CodeBuilder) -> PResult<()> {
        self.rewrite_in(start, end, out, Kind::Paren)
    }

    /// Rewrite statements (a function or test body) in `src[start..end]` into `out`.
    pub fn rewrite_block(&self, start: usize, end: usize, out: &mut CodeBuilder) -> PResult<()> {
        self.rewrite_in(start, end, out, Kind::Block)
    }

    fn rewrite_in(
        &self,
        start: usize,
        end: usize,
        out: &mut CodeBuilder,
        top: Kind,
    ) -> PResult<()> {
        let bytes = self.src.as_bytes();
        let mut pos = start;
        let mut copied = start;
        let mut stack = vec![top];
        // The last significant character of the current statement and the position after it.
        let mut last: Option<u8> = None;
        let mut last_end = start;
        let mut pending_switch = false;
        while pos < end {
            let c = bytes[pos];
            match c {
                b'/' if bytes.get(pos + 1) == Some(&b'/') => {
                    pos = self.src[pos..end].find('\n').map_or(end, |i| pos + i);
                    continue;
                }
                b'/' if bytes.get(pos + 1) == Some(&b'*') => {
                    pos = self.scan(skip_trivia(self.src, pos))?.min(end);
                    continue;
                }
                b'"' | b'`' | b'\'' => {
                    pos = self.scan(scan::skip_string(self.src, pos))?;
                    last = Some(b'"');
                    last_end = pos;
                    continue;
                }
                b'#' if bytes.get(pos + 1) == Some(&b'{') => {
                    stack.push(Kind::Map);
                    pos += 2;
                    last = Some(b'{');
                    continue;
                }
                b'{' => {
                    stack.push(if std::mem::take(&mut pending_switch) {
                        Kind::Switch
                    } else {
                        Kind::Block
                    });
                }
                b'(' | b'[' => stack.push(Kind::Paren),
                b')' | b']' | b'}' => {
                    if stack.len() > 1 {
                        stack.pop();
                    }
                }
                b'\n' => {
                    let ends_statement = stack.last() == Some(&Kind::Block)
                        && last.is_some_and(|l| !b";{,([=+-*/%&|^<>!.:?".contains(&l))
                        && !self.continues(pos + 1, end);
                    if ends_statement {
                        out.push_orig(self.file, copied, &self.src[copied..last_end]);
                        out.push_gen(";");
                        copied = last_end;
                        last = Some(b';');
                    }
                    pos += 1;
                    continue;
                }
                c if scan::is_ident_start(c)
                    && (pos == 0 || !is_ident_char(bytes[pos - 1]) && bytes[pos - 1] != b'.') =>
                {
                    let (path_end, segments) = self.path(pos);
                    last = Some(b'a');
                    last_end = path_end;
                    if &self.src[pos..path_end] == "switch" {
                        pending_switch = true;
                    }
                    if bytes.get(path_end) != Some(&b'(') {
                        pos = path_end;
                        continue;
                    }
                    // Resolve `alias::name`.
                    let (name_start, name_end) = *segments.last().unwrap();
                    let mut strip = false;
                    if segments.len() == 2 {
                        let alias = &self.src[segments[0].0..segments[0].1];
                        if let Some(target) = self.aliases.get(alias) {
                            let name = &self.src[name_start..name_end];
                            let defined = self.defs.get(target).is_some_and(|d| d.contains(name));
                            if !defined {
                                let mut d = Diag::at(
                                    Span::new(self.file, pos, name_end),
                                    format!("`{alias}` has no `{name}`"),
                                );
                                if let Some(s) = self
                                    .defs
                                    .get(target)
                                    .and_then(|d| suggest(name, d.iter().map(String::as_str)))
                                {
                                    d = d.help(format!("did you mean `{alias}::{s}`?"));
                                }
                                return Err(d);
                            }
                            strip = true;
                        }
                    }
                    let open = path_end;
                    let close = self.scan(scan::match_close(self.src, open))?;
                    let args = self.scan(split_top_level(self.src, open + 1, close))?;
                    let named = args.iter().any(|&(s, e)| self.is_named_arg(s, e));
                    if !named {
                        if strip {
                            out.push_orig(self.file, copied, &self.src[copied..pos]);
                            copied = name_start;
                        }
                        pos = name_end;
                        continue;
                    }
                    out.push_orig(self.file, copied, &self.src[copied..pos]);
                    out.push_orig(self.file, name_start, &self.src[name_start..name_end]);
                    out.push_gen("__kw(#{");
                    for (i, &(s, e)) in args.iter().enumerate() {
                        let (s, e) = scan::trim_range(self.src, s, e);
                        if s == e {
                            continue;
                        }
                        if i > 0 {
                            out.push_gen(", ");
                        }
                        if !self.is_named_arg(s, e) {
                            out.push_gen(&format!("\"_{i}\": "));
                        }
                        self.rewrite(s, e, out)?;
                    }
                    out.push_gen("})");
                    pos = close + 1;
                    copied = pos;
                    last = Some(b')');
                    last_end = pos;
                    continue;
                }
                c if c.is_ascii_whitespace() => {
                    pos += 1;
                    continue;
                }
                _ => {}
            }
            last = Some(c);
            pos += 1;
            last_end = pos;
        }
        out.push_orig(self.file, copied, &self.src[copied..end.max(copied)]);
        Ok(())
    }

    /// Does the code after a newline continue the current statement?
    fn continues(&self, pos: usize, end: usize) -> bool {
        let Ok(p) = skip_trivia(self.src, pos) else {
            return false;
        };
        if p >= end {
            return true; // end of the block: keep its value
        }
        let rest = &self.src[p..end];
        let word = ident_at(self.src, p).map(|(s, e)| &self.src[s..e]);
        (rest.starts_with('.') && !rest.starts_with(".."))
            || [
                "&&", "||", "?", "|>", "{", "}", ")", "]", ",", "==", "!=", "+", "*",
            ]
            .iter()
            .any(|t| rest.starts_with(t))
            || matches!(word, Some("else" | "catch"))
    }

    /// `a::b::c` starting at `pos`: returns the end and the segments.
    fn path(&self, pos: usize) -> (usize, Vec<(usize, usize)>) {
        let mut segments = Vec::new();
        let mut p = pos;
        while let Some((s, e)) = ident_at(self.src, p) {
            segments.push((s, e));
            if self.src[e..].starts_with("::") && ident_at(self.src, e + 2).is_some() {
                p = e + 2;
            } else {
                return (e, segments);
            }
        }
        (p, segments)
    }

    /// `name: value` (but not `name::x`).
    fn is_named_arg(&self, s: usize, e: usize) -> bool {
        let Ok(s) = skip_trivia(self.src, s) else {
            return false;
        };
        let Some((_, ie)) = ident_at(self.src, s) else {
            return false;
        };
        let Ok(after) = skip_trivia(self.src, ie) else {
            return false;
        };
        after < e
            && self.src.as_bytes()[after] == b':'
            && self.src.as_bytes().get(after + 1) != Some(&b':')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(src: &str) -> String {
        let aliases = HashMap::from([("orders".to_string(), 1)]);
        let defs = HashMap::from([(1, HashSet::from(["create".to_string()]))]);
        let r = Rewriter {
            src,
            file: 0,
            aliases: &aliases,
            defs: &defs,
        };
        let mut out = CodeBuilder::new(Span::new(0, 0, 0));
        r.rewrite(0, src.len(), &mut out).unwrap();
        out.text
    }

    #[test]
    fn named_arguments_become_maps() {
        assert_eq!(
            rewrite(r#"createUser(name: "bob")"#),
            r#"createUser__kw(#{name: "bob"})"#
        );
        assert_eq!(
            rewrite(r#"f("x", role: g(a: 1))"#),
            r#"f__kw(#{"_0": "x", role: g__kw(#{a: 1})})"#
        );
        assert_eq!(rewrite("f(#{a: 1}, 2)"), "f(#{a: 1}, 2)");
        assert_eq!(rewrite(r#"x.method(a: 1)"#), r#"x.method(a: 1)"#);
        assert_eq!(rewrite(r#""f(a: 1)""#), r#""f(a: 1)""#);
    }

    #[test]
    fn semicolons_are_inserted_between_statements() {
        let aliases = HashMap::new();
        let defs = HashMap::new();
        let src = "let a = 1 // one\nlet b = a +\n  2\nif a == 1 {\n  b\n}\nelse { 3 }\nlet m = #{\n x: 1\n}\nfoo()\n  .bar()\nswitch a {\n 1 => 2,\n _ => 3\n}\nb";
        let r = Rewriter {
            src,
            file: 0,
            aliases: &aliases,
            defs: &defs,
        };
        let mut out = CodeBuilder::new(Span::new(0, 0, 0));
        r.rewrite_block(0, src.len(), &mut out).unwrap();
        assert_eq!(
            out.text,
            "let a = 1; // one\nlet b = a +\n  2;\nif a == 1 {\n  b\n}\nelse { 3 };\nlet m = #{\n x: 1\n};\nfoo()\n  .bar();\nswitch a {\n 1 => 2,\n _ => 3\n};\nb"
        );
    }

    #[test]
    fn aliases_are_stripped() {
        assert_eq!(rewrite("orders::create(1)"), "create(1)");
        assert_eq!(rewrite("orders::create(user: 1)"), "create__kw(#{user: 1})");
        assert_eq!(rewrite("gen::int(1..5)"), "gen::int(1..5)");
    }

    #[test]
    fn source_map_points_back() {
        let src = r#"let u = createUser(name: "bob");"#;
        let aliases = HashMap::new();
        let defs = HashMap::new();
        let r = Rewriter {
            src,
            file: 0,
            aliases: &aliases,
            defs: &defs,
        };
        let mut out = CodeBuilder::new(Span::new(0, 0, 0));
        r.rewrite(0, src.len(), &mut out).unwrap();
        let gen_bob = out.text.find("\"bob\"").unwrap();
        assert_eq!(
            out.map_offset(gen_bob).unwrap().start,
            src.find("\"bob\"").unwrap()
        );
    }
}
