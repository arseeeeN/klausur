//! Parser for the structure of `.kl` files.

use super::scan::{
    self, ScanError, ScanResult, ident_at, is_ident_char, is_text_comment, skip_trivia, trim_range,
};
use super::template::{self, TemplateKind};
use super::*;
use crate::diag::{Diag, PResult, suggest};
use crate::source::{FileId, Span};

const HTTP_METHODS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "TRACE", "CONNECT",
];
const FUTURE_VERBS: &[(&str, &str)] = &[
    ("WS", "WebSocket"),
    ("GRPC", "gRPC"),
    ("SSE", "server-sent events"),
    ("WT", "WebTransport"),
];
const SECTIONS: &[&str] = &["connection", "send", "expect", "let", "undo"];
const TOP_LEVEL: &[&str] = &["let", "const", "fn", "test", "import"];

pub fn parse_file(file: FileId, src: &str) -> (File, Vec<Diag>) {
    let mut p = Parser { src, file, pos: 0 };
    let mut out = File::default();
    let mut diags = Vec::new();
    loop {
        match p.trivia() {
            Ok(()) => {}
            Err(d) => {
                diags.push(d);
                break;
            }
        }
        if p.eof() {
            break;
        }
        match p.item() {
            Ok(item) => out.items.push(item),
            Err(d) => {
                diags.push(d);
                p.recover();
            }
        }
    }
    (out, diags)
}

