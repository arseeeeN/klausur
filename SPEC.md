# klausur — language & tool spec (draft)

Status: **draft**. Sections marked *(post-v1)* are designed but not implemented yet. §11 lists
what the current implementation covers.

klausur is an API client and testing tool. It scales from "send this one request,
but nicer than curl" up to integration test suites with parameterized and fuzzed
tests and automatic rollback of created state.

Files use the `.kl` extension.

---

## 1. Design principles

1. **Requests are declarations, everything else is code.** A request block
   describes exactly one exchange declaratively. Sequencing, loops and logic live in
   functions and tests.
2. **Expressions everywhere.** Values, assertions, captures and arguments are all
   expressions, so there is no special predicate vocabulary.
3. **Extensible by the user.** Modifiers, generators and helpers are functions
   written in the language itself. Built-ins use the same mechanisms.
4. **Never leave a mess silently.** When cleanup fails, the user is told loudly
   and precisely what is left behind and how to recover.

Scripting is done in [Rhai](https://rhai.rs). klausur's own parser handles the file
structure (request blocks, tests, modifiers, named arguments). Rhai evaluates the
expressions and the bodies of `fn`s and tests.

---

## 2. Lexical basics

- `//` line comments and `/* */` block comments, the same as Rhai. In a `connection` block, `//` only starts a comment at the start of a
  line or after whitespace, so `https://…` is safe. `send` bodies are never
  scanned for comments; their content is literal. In code bodies, Rhai handles comments natively.
- `{{ expr }}` interpolates an expression into request text (see §4.3).
- Identifiers follow Rhai rules.
- **Semicolons are optional.** In code bodies, a newline ends a statement unless the line
  obviously continues: it ends with an operator or `,`, or the next line starts with `.`,
  `&&`, `||`, `else`, `catch`, or a closing bracket. klausur inserts the `;` that Rhai needs.
- `null` is accepted as a value. It's Rhai's `()`, and JSON `null` converts to and from it.

---

## 3. Top-level items

A file is a sequence of top-level items. Their order doesn't matter, except for
`let`s that depend on each other.

| Item | Form |
|---|---|
| Variable | `let name = expr` |
| Function | `fn name(params) { … }` (plain Rhai function) |
| Request | `@name(params) modifiers { … }` |
| Test | `test "name" (params) modifiers { … }` |
| Import | `import "path.kl"` / `import "path.kl" as ns` |

Imports are resolved relative to the importing file, and each file is loaded once. In this
version, all requests and functions share **one namespace** across files. An alias
(`ns::create(...)`) checks that the name is defined in that file and makes the call site
clearer, but two files can't define the same name. Real per-file namespaces are post-v1.

```
let base  = "https://api.example.com"
let token = env("API_TOKEN")
```

---

## 4. Request blocks

```
@createUser(name = "alice", role = "user") {
    connection {
        POST {{base}}/users
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
```

`@name` binds a callable request value to `name`. Parameters can have default
values.

### 4.1 Sections

| Section | Required | Count | Meaning |
|---|---|---|---|
| `connection { … }` | yes | 1 | Protocol line plus headers / metadata |
| `send …` | no | ≤1 | Request payload |
| `expect …` | no | ≤1 | Assertions on the response |
| `let x = expr` | no | any (unique names) | Captures, exposed on the result |
| `undo …` | no | ≤1 | Compensating action (see §7) |

A request block is **declarative**. Sections appear at most once, and their source
order is irrelevant. A duplicate section is a parse error that points at both
occurrences. The runtime always runs a block in this order:

1. resolve `connection`
2. send `send`
3. receive the response
4. evaluate `expect`
5. bind the `let`s
6. register `undo`, but only if `expect` passed

`klausur fmt` rewrites the sections into the canonical order above.

### 4.2 Connection & protocols

The first line of `connection` selects the protocol by its verb:

| Verb | Protocol | v1 |
|---|---|---|
| `GET POST PUT PATCH DELETE HEAD OPTIONS` | HTTP | ✓ |
| `WS` | WebSocket | *(post-v1)* |
| `GRPC host Service/Method` | gRPC | *(post-v1)* |
| `SSE` | server-sent events | *(post-v1)* |
| `WT` | WebTransport | *(post-v1)* |

The lines after the protocol line are headers, `Name: value`, with interpolation.

The HTTP method can be a variable, written as one interpolation in place of the verb:

```
@indexFile(jwt, url, file, method = "POST") {
    connection {
        {{method}} {{base}}/api/indexing/file
        Authorization: Bearer {{jwt}}
    }
    send = multipart(#{ url: url, file: file })
    expect body.status == if method == "PUT" { "updated" } else { "indexed" }
}
```

- The interpolation must be the whole first word: `P{{x}}T` is a load error.
- The value is checked on every call. It must be a string of HTTP token characters, so
  custom methods such as `PURGE` work, and `()` or `"P T"` fail the call.
- It is sent as given. Methods are case-sensitive, so `"put"` isn't uppercased. If an
  expectation fails after a lowercase standard method was sent, the failure says so.
- Literal methods are still checked when the file loads.
- `undo` doesn't know which method was sent. Branch on the parameter if that matters:
  `undo { if method == "POST" { deleteByUrl(jwt, url) } }`.

### 4.3 `send` and interpolation

- `send { … }` / `send [ … ]`: a **JSON body**. Inside it, `{{expr}}` inserts the
  value *as JSON*: `{{name}}` becomes `"alice"`, `{{tags}}` becomes `["a","b"]`, and
  quotes and escaping are handled for you. If no `Content-Type` is set,
  `application/json` is used.
- ```` send ``` … ``` ````: a **raw text body**. `{{expr}}` inserts the value as
  plain text.
- `send = expr`: a body computed by script.
  - A map or array is serialized to JSON.
  - A string is sent as-is, and a blob as bytes.
  - `multipart(#{ … })` becomes a `multipart/form-data` body. Plain values become fields,
    `file(...)` values become file parts, and arrays become repeated parts.
  - A `file(...)` is sent as raw bytes, with its content type.
- In header values, `{{expr}}` inserts the value as plain text. **A header with a null
  interpolation is left out**, so `Authorization: Bearer {{jwt}}` with `jwt = ()` sends no
  `Authorization` header.
- In the URL, an interpolation at the very start is inserted verbatim, so `{{base}}/users`
  works. Every later interpolation is percent-encoded as a URL component: everything except
  `A–Z a–z 0–9 - . _ ~`.
- `raw(x)` inserts `x` verbatim everywhere: no URL encoding, and raw JSON in JSON bodies.
- Inside a JSON string literal, `"id-{{x}}"` inserts the value escaped, without quotes.
- After interpolation, a JSON body must be valid JSON; otherwise the request fails before
  it is sent.

### 4.4 `expect`

`expect` takes a single expression, or a block of them (one per line). Every
expression must evaluate to `true`. The following are in scope:

- `status` (int)
- `headers` (map, case-insensitive)
- `body` (parsed JSON if possible, otherwise a string)
- `raw` (blob)
- `duration` (ms)
- the request's parameters

A failure report shows the expression, the values of its sub-expressions, and the
request and response.

`x is <type>` is sugar for a type check: `string`, `int`, `float`, `number`, `bool`,
`array`, `map`, `blob`, `null`.

For a failed comparison, the report shows both values, e.g. `expectation failed: 404 == 200`.
In `a && b` chains, it reports the part that failed. Operands are evaluated only once, so
a request inside an expectation runs once.

### 4.5 Calling requests

```
let u = createUser(name: "bob")        // named arguments
let u = createUser("bob", "admin")     // positional arguments
```

A call runs the request and returns a **result** with these fields:
- `status`, `headers` (lowercase names), `body`, `raw`, `duration` (ms)
- `request`: the method, URL, headers and body that were sent
- every capture as a field (`u.id`)

Redirects are not followed, so a `302` is reported as a `302`. The default request timeout
is 30 s; change it with `--timeout`, or per request with the `timeout` modifier (§7.3).

If `expect` fails, the call raises an error, which fails the enclosing test or
function. For now, catch the error with Rhai's `try { … } catch (e) { … }`, which is a
statement. A dedicated non-raising form, like `try_createUser(...)` or `createUser(...)?`,
is still open.

Requests and functions are called **the same way**, with no syntax that marks a
call as a request. That way a request can be turned into a wrapping `fn` without
changing any call site, and both can be passed around as values. Requests and
functions share a namespace, so defining a name twice is a load-time error.
Editors and the reporter can tell the two apart, because every name is resolved
when the file loads. If it turns out in practice that callers need to see which
calls do I/O, a distinction can be added later.

**Implementation note.** A request block is parsed into a **Rust struct**, which
is the single source of truth. Each expression inside it (interpolations, `expect`
lines, captures) is compiled once into a Rhai AST. The request is registered in
the Rhai engine as a **native function**. When it's called, Rust evaluates those
expressions with the bound parameters and runs the steps in the order from §4.1.
To scripts, a request is an ordinary function value. Tooling (`run`, the TUI,
`fmt`, the journal) works on the struct. Named arguments and defaults use the same
mechanism for requests and `fn`s. See §13 for how the parsing is split between
klausur and Rhai.

### 4.6 Streaming protocols *(post-v1)*

For conversational protocols, a call returns an **open handle**. The conversation
takes place in imperative code:

```
@chat(room) {
    connection { WS wss://chat.example.com/rooms/{{room}} }
}

test "chat greets" {
    let ws = chat("lobby")
    ws.send(#{ type: "hello" })
    expect ws.receive().type == "welcome"
}
```

The handle closes when its scope ends.

---

## 5. Functions

Functions are ordinary Rhai functions, plus default parameter values and named
arguments, which work exactly as they do for requests (§4.5). They can call
requests and other functions, and order matters as usual. `expect expr` can be used as a statement
anywhere. `assert` is an alias for it.

```
fn onboard(name) {
    let u = createUser(name: name)
    getUser(u.id)
    u
}
```

### 5.1 Built-in functions

| Function | |
|---|---|
| `env(name)`, `env(name, default)` | Environment variable (§9); fails if unset and no default |
| `raw(x)` | Insert verbatim into templates (§4.3) |
| `json(path)`, `csv(path)`, `read_file(path)` | Read files relative to the current file. CSV rows become maps, with numbers and booleans converted |
| `scoped(\|\| …)`, `transaction(\|\| …)` | Undo scopes (§8.2) |
| `ctx()` | The running case's context (§7.1). Lets hooks hand values to the test body |
| `log(x)`, `print(x)` | Add to the case log, shown on failure or with `-v`. Maps and arrays are shown as JSON, here and in `${…}` |
| `fail(msg)` | Fail with a message |
| `uuid()` | |
| `sleep(ms)` | Wakes up early for Ctrl-C and the test's `timeout`. To wait for a condition, use `poll` (§7.3) |
| `gen::…` | Generators (§6.2) |
| `file(path)`, `file_from(name, content[, type])`, `read_blob(path)` | Files to upload; the content type is guessed from the name |
| `write_file(path, content)`, `remove_file(path)`, `file_exists(path)` | `content` is a string or a blob, and missing folders are created. `write_file` returns the absolute path. `remove_file` returns whether the file existed |
| `undo(\|\| …)`, `undo(label, \|\| …)` | Register a cleanup that isn't tied to a request (§8.1) |
| `multipart(#{ … })` | A `multipart/form-data` body (§4.3) |
| `sse_parse(text)` | Server-sent events as `[#{event, data, id}]`. Responses with `text/event-stream` get them as `events` automatically |
| `base64_encode`, `base64url_encode`, `base64_decode`, `base64_decode_blob` | |
| `jwt_decode(token)` | `#{header, claims, signature}`, without verifying the signature |
| `regex_match`, `regex_find`, `regex_find_all`, `regex_captures`, `regex_replace` | `(text, pattern, …)`; named groups in `regex_captures` become a map |
| `html_select(html, css)`, `xml_select(xml, css)` | Elements matching a CSS selector, as `[#{tag, text, attrs, html}]` (`xml` instead of `html` for XML). `text` has its whitespace collapsed. XML namespaces are ignored: `entry > title` matches Atom's `<entry>` |
| `html_text(html)` | The visible text of a page, without `<head>`, scripts and styles |
| `json_path(value, path)` | Every value a JSONPath (RFC 9535) selects, as an array: `json_path(body, "$..url")`. The leading `$` is optional |
| `s.trimmed()`, `a.sorted()`, `a.sorted(\|x, y\| …)`, `a.reversed()` | Changed copies. Rhai's own `trim`, `sort` and `reverse` change the value in place and return `()`, so `let t = s.trim()` is `()`. For case, Rhai's `to_lower()` and `to_upper()` already return copies |

Top-level `let`s are evaluated **lazily** on first use. So their order doesn't matter, and
an unused `env("X")` doesn't fail when `X` isn't set.

---

## 6. Tests

```
test "name" (params) modifiers { body }
```

Code never calls a test. The **runner** invokes it once per case, using the
parameter sources the test declares.

### 6.1 Parameter sources

- **Values:** `(role in ["user", "admin"])`. Any iterable works, including arrays and
  ranges. When a test has several `in` parameters, it runs every combination of
  their values (the cartesian product).
- **Tables:** `(user, pass, ok) cases [ #{…}, #{…} ]`. Each row is one case.
  `cases csv("file.csv")` and `cases json("file.json")` load rows from a file. Each row must
  have exactly the parameters that have no `in` source.
- **Generators:** `(name in gen::string(1..64))`. The test samples `runs(n)`
  times. When combined with lists, it takes n samples per combination.

```
test "login" (user, pass, ok) cases [
    #{ user: "alice", pass: "secret", ok: true  },
    #{ user: "alice", pass: "wrong",  ok: false },
] {
    expect login(user: user, pass: pass).status == if ok { 200 } else { 401 }
}
```

Each case is reported on its own, with its arguments:
`✓ login [user="alice", pass="wrong", ok=false]`.

### 6.2 Generators & seeds

The built-in generators live in `gen::`:
- `int(range)`
- `float(range)`
- `string(len_range)`
- `email()`
- `uuid()`
- `one_of(...)`
- `array(gen, len_range)`
- `map(#{ k: gen })`

`gen::from_schema(openapi(file), "Type")` *(post-v1)* derives a generator from an
OpenAPI schema.

Users write their own generators with `gen::custom(|rng| …)`, as plain functions:
`fn even() { gen::custom(|rng| rng.int(0, 50) * 2) }`. `rng` has the methods `int(lo, hi)`,
`float()`, `bool()`, `pick(array)` and `string(len)`. `gen::rng()` returns the running case's
generator, for randomness inside test bodies. Generator values passed to `gen::map` and
`gen::array` can be nested generators or plain values.

A test with generator parameters runs **25 samples per combination** unless it sets
`runs(n)`.

Every run has a seed, shown in the summary when generated cases ran. Each case's seed is
derived from the run seed, the file, the test name and the case's index. So
`klausur test --seed N` reproduces the same values even when other tests are filtered
out, and the failure report prints the exact rerun command. Shrinking failing inputs is
*(post-v1)*.

### 6.3 Case labels

The report names each case after its arguments: `rules [role="admin", n=3]`. Maps and lists
shrink to `{…}` and `[…]`. The `label` modifier names the case instead. Its argument is an
expression evaluated per case, with the case's arguments in scope:

```
test "read rules" (name, acl, visible) cases [
    #{ name: "deny subtree", acl: #{…}, visible: false },
] label(name) { … }

test "role matrix" (endpoint in ["stats", "health"], role in ["none", "admin"]) label(`${role} → ${endpoint}`) { … }
```

No key is reserved: `name` above is an ordinary parameter. A failing case lists its full
arguments under the label.

### 6.4 Pinning cases from the CLI

`klausur test "login" --case user=alice` runs only the matching cases. `--case "deny subtree"`
matches a label exactly.

---

## 7. Modifiers

Modifiers sit between a test's parameter list and its `{`. They're written as a
bare name or as a call: `rollback runs(200) as_user("admin")`.

### 7.1 Semantics

A modifier is a Rhai function called **method-style on the test definition**, so
`rollback runs(200)` desugars to `t.rollback(); t.runs(200);`. Inside it, `this`
is the test definition, which exposes:

| API | Purpose |
|---|---|
| `this.before(\|ctx\| …)` | Runs before each case |
| `this.after(\|ctx, result\| …)` | Runs after each case |
| `this.around(\|ctx, run\| …)` | Wraps each case; `run.call()` runs the inner part |
| `this.on_request(\|req\| …)` | Middleware for every request in the case. Receives `#{method, url, headers, body}` and returns the (modified) map |
| `this.on_dirty(\|ctx, report\| …)` | Called if rollback fails (see §8.4) |
| `this.meta(key, value)` | Settings read by the runner (tags, run count, …) |

`ctx` is the context of one running case:
- `ctx.headers`: default headers for every request
- `ctx.test`, `ctx.args`: the test's name and arguments
- `ctx.log(x)`
- in test bodies, `ctx` is also a variable, so `ctx.answer = …` works
- any other property, e.g. `ctx.user = …`, as free-form storage that the body can read
  through `ctx()`

Hook semantics:
- **`before`:** if a `before` hook fails, the case fails and the inner layers don't run.
  The `after` hooks of that layer still run.
- **`after`:** receives `#{passed, error}`.
- **`around`:** gets `run`, and `run.call()` returns `#{passed, error}`. If the hook returns
  that map, it decides the case's outcome; it can even turn a failure into a pass. If it
  returns anything else, the outcome of the last `run.call()` counts. If it never calls
  `run`, the inner part is skipped and the case passes.

Modifiers are applied **outside-in**, so the first one listed wraps all the others.
That makes the order meaningful: in `rollback as_user("admin")`, the user that
`as_user` creates is inside the scope and gets cleaned up.

```
fn as_user(role) {
    this.before(|ctx| {
        let u = createUser(role: role)
        let s = login(user: u.name, pass: u.password)
        ctx.headers["Authorization"] = `Bearer ${s.body.token}`
    })
}
```

An unknown modifier is a load-time error with a suggestion ("did you mean
`rollback`?").

### 7.2 Built-ins

These are implemented in Rust on top of the same API:
- `rollback`: runs each case inside `scoped` (§8.2)
- `timeout("5s")`: also `"500ms"`, `"2m"` or an integer in ms. It applies to the whole case,
  hooks included. In-flight requests are cut off when time runs out; the cleanup that
  follows isn't.
- `retries(n)`: up to `n` extra attempts
- `runs(n)`
- `seed(n)`
- `tags(…)`
- `skip("reason")`
- `only`
- `label(expr)`: names each case in the report (§6.3)

### 7.3 Modifiers on requests and functions

Requests take modifiers between their parameters and `{`: `timeout` and `poll`.

```
@chat(message) timeout("120s") {
    connection { POST {{base}}/api/chat }
    send { "message": {{message}} }
}
```

The argument is evaluated on every call, with the parameters in scope:
``timeout(`${ms * 3}ms`)`` works. It overrides `--timeout` for that request and is still cut
short by a test's `timeout`.

`poll` repeats a request until a condition holds, for work a service does in the background:

```
@jobStatus(id) poll(until: body.state == "done", every: "1s", within: "2m") {
    connection { GET {{base}}/jobs/{{id}} }
    expect body.result.errors == []
}
```

- `until` is evaluated after every response, with the response in scope the way `expect`
  sees it. It can also be a closure, which gets the response: `until: |r| r.status == 200`.
  It must give `true` or `false`.
- `every` is the pause between attempts (default `"1s"`). `within` bounds the whole wait
  (default `"60s"`), and a test's `timeout` still cuts it short.
- An attempt that gets no response (connection refused, `timeout` hit) counts as "not yet",
  so `poll` also waits for a service to come up.
- `expect`, `let` and `undo` run once, on the response that satisfied `until`.
- When `within` runs out, the call fails with the number of attempts and the last response,
  or the last error.

`poll(while: …, for: …)` checks the opposite: that something keeps holding, for instance that
a file the service should ignore never gets indexed.

```
@stillUnindexed(name) poll(while: body.total == 0, every: "1s", for: "15s") {
    connection { GET {{base}}/search?q={{name}} }
}
```

- The request repeats for the whole of `for`, and the call fails on the first response
  where `while` doesn't hold, showing that response.
- `for` is required. `within` goes with `until` and `for` with `while`.
- Unlike `until`, an attempt that gets no response fails the call: it proves nothing about the
  condition.
- `expect`, `let` and `undo` run once, on the last response.

A polling call blocks the test that makes it, like any other request. Polling needs no
`async` in the scripting language.

*(post-v1)* Modifiers on `fn`s.

---

## 8. Undo, rollback & failure handling

### 8.1 Undo actions

`undo` holds a compensating action. It can be written in three ways:

- An inline request, `undo { DELETE {{base}}/users/{{id}} }`. This is shorthand for
  a `connection` block.
- A full form with its own sections: `undo { connection {…} expect … }`.
- A call: `undo deleteUser(id)`.

Code that changes state without a request, such as writing a file into a folder a service
watches, registers its cleanup with the `undo` function:

```
fn drop_file(name, content) {
    let path = write_file(`${uploads}/${name}`, content)
    undo("remove the dropped file", || remove_file(path))
    path
}
```

The closure goes onto the same stack as request undos, so it runs with `rollback`, `scoped` and
`transaction`, and a failure shows up in the dirty report. `undo` returns whether it was
registered: with no scope open, it isn't.

An undo action is **registered only after its request's `expect` passed**. At
registration it captures the request's parameters and captures by value.

The exception is a request that **timed out**: the server may still complete it. Its undo is
registered anyway, with the parameters only. If the undo needs a capture from the missing
response, it fails, and the case is reported as dirty with "outcome unknown".

The default success criterion for an undo is `status` 2xx, **or 404/410**. "Already
gone" counts as success, so undo is idempotent. An explicit `expect` in the undo
overrides this.

### 8.2 Scopes

There is one undo stack, divided into scopes. Two built-in functions open a
scope. They differ only in what happens when the scope exits:

| | On success | On failure (uncaught error) |
|---|---|---|
| `scoped(\|\| …)` | unwind | unwind, rethrow |
| `transaction(\|\| …)` | **commit** | unwind, rethrow |

- **Unwind** runs the scope's undo actions in reverse order (LIFO). An interrupt
  counts as a failure.
- **Commit** hands the scope's undo actions to the enclosing scope. Nothing is
  undone yet, but nothing is forgotten either:
  - A committed transaction inside a `scoped` or `rollback` test is still cleaned
    up when that outer scope unwinds.
  - A committed inner transaction is undone if an outer transaction fails later.
  - With no enclosing scope, the state persists and its journal entries are
    removed.
- Both functions return the closure's value.
- The `rollback` test modifier is sugar for running the whole case inside
  `scoped`.
- Requests called outside any scope don't register their undo. The CLI shows a
  hint that a cleanup action was skipped. Error handling outside scopes is
  manual.

```
fn provision_customer(name) {
    transaction(|| {
        let org = createOrg(name: name)
        createUser(org: org.id, role: "admin")
        createBilling(org: org.id)   // fails: user deleted, then org deleted, error rethrown
        org
    })                               // succeeds: everything stays
}
```

Despite the name, `transaction` gives no atomicity or isolation. Other clients
can see the intermediate state, and undo is a best-effort compensation (the saga
pattern).

### 8.3 When an undo action fails

1. **Retry transient failures.** Connection errors, timeouts, `429` and `5xx`
   responses are retried with backoff (250 ms, then 500 ms), 3 attempts in total. Any
   other failure is final immediately. For the call form (`undo deleteUser(id)`),
   whether a failure is transient is judged by its last HTTP response.
2. **Keep unwinding.** The remaining undo actions still run (best effort), so as
   little as possible is left behind. Undo actions that fail after an earlier
   failure are marked as possibly *consequent* failures. For example, a user
   delete fails because the order delete before it failed.
3. **Mark the case `DIRTY`.** Each case gets two independent statuses:
   - result: `passed` / `failed`
   - cleanup: `clean` / `dirty`

   "Test failed, cleanup succeeded" is an ordinary failure. **Any dirty case**
   makes the run dirty.
4. **Stop the run (by default).** Results after a dirty case can't be trusted, so
   klausur stops scheduling new tests. Tests already running are allowed to finish
   and unwind. `--on-dirty=continue` overrides this.
5. **Exit with a dedicated exit code.**

   | Exit code | Meaning |
   |---|---|
   | `0` | everything passed |
   | `1` | test failures |
   | `2` | usage / parse error |
   | `3` | **dirty**: the environment may contain leftover state |

   Code 3 takes precedence over 1, so CI can tell "the tests failed" apart from
   "the environment is polluted".

### 8.4 Dirty report

A dirty run ends with a prominent report. For each undo action that didn't
complete, it lists:

- the test, the case arguments and the seed
- the **original request** that created the state: method, URL, status and captures
  such as `id`
- the undo action that was attempted, its resolved request, and the response or
  error from every attempt
- whether the failure may be a consequence of an earlier failed undo
- every undo action that still hasn't run

`on_dirty` hooks run after the report is printed, e.g. to notify a channel.

### 8.5 Recovery journal *(post-v1)*

Undo actions are written to a **write-ahead journal** at the moment they're
registered, in `.klausur/journal/<run-id>.jsonl`. Entries are removed once they
complete. That way a crash, a `kill`, or a second Ctrl-C still leaves a precise
record of what is left over.

- The journal stores **unresolved** templates plus the captured values, never
  resolved secrets. Environment values such as tokens are resolved again at
  recovery time.
- `klausur recover` lists the pending undo actions from earlier runs and retries
  them interactively or all at once (`--all`).
- `klausur recover --discard <run-id>` drops a journal once it has been cleaned up
  by hand.
- At startup, klausur warns if pending journals exist for the environment it's
  about to use.

### 8.6 Interrupts

- First Ctrl-C: stop scheduling, let the in-flight request finish, fail the running case,
  and **unwind all open scopes**. Undo actions keep running during the interrupt.
- Second Ctrl-C: exit immediately. The journal keeps whatever was left behind.

---

## 9. Environments

`env("NAME")` looks the name up in this order; the first match wins:
1. the process environment
2. `.env.<name>`, when `--env <name>` is given
3. `.env`

Both files are read from the current directory, in dotenv format.

Reports redact secret headers: `Authorization` keeps only its scheme (`Bearer ***`). Other
secret headers are hidden entirely: `Cookie`, `Set-Cookie`, API-key headers, and any header
whose name contains `token`, `secret` or `password`. *(Open: whether the env-file format
should grow beyond dotenv, e.g. with per-environment base URLs.)*

---

## 10. CLI

```
klausur run  <file>[#name] [-a k=v ...] [--env NAME] [--timeout SECS] [-q]
klausur test [paths/filters] [--seed N] [--case k=v|label] [--tag T] [--on-dirty=abort|continue] [--env NAME] [--timeout SECS] [-v]
klausur list [paths]
klausur recover [--all | --discard <run-id>]   # post-v1
klausur fmt  [paths]                           # post-v1
```

- **`run`** calls a request or a function. Argument values are parsed as JSON if possible,
  then as a Rhai map literal (`#{…}`), and are otherwise used as strings. When run in a
  terminal, it prompts for missing required arguments.
  - The trace (`→ POST …` / `← 201`), the status and the headers go to **stderr**.
  - The response body or return value goes to **stdout**, so
    `klausur run api.kl#getUser -a id=1 | jq` works.
  - Without `#name`, it runs the file's only request, or lists the choices.
- **`test`** treats arguments that are existing paths as files or directories (searched
  recursively for `*.kl`), and every other argument as a test-name filter (substring). It
  defaults to the current directory. Only tests from the given files run; tests in
  imported files don't. `only` on any test narrows the run to the `only` tests.
- **`list`** shows the requests (with signatures and protocol lines), functions and tests.

Output: human-readable. `--reporter junit|tap|json` for CI is *(post-v1)*.

*(post-v1)* An interactive TUI to browse requests and tests, fill in arguments,
inspect responses and drive recovery.

---

## 11. Implementation status

Implemented:
- Parser with span-based diagnostics, error recovery and suggestions.
- Top-level `let` (lazy) / `fn` / `@request` / `test` / `import`.
- Named and positional arguments and defaults for requests and functions.
- HTTP requests:
  - `connection`, `send` (JSON, fenced raw, `= expr`), `expect`, `let`, `undo` (all three
    forms)
  - `{{}}` interpolation that understands JSON and URLs
- Tests with `in` (lists, ranges, generators) and `cases` (inline, CSV, JSON), plus
  `--case` filters.
- Generators and reproducible seeds.
- Modifiers:
  - user-defined, with `before` / `after` / `around` / `on_request` / `on_dirty` / `meta`
  - built-ins `rollback`, `timeout`, `retries`, `runs`, `seed`, `tags`, `skip`, `only`
- Undo: `scoped`, `transaction`, retries for transient failures, the dirty report,
  `--on-dirty` and exit code 3.
- Ctrl-C handling.
- `klausur run`, `klausur test`, `klausur list`, and a human reporter.

Also implemented:
- the request modifiers `timeout` and `poll`
- multipart and file bodies, and writing files
- `undo(|| …)` for cleanup in scripts
- SSE parsing, and base64, JWT and regex helpers
- CSS selectors over HTML and XML, and JSONPath

See [GAPS.md](GAPS.md) for what testing a real service turned up.

Not yet: the recovery journal and `recover`, `fmt`, non-HTTP protocols, modifiers on
functions, shrinking, OpenAPI generators, CI reporters, real per-file
namespaces, the TUI.

## 12. Open questions

- Syntax for test parameter sources: the inline `in` / `cases` form is accepted for
  now, but not loved.
- How non-raising calls look (`try_x(...)` vs `x(...)?`).
- Whether imports should get real per-file namespaces (currently one shared namespace).
- Environment file format.
- Final name.

## 13. Implementation: what parses what

| Layer | Handled by | Why |
|---|---|---|
| File structure: top-level items, `@` blocks, `test` headers, modifier lists, `connection` / `send` text | klausur's own parser | Rhai's tokenizer can't read this text: `//` in URLs starts a Rhai comment, header lines aren't Rhai, and raw bodies aren't Rhai. Custom syntax also can't declare top-level functions. |
| Code bodies (`fn`, `test`, closures) and embedded expressions | Rhai, compiled from the user's original text | Error positions point at the real file. Each fragment is compiled with its line/column offset. |
| `expect` / `assert` statements, `is` type checks | Rhai custom syntax / custom operator | They work inside any Rhai code with Rhai's own spans. |
| Top-level variables in `fn`s and requests | `Engine::on_var` resolver | Rhai script functions can't see variables from the top level of the file. |
| Named arguments `f(a: 1)` | klausur pre-pass (rewrite to `f__kw(#{a: 1})`; positional arguments mixed in become `"_0"`, `"_1"`, …) | Rhai can't parse this. Every request and function also gets a `name__kw` variant that unpacks the map and fills in defaults. |
| Optional semicolons | klausur pre-pass | Rhai requires `;` between statements. |
| Functions | One generated library, registered as a global Rhai module | Every unit can call every function, and errors keep their source. Default parameters become extra overloads. |

All rewrites only insert text. The generated code carries a source map, so every Rhai
error, failed expectation and call trace points at the original file.
