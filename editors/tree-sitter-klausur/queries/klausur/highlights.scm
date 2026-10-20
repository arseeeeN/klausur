; klausur highlights. Rhai's parts follow tree-sitter-rhai's queries, with Neovim's capture names.
; Later patterns win, so general ones come first.

(ident) @variable

; What `expect`, `let` and `undo` see in a request block, and the test context.
((ident) @variable.builtin
  (#any-of? @variable.builtin "status" "headers" "body" "raw" "duration" "events" "ctx" "this"))

; --- Keywords -----------------------------------------------------------------------------

[ "let" "const" "export" "private" ] @keyword
"fn" @keyword.function
[ "import" "as" ] @keyword.import
[ "if" "else" "switch" ] @keyword.conditional
[ "loop" "while" "for" "do" "until" "break" ] @keyword.repeat
(ExprContinue) @keyword.repeat
"return" @keyword.return
[ "try" "catch" "throw" ] @keyword.exception
"in" @keyword.operator

[ "test" "cases" ] @keyword
[ "connection" "send" "undo" ] @keyword
[ "expect" "assert" ] @keyword

((binop) @keyword.operator
  (#any-of? @keyword.operator "in" "is"))
((binop) @operator
  (#not-any-of? @operator "in" "is"))
"=" @operator

; `x is string`
(ExprBinary
  operator: (binop) @_op
  right: (Expr (ExprPath (Path (ident) @type.builtin)))
  (#eq? @_op "is"))

; --- Literals -----------------------------------------------------------------------------

(lit_bool) @boolean
(lit_null) @constant.builtin
(lit_str) @string
(lit_char) @character
[ (lit_int) (lit_float) (json_number) ] @number
(str_template_expr [ "${" "}" ] @punctuation.special)

[ (comment_line) (comment_block) ] @comment @spell
[ (comment_line_doc) (comment_block_doc) ] @comment.documentation @spell

; --- Names --------------------------------------------------------------------------------

(Path (ident) @module . "::")

(ExprCall fn_name: (Expr (ExprPath (Path (ident) @function.call .))))
(UndoCall fn_name: (Path (ident) @function.call .))

(ExprDotAccess [ "." "?." ] (Expr (ExprPath (Path (ident) @variable.member))))
(ExprDotAccess
  [ "." "?." ]
  (Expr (ExprCall fn_name: (Expr (ExprPath (Path (ident) @function.method.call .))))))

(ObjectField key: (_) @property)

(ExprFn fn_name: (FnDeclName (ident) @function))
(DefParam name: (ident) @variable.parameter)
(Param (ident) @variable.parameter)
(TestParam name: (ident) @variable.parameter)
(NamedArg name: (ident) @variable.parameter)

; --- Requests and tests -------------------------------------------------------------------

(Request "@" @function name: (ident) @function)
(Modifier name: (ident) @attribute)
(Test name: (lit_str) @string.special)

(method) @function.method
(RequestLine method: (Interp) @function.method)
(Url (text) @string.special.url)
(header_name) @constant
(HeaderValue (text) @string)
(Interp [ "{{" "}}" ] @punctuation.special)

(JsonString) @string
(JsonPair key: (JsonString) @property)
(Fenced "```" @punctuation.special)
(fenced_text) @string

; --- Punctuation --------------------------------------------------------------------------

[ "(" ")" "{" "}" "[" "]" "#{" "|" ] @punctuation.bracket
[ "," ";" ":" "::" "." "?." "=>" ] @punctuation.delimiter