struct Parser<'a> {
    src: &'a str,
    file: FileId,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start, end)
    }

    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<u8> {
        self.src.as_bytes().get(self.pos).copied()
    }

    fn scan<T>(&self, r: ScanResult<T>) -> PResult<T> {
        r.map_err(|ScanError { pos, message }| Diag::at(self.span(pos, pos + 1), message))
    }

    fn trivia(&mut self) -> PResult<()> {
        self.pos = self.scan(skip_trivia(self.src, self.pos))?;
        Ok(())
    }

    fn inline_trivia(&mut self) -> PResult<()> {
        self.pos = self.scan(scan::skip_inline_trivia(self.src, self.pos))?;
        Ok(())
    }

    fn error_here(&self, message: impl Into<String>) -> Diag {
        let end = if self.eof() { self.pos } else { self.pos + 1 };
        Diag::at(self.span(self.pos, end), message)
    }

    /// Skip to the next line that starts with a top-level keyword.
    fn recover(&mut self) {
        let mut pos = self.pos;
        while let Some(i) = self.src[pos..].find('\n') {
            pos += i + 1;
            let rest = &self.src[pos..];
            let starts_item = rest.starts_with('@')
                || TOP_LEVEL.iter().any(|kw| {
                    rest.starts_with(kw)
                        && rest
                            .as_bytes()
                            .get(kw.len())
                            .is_some_and(|c| !is_ident_char(*c))
                });
            if starts_item {
                self.pos = pos;
                return;
            }
        }
        self.pos = self.src.len();
    }

    fn word(&self) -> Option<&'a str> {
        ident_at(self.src, self.pos).map(|(s, e)| &self.src[s..e])
    }

    fn ident(&mut self, what: &str) -> PResult<(String, Span)> {
        match ident_at(self.src, self.pos) {
            Some((s, e)) => {
                self.pos = e;
                Ok((self.src[s..e].to_string(), self.span(s, e)))
            }
            None => Err(self.error_here(format!("expected {what}"))),
        }
    }

    fn expect_byte(&mut self, c: u8, what: &str) -> PResult<usize> {
        if self.peek() == Some(c) {
            self.pos += 1;
            Ok(self.pos - 1)
        } else {
            Err(self.error_here(format!("expected {what}")))
        }
    }

    fn skip_semicolon(&mut self) -> PResult<()> {
        self.inline_trivia()?;
        if self.peek() == Some(b';') {
            self.pos += 1;
        }
        Ok(())
    }

    /// A Rhai expression from the current position to the end of the statement.
    fn expr(&mut self, what: &str) -> PResult<Span> {
        self.trivia()?;
        let start = self.pos;
        let end = self.scan(scan::stmt_end(self.src, start))?;
        let (s, e) = trim_range(self.src, start, end);
        if s == e {
            return Err(self.error_here(format!("expected {what}")));
        }
        self.pos = end;
        Ok(self.span(s, e))
    }

    fn string_literal(&mut self, what: &str) -> PResult<(String, Span)> {
        if self.peek() != Some(b'"') {
            return Err(self.error_here(format!("expected {what} as a string literal")));
        }
        let start = self.pos;
        let end = self.scan(scan::skip_string(self.src, start))?;
        self.pos = end;
        let raw = &self.src[start + 1..end - 1];
        let mut out = String::new();
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(c) => out.push(c),
                    None => {}
                }
            } else {
                out.push(c);
            }
        }
        Ok((out, self.span(start, end)))
    }

    fn item(&mut self) -> PResult<Item> {
        if self.peek() == Some(b'@') {
            return self.request().map(|r| Item::Request(Box::new(r)));
        }
        match self.word() {
            Some("let" | "const") => self.let_decl().map(Item::Let),
            Some("fn") => self.fn_decl().map(Item::Fn),
            Some("test") => self.test_decl().map(Item::Test),
            Some("import") => self.import_decl().map(Item::Import),
            other => {
                let mut d = self.error_here("expected a top-level item");
                if let Some(Some(s)) = other.map(|w| suggest(w, TOP_LEVEL.iter().copied())) {
                    d = d.help(format!("did you mean `{s}`?"));
                } else if self.src[self.pos..].starts_with("###")
                    || self.src[self.pos..].starts_with('#')
                {
                    d = d.help("comments are written with `//` or `/* */`, and requests start with `@name { … }`");
                } else {
                    d = d.help("top-level items are `let`, `fn`, `@request`, `test` and `import`");
                }
                Err(d)
            }
        }
    }

    fn let_decl(&mut self) -> PResult<LetDecl> {
        self.ident("`let`")?;
        self.trivia()?;
        let (name, name_span) = self.ident("a variable name")?;
        self.trivia()?;
        self.expect_byte(b'=', "`=`")?;
        let expr = self.expr("an expression")?;
        self.after_statement()?;
        Ok(LetDecl {
            name,
            name_span,
            expr,
        })
    }

    /// After a top-level statement: an optional `;`, and nothing that closes a block.
    fn after_statement(&mut self) -> PResult<()> {
        self.skip_semicolon()?;
        if matches!(self.peek(), Some(b'}' | b')' | b']')) {
            return Err(self.error_here("unexpected closing bracket"));
        }
        Ok(())
    }

    fn params(&mut self, allow_source: bool) -> PResult<Vec<(String, Span, Option<Span>)>> {
        let open = self.expect_byte(b'(', "`(`")?;
        let close = self.scan(scan::match_close(self.src, open))?;
        let mut params = Vec::new();
        for (s, e) in self.scan(split_top_level(self.src, open + 1, close))? {
            let (s, e) = trim_range(self.src, s, e);
            if s == e {
                continue;
            }
            let Some((ns, ne)) = ident_at(self.src, s) else {
                return Err(Diag::at(self.span(s, e), "expected a parameter name"));
            };
            let name = self.src[ns..ne].to_string();
            let name_span = self.span(ns, ne);
            let rest_start = self.scan(skip_trivia(self.src, ne))?;
            let rest = &self.src[rest_start..e];
            let extra = if rest.is_empty() {
                None
            } else if let Some(stripped) = rest.strip_prefix('=').filter(|_| !allow_source) {
                let ds = rest_start + 1 + (stripped.len() - stripped.trim_start().len());
                if ds >= e {
                    return Err(Diag::at(
                        self.span(rest_start, e),
                        "expected a default value after `=`",
                    ));
                }
                Some(self.span(ds, e))
            } else if allow_source
                && rest.starts_with("in")
                && !rest.as_bytes().get(2).is_some_and(|c| is_ident_char(*c))
            {
                let ss = self.scan(skip_trivia(self.src, rest_start + 2))?;
                if ss >= e {
                    return Err(Diag::at(
                        self.span(rest_start, e),
                        "expected a value source after `in`",
                    ));
                }
                Some(self.span(ss, e))
            } else {
                let what = if allow_source {
                    "`in <values>`, `,` or `)`"
                } else {
                    "`= <default>`, `,` or `)`"
                };
                return Err(Diag::at(
                    self.span(rest_start, e),
                    format!("expected {what}"),
                ));
            };
            if params
                .iter()
                .any(|(n, _, _): &(String, Span, Option<Span>)| *n == name)
            {
                return Err(Diag::at(name_span, format!("duplicate parameter `{name}`")));
            }
            params.push((name, name_span, extra));
        }
        self.pos = close + 1;
        Ok(params)
    }

    fn signature_params(&mut self) -> PResult<Vec<Param>> {
        let params: Vec<Param> = self
            .params(false)?
            .into_iter()
            .map(|(name, span, default)| Param {
                name,
                span,
                default,
            })
            .collect();
        let mut seen_default = None;
        for p in &params {
            match (&p.default, seen_default) {
                (Some(_), _) => seen_default = Some(p.span),
                (None, Some(d)) => {
                    return Err(Diag::at(
                        p.span,
                        format!("parameter `{}` needs a default value", p.name),
                    )
                    .label(d, "because this earlier parameter has one")
                    .help("parameters with default values must come last"));
                }
                _ => {}
            }
        }
        Ok(params)
    }

    fn fn_decl(&mut self) -> PResult<FnDecl> {
        self.ident("`fn`")?;
        self.trivia()?;
        let (name, name_span) = self.ident("a function name")?;
        self.trivia()?;
        let params = self.signature_params()?;
        self.trivia()?;
        if self.peek() != Some(b'{') {
            return Err(self.error_here("expected `{` to start the function body"));
        }
        let close = self.scan(scan::match_close(self.src, self.pos))?;
        let body = self.span(self.pos, close + 1);
        self.pos = close + 1;
        Ok(FnDecl {
            name,
            name_span,
            params,
            body,
        })
    }

    fn import_decl(&mut self) -> PResult<ImportDecl> {
        self.ident("`import`")?;
        self.trivia()?;
        let (path, path_span) = self.string_literal("the imported path")?;
        self.inline_trivia()?;
        let alias = if self.word() == Some("as") {
            self.pos += 2;
            self.trivia()?;
            Some(self.ident("a namespace name")?)
        } else {
            None
        };
        self.after_statement()?;
        Ok(ImportDecl {
            path,
            path_span,
            alias,
        })
    }

    /// Whitespace-separated terms such as modifiers: `rollback runs(200) as_user("admin")`.
    fn term(&mut self) -> PResult<Span> {
        let start = self.pos;
        let bytes = self.src.as_bytes();
        while let Some(&c) = bytes.get(self.pos) {
            match c {
                b'(' | b'[' => self.pos = self.scan(scan::match_close(self.src, self.pos))? + 1,
                b'#' if bytes.get(self.pos + 1) == Some(&b'{') => {
                    self.pos = self.scan(scan::match_close(self.src, self.pos + 1))? + 1;
                }
                b'"' | b'`' | b'\'' => {
                    self.pos = self.scan(scan::skip_string(self.src, self.pos))?
                }
                c if is_ident_char(c) || c == b':' || c == b'.' => self.pos += 1,
                _ => break,
            }
        }
        if self.pos == start {
            return Err(self.error_here("expected a modifier or `{`"));
        }
        Ok(self.span(start, self.pos))
    }

    fn modifiers(&mut self) -> PResult<Vec<Span>> {
        let mut mods = Vec::new();
        loop {
            self.trivia()?;
            if self.eof() || self.peek() == Some(b'{') {
                return Ok(mods);
            }
            if !self.peek().is_some_and(scan::is_ident_start) {
                return Err(self.error_here("expected a modifier or `{`"));
            }
            mods.push(self.term()?);
        }
    }

    fn test_decl(&mut self) -> PResult<TestDecl> {
        self.ident("`test`")?;
        self.trivia()?;
        let (name, name_span) = self.string_literal("the test name")?;
        self.trivia()?;
        let params = if self.peek() == Some(b'(') {
            self.params(true)?
                .into_iter()
                .map(|(name, span, source)| TestParam { name, span, source })
                .collect()
        } else {
            Vec::new()
        };
        self.trivia()?;
        let cases = if self.word() == Some("cases") {
            self.pos += 5;
            self.trivia()?;
            Some(self.term()?)
        } else {
            None
        };
        let modifiers = self.modifiers()?;
        if self.peek() != Some(b'{') {
            return Err(self.error_here("expected `{` to start the test body"));
        }
        let close = self.scan(scan::match_close(self.src, self.pos))?;
        let body = self.span(self.pos, close + 1);
        self.pos = close + 1;
        let needs_cases: Vec<&TestParam> = params.iter().filter(|p| p.source.is_none()).collect();
        if cases.is_none() {
            if let Some(p) = needs_cases.first() {
                return Err(Diag::at(p.span, format!("test parameter `{}` has no values", p.name))
                    .help(format!("give it values with `{} in [...]`, or add `cases [...]` after the parameter list", p.name)));
            }
        } else if let (Some(cases), true) = (cases, needs_cases.is_empty()) {
            return Err(Diag::at(
                cases,
                "`cases` given, but every parameter already has `in` values",
            )
            .help("list the parameters that come from `cases` without `in`"));
        }
        Ok(TestDecl {
            name,
            name_span,
            params,
            cases,
            modifiers,
            body,
        })
    }

    fn request(&mut self) -> PResult<RequestDecl> {
        self.pos += 1;
        let (name, name_span) = self.ident("a request name after `@`")?;
        self.inline_trivia()?;
        let params = if self.peek() == Some(b'(') {
            self.signature_params()?
        } else {
            Vec::new()
        };
        let modifiers = self.modifiers()?;
        if self.peek() != Some(b'{') {
            return Err(self.error_here("expected `{` to start the request block"));
        }
        let body = self.request_body(true)?;
        Ok(RequestDecl {
            name,
            name_span,
            params,
            modifiers,
            body,
        })
    }

    /// Parse `{ sections }` starting at the `{`. Leaves `pos` after the closing `}`.
    fn request_body(&mut self, allow_undo: bool) -> PResult<RequestBody> {
        let open = self.pos;
        self.pos += 1;
        let mut connection: Option<Connection> = None;
        let mut send: Option<(Send, Span)> = None;
        let mut expect: Option<(Vec<Span>, Span)> = None;
        let mut captures: Vec<Capture> = Vec::new();
        let mut undo: Option<(Undo, Span)> = None;
        loop {
            self.trivia()?;
            match self.peek() {
                None => {
                    return Err(Diag::at(
                        self.span(open, open + 1),
                        "unclosed request block",
                    ));
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(b';') => {
                    self.pos += 1;
                    continue;
                }
                _ => {}
            }
            let kw_start = self.pos;
            let Some(kw) = self.word() else {
                return Err(self.error_here(
                    "expected a section (`connection`, `send`, `expect`, `let` or `undo`)",
                ));
            };
            let kw_span = self.span(kw_start, kw_start + kw.len());
            let dup = |first: Span| {
                Diag::at(kw_span, format!("duplicate `{kw}` section"))
                    .label(first, "first defined here")
                    .help("a request block describes exactly one exchange, so each section can appear only once; chain several requests in a `fn` instead")
            };
            match kw {
                "connection" => {
                    if let Some(c) = &connection {
                        return Err(dup(c.span));
                    }
                    self.pos += kw.len();
                    self.trivia()?;
                    if self.peek() != Some(b'{') {
                        return Err(self.error_here("expected `{` after `connection`"));
                    }
                    let open = self.pos;
                    let close = self.scan(scan::match_close_text(self.src, open))?;
                    let mut c = self.connection(open + 1, close)?;
                    c.span = kw_span;
                    connection = Some(c);
                    self.pos = close + 1;
                }
                "send" => {
                    if let Some((_, s)) = &send {
                        return Err(dup(*s));
                    }
                    self.pos += kw.len();
                    self.inline_trivia()?;
                    send = Some((self.send()?, kw_span));
                }
                "expect" => {
                    if let Some((_, s)) = &expect {
                        return Err(dup(*s));
                    }
                    self.pos += kw.len();
                    self.inline_trivia()?;
                    let exprs = if self.peek() == Some(b'{') {
                        self.expr_block()?
                    } else {
                        vec![self.expr("an assertion after `expect`")?]
                    };
                    expect = Some((exprs, kw_span));
                }
                "let" => {
                    self.pos += kw.len();
                    self.trivia()?;
                    let (name, name_span) = self.ident("a capture name")?;
                    if let Some(c) = captures.iter().find(|c| c.name == name) {
                        return Err(Diag::at(name_span, format!("duplicate capture `{name}`"))
                            .label(c.name_span, "first defined here"));
                    }
                    self.trivia()?;
                    self.expect_byte(b'=', "`=`")?;
                    let expr = self.expr("an expression")?;
                    captures.push(Capture {
                        name,
                        name_span,
                        expr,
                    });
                }
                "undo" => {
                    if !allow_undo {
                        return Err(Diag::at(
                            kw_span,
                            "an `undo` action can't have its own `undo`",
                        ));
                    }
                    if let Some((_, s)) = &undo {
                        return Err(dup(*s));
                    }
                    self.pos += kw.len();
                    self.inline_trivia()?;
                    undo = Some((self.undo()?, kw_span));
                }
                other => {
                    let mut d = Diag::at(kw_span, format!("unknown section `{other}`"));
                    d = match suggest(other, SECTIONS.iter().copied()) {
                        Some(s) => d.help(format!("did you mean `{s}`?")),
                        None => d.help("request blocks contain `connection`, `send`, `expect`, `let` and `undo`"),
                    };
                    return Err(d);
                }
            }
            self.skip_semicolon()?;
        }
        let span = self.span(open, self.pos);
        let Some(connection) = connection else {
            return Err(Diag::at(
                self.span(open, open + 1),
                "request block has no `connection`",
            )
            .help("add `connection { GET https://… }`"));
        };
        Ok(RequestBody {
            connection,
            send: send.map(|(s, _)| s),
            expect: expect.map(|(e, _)| e).unwrap_or_default(),
            captures,
            undo: undo.map(|(u, _)| Box::new(u)),
            span,
        })
    }

    /// `{ expr \n expr ... }` (code mode), one span per expression.
    fn expr_block(&mut self) -> PResult<Vec<Span>> {
        let open = self.pos;
        let close = self.scan(scan::match_close(self.src, open))?;
        let mut exprs = Vec::new();
        let mut pos = open + 1;
        loop {
            pos = self.scan(skip_trivia(self.src, pos))?;
            if pos >= close {
                break;
            }
            if self.src.as_bytes()[pos] == b';' {
                pos += 1;
                continue;
            }
            let end = self.scan(scan::stmt_end(self.src, pos))?.min(close);
            let (s, e) = trim_range(self.src, pos, end);
            if s == e {
                return Err(Diag::at(
                    self.span(pos, pos + 1),
                    "unexpected character in `expect` block",
                ));
            }
            exprs.push(self.span(s, e));
            pos = end;
        }
        self.pos = close + 1;
        Ok(exprs)
    }

    fn send(&mut self) -> PResult<Send> {
        match self.peek() {
            Some(b'{' | b'[') => {
                let open = self.pos;
                let close = self.scan(scan::match_close_json(self.src, open))?;
                let t = self.scan(template::parse(self.src, self.file, open, close + 1, TemplateKind::Json))?;
                self.pos = close + 1;
                Ok(Send::Json(t))
            }
            Some(b'`') if self.src[self.pos..].starts_with("```") => self.fenced_body().map(Send::Raw),
            Some(b'=') => {
                self.pos += 1;
                Ok(Send::Expr(self.expr("an expression after `send =`")?))
            }
            _ => Err(self.error_here("expected a JSON body (`{`/`[`), a ``` fenced body, or `= <expression>` after `send`")),
        }
    }

    /// A ``` fenced raw body. Lines are dedented by the indentation of the closing fence.
    fn fenced_body(&mut self) -> PResult<Template> {
        let fence_start = self.pos;
        let line_end = self.src[self.pos..]
            .find('\n')
            .map_or(self.src.len(), |i| self.pos + i);
        let content_start = (line_end + 1).min(self.src.len());
        let mut lines = Vec::new();
        let mut pos = content_start;
        let close = loop {
            if pos >= self.src.len() {
                return Err(Diag::at(
                    self.span(fence_start, fence_start + 3),
                    "unclosed ``` body",
                ));
            }
            let end = self.src[pos..]
                .find('\n')
                .map_or(self.src.len(), |i| pos + i);
            let line = &self.src[pos..end];
            if line.trim() == "```" {
                break (pos, end, line.len() - line.trim_start().len());
            }
            lines.push((pos, end));
            pos = end + 1;
        };
        let (close_start, close_end, indent) = close;
        let mut parts = Vec::new();
        for (i, (s, e)) in lines.iter().enumerate() {
            if i > 0 {
                template::push_lit(&mut parts, "\n");
            }
            let line = &self.src[*s..*e];
            let strip = line.len() - line.trim_start().len();
            let s = s + strip.min(indent);
            self.scan(template::parse_into(
                self.src,
                self.file,
                s,
                *e,
                TemplateKind::Text,
                &mut parts,
                &mut false,
            ))?;
        }
        self.pos = (close_start + indent + 3).min(close_end);
        Ok(Template {
            parts,
            span: self.span(fence_start, close_end),
        })
    }

    fn undo(&mut self) -> PResult<Undo> {
        if self.peek() != Some(b'{') {
            return Ok(Undo::Call(self.expr("an undo action")?));
        }
        let inner = self.scan(skip_trivia(self.src, self.pos + 1))?;
        let first = ident_at(self.src, inner).map(|(s, e)| &self.src[s..e]);
        if first.is_some_and(|w| SECTIONS.contains(&w)) {
            return self.request_body(false).map(|b| Undo::Request(Box::new(b)));
        }
        // Shorthand: the block is the connection text.
        let open = self.pos;
        let close = self.scan(scan::match_close_text(self.src, open))?;
        let connection = self.connection(open + 1, close)?;
        self.pos = close + 1;
        Ok(Undo::Request(Box::new(RequestBody {
            connection,
            send: None,
            expect: Vec::new(),
            captures: Vec::new(),
            undo: None,
            span: self.span(open, close + 1),
        })))
    }

    /// Parse the text of a connection block: a protocol line plus headers.
    fn connection(&self, start: usize, end: usize) -> PResult<Connection> {
        let mut lines = Vec::new();
        let mut pos = start;
        while pos < end {
            let line_end = self.src[pos..end].find('\n').map_or(end, |i| pos + i);
            let content_end = self.scan(text_line_end(self.src, pos, line_end))?;
            let (s, e) = trim_range(self.src, pos, content_end);
            if s < e {
                lines.push((s, e));
            }
            pos = line_end + 1;
        }
        let Some(&(ls, le)) = lines.first() else {
            return Err(Diag::at(
                self.span(start.saturating_sub(1), start),
                "empty `connection` block",
            )
            .help("start it with a method and URL, e.g. `GET https://example.com`"));
        };
        let mut verb_template = None;
        let (vs, ve) = if self.src[ls..le].starts_with("{{") {
            // A templated method: `{{method}} https://…`, the whole first word.
            let close = self.scan(scan::scan_interp(self.src, ls + 2))?;
            let end = close + 2;
            if end < le && !self.src.as_bytes()[end].is_ascii_whitespace() {
                return Err(Diag::at(
                    self.span(ls, end + 1),
                    "a templated method must be the whole word",
                )
                .help("write `{{method}} https://…`, with the method in one interpolation"));
            }
            verb_template = Some(self.scan(template::parse(
                self.src,
                self.file,
                ls,
                end,
                TemplateKind::Text,
            ))?);
            (ls, end)
        } else {
            ident_at(self.src, ls).unwrap_or((ls, ls))
        };
        let verb = &self.src[vs..ve];
        let verb_span = self.span(vs, ve.max(vs + 1));
        if verb_template.is_none() && self.src[ve..le].starts_with("{{") {
            return Err(Diag::at(
                self.span(vs, ve + 2),
                "a templated method must be the whole word",
            )
            .help("write `{{method}} https://…`, with the method in one interpolation"));
        }
        if verb_template.is_none() && !HTTP_METHODS.contains(&verb) {
            if let Some((_, proto)) = FUTURE_VERBS.iter().find(|(v, _)| *v == verb) {
                return Err(Diag::at(
                    verb_span,
                    format!("{proto} connections aren't supported yet"),
                )
                .help("only HTTP methods work in this version"));
            }
            let mut d = Diag::at(verb_span, "expected an HTTP method");
            let upper = verb.to_ascii_uppercase();
            if HTTP_METHODS.contains(&upper.as_str()) {
                d = d.help(format!("methods are uppercase: `{upper}`"));
            } else if let Some(s) = suggest(&upper, HTTP_METHODS.iter().copied()) {
                d = d.help(format!("did you mean `{s}`?"));
            }
            return Err(d);
        }
        let (us, ue) = trim_range(self.src, ve, le);
        if us == ue {
            return Err(Diag::at(verb_span, format!("`{verb}` needs a URL")));
        }
        let url = self.scan(template::parse(
            self.src,
            self.file,
            us,
            ue,
            TemplateKind::Url,
        ))?;
        let mut headers = Vec::new();
        for &(s, e) in &lines[1..] {
            let line = &self.src[s..e];
            let Some(colon) = line.find(':') else {
                return Err(Diag::at(self.span(s, e), "expected a header `Name: value`"));
            };
            let name = line[..colon].trim();
            let valid = !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.!#$%&'*+^`|~".contains(&c));
            if !valid {
                return Err(
                    Diag::at(self.span(s, s + colon), "expected a header `Name: value`")
                        .help("header names can't contain spaces or special characters"),
                );
            }
            let (vs, ve) = trim_range(self.src, s + colon + 1, e);
            let value = self.scan(template::parse(
                self.src,
                self.file,
                vs,
                ve,
                TemplateKind::Text,
            ))?;
            headers.push(Header {
                name: name.to_string(),
                value,
            });
        }
        Ok(Connection {
            verb: verb.to_string(),
            verb_template,
            url,
            headers,
            span: self.span(start, end),
        })
    }
}

