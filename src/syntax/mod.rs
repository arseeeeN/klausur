//! The klausur file format: top-level items, request blocks and templates.
//!
//! klausur parses the structure itself; code fragments (expressions, bodies) are kept as
//! spans and compiled by Rhai later.

pub mod parser;
pub mod scan;
pub mod template;

use crate::source::Span;
pub use template::{InterpMode, Part, Template};

#[derive(Debug, Default)]
pub struct File {
    pub items: Vec<Item>,
}

#[derive(Debug)]
pub enum Item {
    Let(LetDecl),
    Fn(FnDecl),
    Request(RequestDecl),
    Test(TestDecl),
    Import(ImportDecl),
}

#[derive(Debug, Clone)]
pub struct LetDecl {
    pub name: String,
    pub name_span: Span,
    pub expr: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: String,
    pub span: Span,
    pub default: Option<Span>,
}

#[derive(Debug, Clone)]
pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    /// The body including its braces.
    pub body: Span,
}

#[derive(Debug, Clone)]
pub struct RequestDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    /// `@name(params) timeout("5s") { … }`
    pub modifiers: Vec<Span>,
    pub body: RequestBody,
}

#[derive(Debug, Clone)]
pub struct RequestBody {
    pub connection: Connection,
    pub send: Option<Send>,
    /// One span per assertion expression.
    pub expect: Vec<Span>,
    pub captures: Vec<Capture>,
    pub undo: Option<Box<Undo>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Connection {
    pub verb: String,
    pub url: Template,
    pub headers: Vec<Header>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Header {
    pub name: String,
    pub value: Template,
}

#[derive(Debug, Clone)]
pub enum Send {
    Json(Template),
    Raw(Template),
    Expr(Span),
}

#[derive(Debug, Clone)]
pub struct Capture {
    pub name: String,
    pub name_span: Span,
    pub expr: Span,
}

#[derive(Debug, Clone)]
pub enum Undo {
    Request(Box<RequestBody>),
    Call(Span),
}

#[derive(Debug, Clone)]
pub struct TestDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<TestParam>,
    pub cases: Option<Span>,
    pub modifiers: Vec<Span>,
    /// The body including its braces.
    pub body: Span,
}

#[derive(Debug, Clone)]
pub struct TestParam {
    pub name: String,
    pub span: Span,
    /// `name in <source>`; `None` means the value comes from `cases`.
    pub source: Option<Span>,
}

#[derive(Debug, Clone)]
pub struct ImportDecl {
    pub path: String,
    pub path_span: Span,
    pub alias: Option<(String, Span)>,
}
