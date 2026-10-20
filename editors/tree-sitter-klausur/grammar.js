/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

// klausur files are Rhai plus requests, tests and a few additions to the expression syntax.
// This grammar extends tree-sitter-rhai (vendored in ./rhai) and adds:
//
// - `@name(params) modifiers { sections }` requests, with line-based `connection` blocks,
//   JSON `send` bodies with `{{…}}` interpolation, ``` fenced bodies and `undo`
// - `test "name" (params) cases … modifiers { … }`
// - `expect` / `assert`, `x is string`, `null`, named arguments `f(a: 1)` and parameter defaults

const rhai = require("./rhai/grammar");

const PREC_COMPARE = 5;

const commaSep = (rule) => optional(seq(rule, repeat(seq(",", rule)), optional(",")));

module.exports = grammar(rhai, {
  name: "klausur",

  rules: {
    // The root keeps Rhai's name: tree-sitter starts at the base grammar's first rule.
    Rhai: ($) => repeat(choice($.Request, $.Test, $.Stmt)),

    // --- Requests ------------------------------------------------------------------------

    Request: ($) =>
      seq(
        "@",
        field("name", $.ident),
        optional(field("params", $.DefParamList)),
        repeat(field("modifier", $.Modifier)),
        field("body", $.RequestBody),
      ),

    RequestBody: ($) => seq("{", repeat(choice($.Section, $.Undo, ";")), "}"),

    Section: ($) => choice($.Connection, $.Send, $.Expect, $.ExprDeclareVar),

    Connection: ($) => seq("connection", "{", $._connection_lines, "}"),

    _connection_lines: ($) => seq($.RequestLine, repeat($.Header)),

    RequestLine: ($) =>
      seq(field("method", choice($.method, $.Interp)), optional(field("url", $.Url))),

    Url: ($) => repeat1(choice($.text, $.Interp)),

    Header: ($) =>
      seq(field("name", $.header_name), optional(field("value", $.HeaderValue))),

    HeaderValue: ($) => repeat1(choice($.text, $.Interp)),

    Interp: ($) => seq("{{", field("expr", $.Expr), "}}"),

    Send: ($) =>
      seq(
        "send",
        choice(
          field("json", choice($.JsonObject, $.JsonArray)),
          field("raw", $.Fenced),
          seq("=", field("expr", $.Expr)),
        ),
      ),

    // `undo { … }` is always sections or a connection shorthand, so the call form is only a
    // call: `undo deleteUser(id)`.
    Undo: ($) =>
      seq(
        "undo",
        choice(
          field("body", $.UndoBlock),
          field("call", $.UndoCall),
        ),
      ),

    UndoCall: ($) => seq(field("fn_name", $.Path), field("args", $.ArgList)),

    UndoBlock: ($) =>
      seq("{", choice(repeat1($.Section), $._connection_lines), "}"),

    // --- Tests ---------------------------------------------------------------------------

    Test: ($) =>
      seq(
        "test",
        field("name", $.lit_str),
        optional(field("params", $.TestParamList)),
        optional(seq("cases", field("cases", $.Expr))),
        repeat(field("modifier", $.Modifier)),
        field("body", $.ExprBlock),
      ),

    TestParamList: ($) => seq("(", commaSep($.TestParam), ")"),

    TestParam: ($) =>
      seq(field("name", $.ident), optional(seq("in", field("values", $.Expr)))),

    Modifier: ($) => seq(field("name", $.ident), optional(field("args", $.ArgList))),

    // --- Additions to Rhai ---------------------------------------------------------------

    Expr: ($, original) => choice(original, $.Expect),

    // Lower than every operator, so `expect a == b` checks the whole comparison.
    Expect: ($) => prec.right(-1, seq(choice("expect", "assert"), field("condition", $.Expr))),

    ExprBinary: ($, original) =>
      choice(
        original,
        prec.left(
          PREC_COMPARE,
          seq(field("left", $.Expr), field("operator", alias("is", $.binop)), field("right", $.Expr)),
        ),
      ),

    Lit: ($, original) => choice(original, $.lit_null),
    lit_null: (_) => "null",

    ArgList: ($) => seq("(", field("args", commaSep(choice($.NamedArg, $.Expr))), ")"),

    // Names can be keywords: `poll(while: …, for: "10s")`.
    NamedArg: ($) =>
      seq(
        field("name", choice($.ident, alias(choice("while", "for", "in", "if", "do", "until", "loop"), $.ident))),
        ":",
        field("value", $.Expr),
      ),

    // `fn f(a, b = 1)`: parameters with defaults, for functions and requests.
    ExprFn: ($) =>
      seq(
        optional("private"),
        "fn",
        field("fn_name", $.FnDeclName),
        field("params", $.DefParamList),
        field("body", $.ExprBlock),
      ),

    DefParamList: ($) => seq("(", commaSep($.DefParam), ")"),

    DefParam: ($) =>
      seq(field("name", $.ident), optional(seq("=", field("default", $.Expr)))),

    // --- JSON bodies ---------------------------------------------------------------------

    JsonObject: ($) => seq("{", commaSep($.JsonPair), "}"),

    JsonPair: ($) =>
      seq(field("key", choice($.JsonString, $.Interp)), ":", field("value", $._json_value)),

    JsonArray: ($) => seq("[", commaSep($._json_value), "]"),

    _json_value: ($) =>
      choice(
        $.JsonObject,
        $.JsonArray,
        $.JsonString,
        $.json_number,
        alias("true", $.lit_bool),
        alias("false", $.lit_bool),
        $.lit_null,
        $.Interp,
      ),

    // `"id-{{x}}"`: interpolation inside a JSON string.
    JsonString: ($) =>
      seq(
        '"',
        repeat(choice($.json_string_content, alias(token.immediate("{"), $.json_string_content), $.Interp)),
        token.immediate('"'),
      ),

    json_string_content: (_) => token.immediate(prec(3, /([^"\\{]|\\.|\{[^{"])+/)),

    json_number: (_) => /-?\d+(\.\d+)?([eE][+-]?\d+)?/,

    // ``` … ```: raw text with `{{…}}`.
    Fenced: ($) => seq("```", repeat(choice($.fenced_text, $.Interp)), "```"),

    fenced_text: (_) => token(prec(3, /([^`{\s]|`[^`]|``[^`]|\{[^{])+/)),

    // --- Comments ------------------------------------------------------------------------

    // Above operators: in tree-sitter-rhai they're below, so `x // note` lexes `/` `/` and the
    // comment breaks the expression. Raw text (JSON strings, fenced bodies) ranks higher still.
    comment_line: (_) => token(prec(1, /\/\/[^\n]*/)),
    comment_block: (_) => token(prec(1, /\/\*[^*]*\*+(?:[^/*][^*]*\*+)*\//)),
    comment_line_doc: (_) => token(prec(2, /\/\/\/[^\n]*/)),
    comment_block_doc: (_) => token(prec(2, /\/\*\*[^*]*\*+(?:[^/*][^*]*\*+)*\//)),

    // --- Connection tokens ---------------------------------------------------------------

    method: (_) => token(prec(1, /[A-Z][A-Z0-9_-]*/)),

    // `Name: `, with the space, so `https://…` isn't mistaken for a header.
    header_name: (_) => token(prec(2, /[A-Za-z][A-Za-z0-9_-]*:[ \t]/)),

    // A word of a URL or header value. Below comments, so `// note` after a space is a comment,
    // while `https://…` stays text: a comment can't start in the middle of a word.
    text: (_) => token(prec(-2, /[^{}\s]+/)),
  },
});