/// End of the content of a request-text line, before any `//` comment.
fn text_line_end(src: &str, start: usize, end: usize) -> ScanResult<usize> {
    let mut pos = start;
    while pos < end {
        if src[pos..].starts_with("{{") {
            pos = scan::scan_interp(src, pos + 2)? + 2;
            continue;
        }
        if is_text_comment(src, pos) {
            return Ok(pos);
        }
        pos += 1;
    }
    Ok(end)
}

/// Split `src[start..end]` at top-level commas (code mode).
pub fn split_top_level(src: &str, start: usize, end: usize) -> ScanResult<Vec<(usize, usize)>> {
    let mut parts = Vec::new();
    let mut pos = start;
    let mut part_start = start;
    while pos < end {
        match src.as_bytes()[pos] {
            b'(' | b'[' | b'{' => pos = scan::match_close(src, pos)? + 1,
            b'"' | b'`' | b'\'' => pos = scan::skip_string(src, pos)?,
            b'/' if matches!(src.as_bytes().get(pos + 1), Some(b'/' | b'*')) => {
                pos = skip_trivia(src, pos)?
            }
            b',' => {
                parts.push((part_start, pos));
                pos += 1;
                part_start = pos;
            }
            _ => pos += 1,
        }
    }
    parts.push((part_start, end));
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(src: &str) -> File {
        let (file, diags) = parse_file(0, src);
        assert!(
            diags.is_empty(),
            "unexpected errors: {:?}",
            diags.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
        file
    }

    fn first_error(src: &str) -> String {
        let (_, diags) = parse_file(0, src);
        diags.first().expect("expected an error").message.clone()
    }

    const USERS: &str = r#"
let base = "https://api.example.com"  // comment

@createUser(name = "alice", role = "user") {
    connection {
        POST {{base}}/users   // trailing comment
        Authorization: Bearer {{token}}
    }

    send { "name": {{name}}, "role": {{role}} }

    expect {
        status == 201
        body.id is string
    }
    let id = body.id

    undo {
        DELETE {{base}}/users/{{id}}
    }
}

@getUser(id) {
    connection { GET {{base}}/users/{{id}} }
    expect status == 200
}

fn onboard(name) {
    let u = createUser(name: name);
    u
}

test "login" (user, pass, ok) cases [
    #{ user: "alice", pass: "secret", ok: true },
] rollback as_user("admin") {
    expect login(user: user, pass: pass).status == if ok { 200 } else { 401 }
}

test "roles" (role in ["user", "admin"]) {
    expect true
}
"#;

    #[test]
    fn parses_a_full_file() {
        let file = parse_ok(USERS);
        assert_eq!(file.items.len(), 6);
        let Item::Request(r) = &file.items[1] else {
            panic!()
        };
        assert_eq!(r.name, "createUser");
        assert_eq!(r.params.len(), 2);
        assert_eq!(r.body.connection.verb, "POST");
        assert_eq!(r.body.connection.headers.len(), 1);
        assert_eq!(r.body.expect.len(), 2);
        assert_eq!(r.body.captures.len(), 1);
        assert!(matches!(r.body.undo.as_deref(), Some(Undo::Request(_))));
        let Item::Test(t) = &file.items[4] else {
            panic!()
        };
        assert_eq!(t.params.len(), 3);
        assert!(t.cases.is_some());
        assert_eq!(t.modifiers.len(), 2);
    }

    #[test]
    fn duplicate_sections_are_errors() {
        let msg = first_error("@a { connection { GET http://x } send {} send {} }");
        assert_eq!(msg, "duplicate `send` section");
    }

    #[test]
    fn missing_connection_is_an_error() {
        assert_eq!(
            first_error("@a { expect status == 200 }"),
            "request block has no `connection`"
        );
    }

    #[test]
    fn unknown_sections_get_suggestions() {
        let (_, diags) = parse_file(0, "@a { connection { GET http://x } expcet true }");
        assert_eq!(diags[0].help.as_deref(), Some("did you mean `expect`?"));
    }

    #[test]
    fn recovers_after_errors() {
        let (file, diags) = parse_file(
            0,
            "@a { nope }\nlet x = 1\n@b { connection { GET http://x } }",
        );
        assert_eq!(diags.len(), 1);
        assert_eq!(file.items.len(), 2);
    }

    #[test]
    fn fenced_bodies_are_dedented() {
        let src = "@a {\n    connection { POST http://x }\n    send ```\n        line {{x}}\n          more\n        ```\n}";
        let file = parse_ok(src);
        let Item::Request(r) = &file.items[0] else {
            panic!()
        };
        let Some(Send::Raw(t)) = &r.body.send else {
            panic!()
        };
        assert_eq!(t.display(src), "line {{x}}\n  more");
    }

    #[test]
    fn test_params_need_values() {
        assert_eq!(
            first_error("test \"x\" (a) { }"),
            "test parameter `a` has no values"
        );
    }
}
