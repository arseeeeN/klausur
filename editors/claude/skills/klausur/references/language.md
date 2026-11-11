# klausur language reference

Everything needed to write correct `.kl` code. The full spec is
https://raw.githubusercontent.com/arseeeeN/klausur/main/SPEC.md.

Contents: [Files](#files) · [Request blocks](#request-blocks) · [Calling requests](#calling-requests)
· [`expect`](#expect) · [Rhai essentials](#rhai-essentials) · [Tests](#tests) ·
[Modifiers](#modifiers) · [Undo and scopes](#undo-and-scopes) · [Built-ins](#built-ins) ·
[CLI](#cli) · [Pitfalls](#pitfalls)

## Files

A file is a sequence of top-level items, in any order:

```
import "api.kl"                         // relative to this file; each file loads once
import "orders.kl" as orders            // `orders::create(…)` checks the name is defined there
let base = env("API_URL", "http://localhost:8080")   // lazy: evaluated on first use
const ADMIN = "admin"
fn helper(x, y = 2) { x + y }           // Rhai function, defaults and named args allowed
@createUser(name, role = "user") { … }  // request block
test "a user can be created" rollback { … }
```

All loaded files share **one namespace**: a name defined in two files is a load error. Put
shared helpers in one file (usually `lib.kl`) that the others import.

Semicolons are optional: a newline ends a statement unless the line obviously continues (ends
with an operator or `,`, or the next line starts with `.`, `&&`, `||`, `else`, `catch` or a
closing bracket). `//` and `/* */` comments. `null` is Rhai's `()` and JSON's `null`.

## Request blocks

```
@createUser(name, role = "user", jwt = ()) timeout("10s") {
    connection {
        POST {{base}}/users?notify={{notify}}
        Authorization: Bearer {{jwt}}
        X-Request-Id: {{uuid()}}
    }
    send { "name": {{name}}, "role": {{role}}, "tags": ["new"] }
    expect {
        status == 201
        body.id is int
        headers["content-type"].contains("json")
    }
    let id = body.id
    undo {
        DELETE {{base}}/users/{{id}}
        Authorization: Bearer {{jwt}}
    }
}
```

Sections appear at most once, in any order; they always run in this order: connection → send →
response → `expect` → `let` captures → register `undo` (only if `expect` passed).

**`connection`**: the method and URL, then `Name: value` headers. The method can be
interpolated as the whole first word: `{{method}} {{base}}/x`. Redirects aren't followed.

**Interpolation `{{expr}}`** (any expression; parameters and top-level `let`s are in scope):
- URL: the first interpolation is inserted verbatim (`{{base}}/users`); later ones are
  percent-encoded as one URL component, so a `/` becomes `%2F`. For a value that is itself a
  path, use `url_path(x)` (0.3+): segments encoded, slashes kept,
  `{{base}}/nodes{{url_path("/my site/news")}}` → `/nodes/my%20site/news`. `raw(x)` inserts
  verbatim (on 0.2, `raw` is the way to keep slashes).
- Headers: as text. **A header whose interpolation is null isn't sent**, so `jwt = ()` means
  "no Authorization header".
- JSON body: as JSON (`{{name}}` → `"alice"`, `{{tags}}` → `["a"]`, maps → objects). Inside a JSON
  string, `"id-{{x}}"` inserts escaped text. A null value is sent as `null`; the field isn't left
  out. To leave optional fields out (and so test the service's defaults), build the body in a
  function and use `send = …`:

  ```
  fn user_body(name, role) {
      let b = #{ name: name }
      if role != () { b.role = role }
      b
  }
  @createUser(name, role = ()) {
      connection { POST {{base}}/users }
      send = user_body(name, role)
      …
  }
  ```

**`send`** forms:
- `send { … }` / `send [ … ]`: JSON with interpolation (`Content-Type: application/json` unless set).
- ```` send ``` … ``` ````: raw text with `{{…}}` as text, dedented to the closing fence.
- `send = expr`: computed. Map/array → JSON; string → as is; blob → bytes;
  `multipart(#{ field: "x", doc: file("a.pdf") })` → `multipart/form-data`; `file(path)` → raw
  bytes with a guessed content type. `send = if a == () { #{…} } else { #{…} }` works.

**`expect`**: one expression, or a block with one per line. In scope: `status`, `headers`
(lowercase names), `body` (parsed JSON, or a string), `raw` (blob), `duration` (ms), `events`
(parsed SSE for `text/event-stream`), and the parameters.

**`let name = expr`**: captures, same scope as `expect`; they become fields of the result.

**`undo`**: how to reverse the request; captured values and parameters are frozen when it's
registered. Three forms:
- `undo { DELETE {{base}}/users/{{id}} }`: an inline connection,
- `undo { connection { … } expect … }`: a full request,
- `undo deleteUser(jwt, id)`: a call to a request or function.

An undo succeeds on 2xx, 404 or 410 unless it has its own `expect`. So a test may delete what
it created (to test deletion) and the registered undo still succeeds afterwards. Transient failures (connection
errors, 429, 5xx) are retried 3 times.

**Request modifiers** (between parameters and `{`, evaluated per call):
- `timeout("90s")`: per-request timeout (default 30 s, or `--timeout`).
- `poll(until: body.state == "done", every: "1s", within: "60s")`: repeat until the condition
  holds (`until` sees the response like `expect`; can be a closure `|r| r.status == 200`). No
  response counts as "not yet". `expect`/`let`/`undo` run on the final response.
- `poll(while: body.total == 0, every: "1s", for: "15s")`: check something *keeps* holding for
  the whole time; fails on the first response where it doesn't.

## Calling requests

Requests are called like functions, positionally or with named arguments:

```
let u = createUser("bob")
let u = createUser(name: "bob", role: "admin")
let u = createUser("bob", jwt: token)          // positional then named
```

The result has `status`, `headers`, `body`, `raw`, `duration`, `request` (what was sent), and
every capture (`u.id`). If `expect` fails, the call raises an error that fails the test. To
handle it: `try { createUser("x") } catch (e) { log(e) }` (`try` is a statement, not an
expression).

## `expect`

`expect expr` (alias `assert`) works anywhere: in request blocks, tests, functions, hooks. The
expression must be `true`. Failures explain themselves:
- comparisons show both sides: `expectation failed: 404 == 200`,
- other expressions list their parts' values under `where:`, and `xs.all(|x| …)` /
  `xs.some(|x| …)` list the elements that broke it,
- each part is evaluated once (a request inside an `expect` is sent once).

`x is <type>` checks a type: `string`, `int`, `float`, `number`, `bool`, `array`, `map`, `blob`,
`null`.

## Rhai essentials

```
let x = 1;  x += 1                      // variables; `const` for constants
let s = "plain"; let t = `x is ${x}`    // only backticks interpolate
let m = #{ a: 1, "b-c": 2 }; m.a; m["b-c"]; m.d = 3; m.keys(); "a" in m
let a = [1, 2, 3]; a[0]; a.len(); a.push(4); 2 in a
if x > 1 { "big" } else { "small" }     // if is an expression
switch x { 1 => "one", 2 | 3 => "few", _ => "many" }
for item in a { … }   for i in 0..10 { … }   while c { … }   loop { break; }
let f = |x| x * 2;  f.call(3)           // closures capture by value
fn name(a, b = 2) { a + b }             // last expression is the return value
```

- Arrays: `map`, `filter`, `find` (element or `()`), `index_of`, `some`, `all`, `reduce(|acc, x| …, init)`,
  `contains`, `len`, `is_empty`, `sorted()`, `sorted(|a, b| …)`, `reversed()`.
- Strings: `contains`, `starts_with`, `ends_with`, `to_lower`, `to_upper`, `sub_string(start, len)`,
  `split(sep)`, `index_of`, `len`, `trimmed()`, `parse_int(s)`, `parse_float(s)`.
- Maps: `keys`, `values`, `contains`, `to_json()`; `parse_json(s)` parses JSON text.
- A missing map key is `()`, not an error. Integer division truncates (`7 / 2 == 3`).
- `${map}` and `print`/`log` show maps and arrays as JSON.

## Tests

```
test "name, as a behaviour" (params) modifiers { body }
```

Parameter sources, one case per value or row:
- `(role in ["user", "admin"])`; several `in` parameters → every combination; ranges work (`n in 1..5`).
- `(user, pass, ok) cases [ #{ user: "a", pass: "x", ok: false }, … ]`; also
  `cases csv("rows.csv")`, `cases json("rows.json")`.
- Generators: `(name in gen::string(1..64))`, 25 samples per combination unless `runs(n)`.
  `gen::int(range)`, `float(range)`, `string(len)`, `email()`, `uuid()`, `one_of(a, b, …)`,
  `array(gen, len)`, `map(#{ k: gen })`, `gen::custom(|rng| …)`. A failure prints the seed;
  `--seed N` replays it.

In the body, `ctx` is the case context: `ctx.args`, `ctx.test`, and anything hooks stored
(`ctx.jwt`). `ctx.headers["X"] = "y"` adds a header to every request of the case.

## Modifiers

Between the parameters and `{`, applied outside-in (the first wraps the others):

| Built-in | |
|---|---|
| `rollback` | Run the case in an undo scope: everything it registered is undone at the end |
| `timeout("30s")` | Whole case, hooks included |
| `retries(n)` | Up to n extra attempts |
| `runs(n)` | Samples for generator parameters |
| `seed(n)` | Fixed seed for this test |
| `tags(["slow", "ui"])` | An **array**; `--tag ui` runs only tagged tests |
| `skip("reason")` | Report as skipped |
| `only` | Run only `only` tests |
| `label(expr)` | Case name in the report, e.g. `label(name)` or ``label(`${role} on ${path}`)`` |
| `parallel` | May overlap with other `parallel` cases (`-j N`, default 4) |
| `parallel("llm", 1)` (0.4+) | Parallel, but at most 1 case of the group `llm` at a time (for a slow or rate-limited resource) |
| `cookies` (0.5+) | A cookie jar per case: requests (hooks and undos too) keep `Set-Cookie` and send the cookies back. Read with `ctx.cookies.sid`, add with `set_cookie(url, "sid=…; Path=/")`. A request's own `Cookie:` header wins |

A user modifier is a function called on the test definition (`this`):

```
fn logged_in(user = "admin") {
    this.before(|ctx| { ctx.jwt = login(user).token })   // per case, before the body
}
fn log_answer() {
    this.after(|ctx, result| { if !result.passed { ctx.log(`answer: ${ctx.answer}`) } })
}
```

Hooks: `this.before(|ctx| …)`, `this.after(|ctx, result| …)` (`result.passed`, `result.error`),
`this.around(|ctx, run| { …; run.call() })`, `this.on_request(|req| { req.headers["X"] = "y"; req })`,
`this.on_dirty(|ctx, report| …)`. Use them as `test "…" rollback logged_in() { … }`: put
`rollback` first so state created by later modifiers is undone too.

## Undo and scopes

- `rollback` (test modifier) = run the case inside `scoped`.
- `scoped(|| …)`: undo everything registered inside when it ends, success or failure.
- `transaction(|| …)`: undo on failure, keep (hand to the outer scope) on success.
- `undo(|| …)` / `undo("label", || …)`: register cleanup for state no request created (a file
  written into a watched folder, say). Returns whether a scope was open to take it. Relative
  paths in it resolve as where it was registered (0.5+; before 0.5 they resolved against the
  working directory, so pass the absolute path `write_file` returns).
- Outside any scope (e.g. `klausur run`), undo actions aren't registered; klausur says so.
- A failed undo marks the case **dirty**; the run stops (unless `--on-dirty=continue`), lists
  what's left behind, and exits with 3.

## Built-ins

| | |
|---|---|
| `env(name)`, `env(name, default)` | Process env, then `.env.<name>` (`--env name`), then `.env` |
| `log(x)`, `print(x)` | Into the case log (shown on failure, or always with `-v`) |
| `fail(msg)` | Fail now |
| `unique("alice")`, `unique()` (0.4+) | `alice-3f9c2a1b` / `3f9c2a1b`: data no other test uses. A suite's own `fn unique` replaces it |
| `uuid()`, `sleep(ms)` | `sleep` is for pacing only; wait for conditions with `poll` |
| `json(path)`, `csv(path)`, `read_file(path)`, `read_blob(path)` | Relative to the current file |
| `file(path)`, `file_from(name, content[, type])`, `multipart(#{…})` | Upload bodies |
| `write_file(path, content)`, `remove_file(path)`, `file_exists(path)` | `write_file` returns the absolute path |
| `write_file(path, content, undo: true)` (0.3+) | Also registers an undo that restores the earlier content, or removes a new file: the way to write fixture files |
| `watch_file(path)` (0.3+) | `.lines()` / `.text()`: what was appended since, e.g. `expect !log.lines().some(\|l\| l.contains("ERROR"))` |
| `now()`, `now("%d.%m.%Y")`, `today()` (0.3+) | Local time: ISO 8601, a strftime pattern, `2026-11-05` |
| `raw(x)`, `url_path(x)` (0.3+) | Insert verbatim; insert a path into a URL |
| `html_select(html, css)`, `xml_select(xml, css)` | `[#{tag, text, attrs, html}]` (`xml` for XML); `html` is the **outer** markup. 0.3+ adds `inner_html`/`outer_html` (`inner_xml`/`outer_xml`) |
| `json_ld(html)` (0.3+) | Every JSON-LD script of a page, parsed (arrays flattened) |
| `html_text(html)` | Visible text of a page |
| `json_path(value, "$..url")` | Every value a JSONPath selects, as an array |
| `regex_match`, `regex_find`, `regex_find_all`, `regex_captures`, `regex_replace` | `(text, pattern, …)` |
| `base64_encode`, `base64url_encode`, `base64_decode`, `base64_decode_blob`, `jwt_decode(token)` | `jwt_decode` → `#{header, claims, signature}`, unverified |
| `sse_parse(text)` | `[#{event, data, id}]` |
| `ctx()` | The running case's context (in functions; `ctx` in test bodies) |
| `browser()` | Browser tests, see `browser.md` |

## CLI

```
klausur test [paths…] ["name filter"…] [--tag T] [--case k=v|label] [--seed N]
             [-j N] [-v] [--timeout SECS] [--env NAME] [--on-dirty=abort|continue] [--headed]
             [--fail-fast] [--junit FILE] [--watch]   # 0.4+: stop early; JUnit XML; rerun on change
             [--skip-tag T]                           # 0.5+: leave out tests with a tag
klausur run file.kl[#name] [-a k=v …] [-q] [--env NAME] [--headed]
klausur list [paths…]
klausur update                           # 0.4+: replace klausur with the latest release
klausur init [dir] [--auth basic|bearer] # 0.4+: a suite to start from (src/test/klausur)
```

Arguments that are existing paths select files or directories (searched for `*.kl`); others
filter test names by substring. Without paths, `test` uses the current directory. `run` prints
the trace and headers on stderr and the body on stdout (`| jq` works); argument values are parsed
as JSON when they can be. Exit codes: 0 ok, 1 failed, 2 load error, 3 dirty.

## Pitfalls

- **Reserved words** can't be variable or parameter names: `call`, `eval`, `goto`, `match`,
  `case`, `default`, `new`, `with`, `async`, `await`, `spawn`, `this`, `var`, `static`,
  `shared`. Pick `kind` for `match`, `entry` for `case`, `fallback` for `default`. As keys
  they're fine from 0.5: `body.default`, `#{ call: 1 }`; before 0.5 write `body["default"]`
  and `#{ "call": 1 }`.
- **In-place methods return `()`**: `s.trim()`, `s.replace(a, b)`, `a.sort()`, `a.reverse()`
  change the value and return nothing. Use `trimmed()`, `sorted()`, `reversed()`, or call the
  in-place method on its own line and use the variable afterwards.
- **`"…"` doesn't interpolate**; use backticks: `` `user-${id}` ``.
- **One namespace**: the same `fn` in two test files is a load error ("first defined here").
- **Missing keys are `()`**: `body.typo` silently is null. Assert shapes with `is`.
- **Raw request variants have no undo**: if the service wrongly accepts what a test expects it to
  reject, that state stays. Register cleanup when it matters:
  `if r.status == 201 { undo(|| deleteUser(r.body.id)) }`.
- **Named arguments** work on calls to functions and requests (`f(a: 1)`) and on built-in
  methods that take options (`page.wait(sel, state: "hidden")`), but not on your own functions
  called as methods: `x.f(a: 1)` loses `x`. Write `f(x, a: 1)`.
- **No named arguments inside `${…}`**: `` `${f(a: 1)}` `` is a load error. Assign first:
  `let v = f(a: 1)`, then `` `${v}` ``.
- **Slashes in URL values become `%2F`**: a path parameter needs `url_path(x)` (0.3+) or
  `raw(x)`.
- **Closures capture by value** at creation; hooks get fresh values through `ctx`.
- **Integer division** truncates; use a float (`7.0 / 2`) for 3.5.
