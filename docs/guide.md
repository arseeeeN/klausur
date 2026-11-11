# Writing a test suite

How to build an API (and browser) test suite with klausur: the layout that scales, the patterns
that keep tests independent and clean, how to turn requirements into tests, and what to do when
one fails. [SPEC.md](../SPEC.md) is the reference for every construct mentioned here.

- [Layout](#layout)
- [Requests: `api.kl`](#requests-apikl)
- [Helpers and modifiers: `lib.kl`](#helpers-and-modifiers-libkl)
- [Tests](#tests)
- [From requirements to tests](#from-requirements-to-tests)
- [Running and debugging](#running-and-debugging)
- [CI](#ci)
- [Pitfalls](#pitfalls)

## Layout

Put the suite next to the code it tests, e.g. `src/test/klausur/` in a Maven project:

```
src/test/klausur/
├── README.md          how to start the service, which env vars, how to run
├── api.kl             one request block per endpoint
├── lib.kl             imports api.kl; shared helpers, fixtures and modifiers
├── fixtures/          files the tests upload or index
├── users.test.kl      one test file per area of the API, each imports lib.kl
├── orders.test.kl
└── FINDINGS.md        what the tests turned up, if you're testing an existing service
```

Only `*.test.kl` files hold tests by convention; `klausur test <dir>` runs the tests of every
`.kl` file under it, and imported files are loaded once. All files share **one namespace**: two
files can't both define `fn logged_in()`, so shared helpers belong in `lib.kl`.

`klausur init` (0.4+) creates this layout with an example request and test, in
`src/test/klausur` or the folder you name; `--auth basic|bearer` adds a working login modifier.

## Requests: `api.kl`

One request block per endpoint, named after what it does. The block states what a successful
call looks like, so every test that uses it checks that for free, and how to undo it:

```
let base = env("API_URL", "http://localhost:8080")

@login(user, pass = "secret") {
    connection {
        POST {{base}}/login
    }
    send { "user": {{user}}, "pass": {{pass}} }
    expect status == 200
    let token = body.token
}

@createUser(name, role = "user", jwt = ()) {
    connection {
        POST {{base}}/users
        Authorization: Bearer {{jwt}}
    }
    send { "name": {{name}}, "role": {{role}} }
    expect {
        status == 201
        body.id is int
    }
    let id = body.id
    undo {
        DELETE {{base}}/users/{{id}}
    }
}

@getUser(id) {
    connection { GET {{base}}/users/{{id}} }
}
```

- **Put the happy path's invariants in `expect`.** A test that calls `createUser` then knows the
  user exists. For error cases, add a raw variant without expectations (`@postUser(payload)`
  with `send = payload`) and check `status` in the test.
- **Give everything that creates state an `undo`.** It runs only if `expect` passed, so a
  rejected request doesn't trigger a pointless delete. A 404 or 410 on undo counts as success.
- **Defaults keep call sites short.** `jwt = ()` leaves the header out entirely: a header whose
  interpolation is null isn't sent.
- **Capture what tests need** with `let`: `createUser("bob").id`.
- **Paths go through `url_path`.** Every interpolation after the first in a URL is encoded as
  one component, so `/` becomes `%2F`. For a value that is itself a path (a JCR node, a file),
  write `{{base}}/nodes{{url_path(node)}}`: each segment encoded, the slashes kept.
- **Slow endpoints get a timeout**, so a hang fails fast: `@chat(q) timeout("90s") { … }`.
- **Background work is waited for with `poll`**, never with `sleep`:
  `@job(id) poll(until: body.state == "done", every: "500ms", within: "30s") { … }`. If the
  service settles in a burst, add `stable_for: "1s"`: the condition must then hold for a second
  without a break.

`klausur run api.kl#createUser -a name=alice` sends one request, which is the fastest way to
check a block while writing it.

## Helpers and modifiers: `lib.kl`

```
import "api.kl"

// `unique("alice")` is built in (0.4+): "alice-3f9c2a1b", a value no other test or earlier run
// uses, so tests can't see each other's data. Before 0.4, define it here:
// fn unique(prefix) { `${prefix}-${uuid().sub_string(0, 8)}` }

// Log in before each case; the token is `ctx.jwt` in the test body.
fn logged_in(user = "admin") {
    this.before(|ctx| { ctx.jwt = login(user).token })
}
```

A modifier is a plain function called on the test definition (`this`). `before`, `after` and
`around` hooks run per case, and `ctx` carries values into the test body. Fixtures that several
tests need (a set of documents, a customer with orders) are best as a modifier too, used with
`rollback` so they're removed afterwards: `test "…" rollback with_customer() { … }`.

A fixture that every test of a file shares and that is slow to make, such as a config change
the server first has to load, can be made once per file (0.5+):

```
setup mail_off()                                   // its undos run after the file's last test
setup let admin = createUser(role: "admin").body   // a value for the file's tests
teardown log("done")                               // custom cleanup, runs first at the end
```

Keep it for fixtures the tests only read: a test that changes a shared fixture makes the
others depend on the order they run in.

For a site with session cookies (a login form, a CSRF token), mark the test `cookies` (0.5+):
each case keeps a jar like a browser, so a login request in a `before` hook logs in every
request after it. `ctx.cookies.JSESSIONID` reads a cookie.

## Tests

```
import "lib.kl"

test "a new user can be fetched" rollback logged_in() {
    let user = createUser(unique("alice"), jwt: ctx.jwt)
    let fetched = getUser(user.id)
    expect fetched.status == 200
    expect fetched.body.name == user.body.name
}

test "names must not be empty" (name in ["", " "]) rollback logged_in() {
    let r = postUser(#{ name: name }, ctx.jwt)
    expect r.status == 400
}

test "roles" (role, can_delete) cases [
    #{ role: "admin", can_delete: true },
    #{ role: "user",  can_delete: false },
] label(role) rollback {
    …
}
```

- **Name a test after the behaviour it checks**, as a sentence: "a reader can't see HR
  documents", not "test_rbac_3". The report and failure output use the name.
- **`rollback` on every test that creates state.** It undoes everything the case registered, in
  reverse order, even when the case fails. A cleanup that fails marks the run dirty (exit code 3)
  and lists what was left behind.
- **Make data unique** (`unique("alice")`), so a test never sees another test's or an earlier
  run's leftovers, and searches find only what the test created.
- **One behaviour per test.** Several `expect`s are fine when they check one behaviour; the
  first failing one ends the case.
- **Tables for matrices.** `(x in [...])` runs every value, several `in` parameters run every
  combination, `cases [ #{…} ]` runs rows, and `label(expr)` names each case in the report.
  Generators (`gen::string(1..64)`, `gen::email()`, …) sample values, 25 per combination unless
  `runs(n)`; a failure prints the seed that reproduces it.
- **Group what mustn't overlap.** `parallel("llm", 1)` keeps tests that share a slow or
  rate-limited resource from running at the same time, while the others still overlap.
- **`--watch` while writing tests** reruns the selected tests on every save.
- **`parallel` only for tests that don't care what else runs.** A test that checks a global
  count ("exactly one document exists") must stay serial. Unmarked tests run alone.
- **Fixture files restore themselves**: `write_file(path, content, undo: true)` puts back what was
  there (or removes the file and the directories it created) when the case ends, so a fixture
  can't destroy an existing file. Prefer it to a hand-written `undo(|| remove_file(path))`.
- **Check the service's log** when "no new errors" is part of done:
  `let log = watch_file(log_path)` before, `expect !log.lines().some(|l| l.contains("ERROR"))`
  after. `today()` helps with dated log names.
- **Know when a test is flaky by nature** (an LLM's answer): check for facts, not wording, and
  use `retries(1)` sparingly. `log(…)` what the service said, so a failure shows it.
- **A test for a known bug** stays in the suite: `skip("BUG-123: …")` until it's fixed, so the
  report keeps listing it.

### Browser tests

With Chrome installed (klausur 0.2+), tests can drive the UI, set up through the API, and still
roll back. The API follows Playwright's (SPEC §14):

```
test "a new user shows up in the admin list" rollback logged_in() {
    let name = unique("alice")
    createUser(name, jwt: ctx.jwt)
    let page = browser().open(`${ui}/admin/users`)
    page.fill("input[type=search]", name)
    expect page.locator(".user-row").texts().contains(name)
}
```

Actions wait until their element can take them; `expect` retries while it reads the page; a
failure saves a screenshot to `klausur-results/` and lists console errors and failed requests.
Mock what the test doesn't own with `page.route("**/api/x*", #{ status: 500 })`.

## From requirements to tests

A ticket or merge request says what should be true afterwards. Turn that into tests in four
steps:

1. **List the behaviours.** For each acceptance criterion, write down the observable behaviour:
   which request (or UI action), with which input, gives which response or state. Add what the
   criterion implies but doesn't say: the error cases (invalid input, missing fields), who may
   and may not do it (roles, ownership), what it changes elsewhere (lists, counts, other
   endpoints), and what happens on repeat (idempotency, conflicts).
2. **Map behaviours to tests**, one test per behaviour, named as the behaviour. Matrices
   (roles × endpoints, valid × invalid values) become one test with `in` parameters or `cases`.
   Note what can't be tested from outside (internal logging, a cron schedule) instead of
   skipping it silently.
3. **Add the requests** the tests need to `api.kl`, with their success invariants and undo.
4. **Write and run the tests** against a running instance. When one fails, decide whether the
   test or the service is wrong: read the requirement again, and the failure report. A test
   that found a real bug keeps failing (or gets `skip("…")` with the ticket), and the bug is
   reported; don't weaken the test until it passes.

For example, "Admins can deactivate users. Deactivated users can't log in; their data stays.
Only admins may deactivate." becomes:

| Behaviour | Test |
|---|---|
| An admin deactivates a user | `an admin can deactivate a user` |
| The user can't log in afterwards | `a deactivated user can't log in` |
| Their data is still there | `deactivating keeps the user's data` |
| Non-admins get 403, and nothing changes | `only admins can deactivate` (`role in ["user", "anonymous"]`) |
| Deactivating twice (implied) | `deactivating twice is harmless` |
| Unknown user (implied) | `deactivating an unknown user is a 404` |

## Running and debugging

```
klausur test src/test/klausur                     # everything
klausur test src/test/klausur/users.test.kl       # one file
klausur test src/test/klausur "deactivat"         # tests whose name contains this
klausur test src/test/klausur --case role=user    # only matching cases
klausur test src/test/klausur --seed 1234         # replay generated values
klausur test src/test/klausur -v                  # logs and the request trace of every test
klausur test src/test/klausur --tag ui --headed   # tagged tests, with a visible browser
klausur list src/test/klausur                     # requests, functions and tests
klausur init                                      # a suite to start from, in src/test/klausur
```

A failure shows the source line, the values of the expression's parts (`where:`), the call
chain, and the last request and response; browser failures add the page, a screenshot, console
errors and failed requests. Read it before changing anything: it usually says whether the
service or the test is wrong.

Exit codes: `0` passed, `1` tests failed, `2` load error, `3` **dirty**: a cleanup failed and the
environment may hold leftovers, listed in the report. Clean those up before trusting the
environment again.

## CI

Run the service (or point `API_URL` at a test environment), install klausur, and run the suite:

```
curl -fsSL https://github.com/arseeeeN/klausur/releases/latest/download/install.sh | sh
klausur test src/test/klausur --junit klausur-report.xml
```

With `--junit`, GitLab shows the results in the merge request and the pipeline's Tests tab,
failures with their source line and values:

```yaml
api-tests:
  script:
    - curl -fsSL https://github.com/arseeeeN/klausur/releases/latest/download/install.sh | sh
    - ~/.local/bin/klausur test src/test/klausur --junit klausur-report.xml
  artifacts:
    when: always
    reports:
      junit: klausur-report.xml
    paths:
      - klausur-results/
```

`klausur-results/` keeps the screenshots of failed browser tests.

Treat exit code 3 as more serious than 1: the environment is polluted for the next run.
CI installs the latest release each time, so the update hint is off there (`CI` is set).
`--on-dirty=continue` keeps going after a dirty case instead of stopping. Keep secrets in the
environment (`env("API_TOKEN")`); `.env` and `.env.<name>` (with `--env <name>`) are read too.

## Pitfalls

- **Reserved words.** Rhai reserves `call`, `eval`, `goto`, `match`, `case`, `default`, `new`,
  `with`, `async`, `await`, `spawn`, `this`, `var`, `static`, `shared` and a few more; they
  can't be variable or parameter names. As keys they're fine (0.5+): `body.default` and
  `#{ call: 1 }` work, because klausur writes them as `body["default"]` and `#{ "call": 1 }`.
- **In-place methods return `()`.** `s.trim()`, `a.sort()` and `a.reverse()` change their value
  and return nothing, so `let t = s.trim()` is `()`. Use `trimmed()`, `sorted()`, `reversed()`.
- **Strings.** `"…"` is plain; interpolation needs backticks: `` `id-${x}` ``. In request
  blocks, `{{expr}}` interpolates (as JSON in JSON bodies, URL-encoded in URLs).
- **No named arguments inside `${…}`**: `` `${f(a: 1)}` `` doesn't load. Assign to a variable
  first. Named arguments on your own functions work as calls, `f(x, a: 1)`, not as methods,
  `x.f(a: 1)`.
- **Missing map keys are `()`**, not an error: `body.missing == null` is true. Check with
  `expect body.id is int` rather than relying on a later failure.
- **`try` is a statement**, not an expression: `try { … } catch (e) { … }`.
- **Top-level `let`s are lazy**, so order doesn't matter and an unused `env("X")` doesn't fail.
- **One namespace across all loaded files**; helpers used by several files go in `lib.kl`.
- **Raw variants have no undo.** A test that expects `postUser` to be rejected leaves the user
  behind when the service wrongly accepts it. The test fails either way, but if leftovers
  matter, register a cleanup when the status says it was created:
  `if r.status == 201 { undo(|| deleteUser(r.body.id)) }`.
- **Undo outside a scope doesn't run.** `klausur run` and code outside `rollback`/`scoped` don't
  register undo actions; klausur says so.
