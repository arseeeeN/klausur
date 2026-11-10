---
name: klausur
description: "Writes, runs and debugs API and browser end-to-end tests with klausur (`.kl` files, `klausur test`), and turns ticket, issue, pull-request or merge-request requirements into klausur tests. Use this skill whenever the user wants to test an HTTP API or web UI end to end, add or fix tests in a `src/test/klausur` suite or any `*.kl`/`*.test.kl` file, check that a feature or bug fix works against a running service, cover acceptance criteria of a ticket or MR with tests, send a single API request nicer than curl (`klausur run`), or read a failing klausur report, even if they don't name klausur and just say 'write API tests for this endpoint', 'test this ticket', 'cover this MR with e2e tests' or 'does the deactivate endpoint work'."
---

# klausur: API and browser tests from requirements

klausur (https://github.com/arseeeeN/klausur) is a test runner and API client in one small
language: **request blocks** declare one HTTP exchange each (connection, body, expectations,
captures, how to undo it), and everything else is [Rhai](https://rhai.rs) script: functions,
tests, modifiers. Tests can also drive Chrome. Its strength is that tests **clean up after
themselves**: every request that creates state declares its `undo`, and `rollback` runs those
undos when the case ends, pass or fail.

Use it for end-to-end tests against a *running* service, not for unit tests.

## Step 0: Check the tool and find the suite

```bash
klausur --version          # 0.2+ for browser tests; install below if missing
```

If klausur says a newer release is out, or a suite uses something your version lacks (an
"isn't defined" for a documented built-in), `klausur update` (0.4+) or the install script below
brings it up to date; ask the user first, it changes their machine.

If it's missing, install the release binary (Linux, macOS):
`curl -fsSL https://github.com/arseeeeN/klausur/releases/latest/download/install.sh | sh`
(`irm …/install.ps1 | iex` on Windows). Ask before installing on someone else's machine.

Then find the project's suite, usually `src/test/klausur/` (`**/*.kl` otherwise), and read, in
this order:

1. its `README.md`: how to start the service and which env vars the suite reads,
2. `api.kl` (the requests) and `lib.kl` (helpers, fixtures, modifiers): **reuse these** rather
   than writing new requests for endpoints that already have one,
3. one existing `*.test.kl` close to what you're about to test, to match its style.

`klausur list <suite-dir>` prints every request with its signature, every function and every
test.

No suite yet? `klausur init [dir]` (0.4+) creates one in `src/test/klausur` or the folder you
name, with `--auth basic|bearer` for a working login modifier. Rename its placeholder `BASE_URL`
after the service when that helps (`ORDERS_URL`). Before 0.4, create the same by hand:

```
api.kl            let base = env("BASE_URL", "http://localhost:8080"), one request per endpoint
lib.kl            import "api.kl"; shared helpers (login modifiers, fixtures)
<area>.test.kl    import "lib.kl"; the tests, one file per area of the API
README.md         how to start the service, the env vars the suite reads, how to run it
```

Say in the README how to start the service locally. All loaded files share one namespace, which is why
shared helpers live in `lib.kl` only.

**Before running anything, know where the tests point.** Tests create and delete real data.
Running against a local or disposable environment is fine; for anything shared (dev, staging,
or a URL you don't recognise) ask the user first. The base URL is usually
`env("…_URL", "http://localhost:…")` at the top of `api.kl`.

## Step 1: Learn the language before writing code

Read **`${CLAUDE_SKILL_DIR}/references/language.md`** before writing your first `.kl` code in a
session. It's a compact but complete reference: request blocks, interpolation, `expect`, tests
and parameter sources, modifiers, undo and scopes, built-ins, and the Rhai pitfalls that cost
time (reserved words such as `match` and `call`, in-place methods returning `()`, one namespace
across files). The language looks familiar but isn't JavaScript, and guessing costs more than
reading.

For browser tests, also read **`${CLAUDE_SKILL_DIR}/references/browser.md`**.

## Step 2: Turn the requirements into a test plan

When the task comes from a ticket, a pull or merge request, or a feature description, follow
**`${CLAUDE_SKILL_DIR}/references/requirements.md`**: gather the requirements (the ticket, the
PR or MR with `gh` / `glab`, the user's text), list the behaviours each one implies, including
the ones it implies without saying (errors, permissions, side effects, repeats), and map them to
named tests. Show the plan as a table **before** writing a lot of tests when the scope is large
or unclear; for a handful of obvious tests, just write them and show the table in the report.

## Step 3: Write the tests

Add the requests the plan needs to `api.kl` (or a new `*.kl` the tests import), then the tests.
The patterns that keep a suite trustworthy, and why:

- **Success invariants go in the request's `expect`**, so every caller checks them for free;
  error cases use a raw variant without `expect` (`send = payload`) and check `status` in the test.
- **Every request that creates state has an `undo`**, and every test that creates state uses
  `rollback`. Otherwise one run's leftovers break the next, and nobody knows why.
- **Unique data per test** with `unique("alice")` → `alice-3f9c2a1b` (built in from 0.4; before,
  ``fn unique(p) { `${p}-${uuid().sub_string(0, 8)}` }`` in `lib.kl`): tests then can't see
  each other's records, even across runs.
- **Wait with `poll`, never `sleep`**, for anything the service does in the background.
- **Name tests after the behaviour** ("a deactivated user can't log in"): the name is what the
  report and the user see.
- **`parallel` only for tests that don't depend on global state** (counts, "the only one"); the
  rest run alone, and that's the default for a reason. Tests that share a slow or rate-limited
  resource (an LLM, an external API) go in a group: `parallel("llm", 1)` (0.4+) keeps them from
  overlapping while the others still run in parallel.
- **Don't weaken a test to make it pass.** If the service is wrong, that's the finding: keep the
  test failing, or mark it `skip("BUG-123: what's wrong")` if the user wants a green run, and
  report it.

## Step 4: Run, read failures, iterate

```bash
klausur test <suite-dir> "name filter"   # the tests you wrote; filters match test names
klausur test <suite-dir>                 # the whole suite, before you hand over
klausur test <file> -v                   # logs and the request trace of passing tests too
klausur test <suite-dir> --fail-fast     # 0.4+: stop at the first failure (long suites)
# Not `--watch`: it reruns on every change and never exits, so a tool call would hang. It's
# for people writing tests; suggest it to the user instead.
klausur run <suite-dir>/api.kl#createUser -a name=alice   # one request, body to stdout
```

For CI, add `--junit klausur-report.xml` (0.4+): GitLab and Jenkins show the results, failures
with their source line and values, from that file. The guide's CI section has a GitLab job:
https://github.com/arseeeeN/klausur/blob/main/docs/guide.md#ci

Start the service the way the suite's README says if it isn't running (check its health URL
first). A failure report shows the source line, the values of the expression's parts under
`where:`, the call chain, and the last request and response; browser failures add the page URL,
a screenshot in `klausur-results/`, console errors and failed requests. **Read it fully before
changing code**: it almost always tells you whether the test or the service is wrong. Look at
browser screenshots with the image viewer (`Read` the PNG).

Exit codes: `0` passed, `1` failed, `2` the files don't load (the message points at the line),
`3` **dirty**: a cleanup failed and the report lists what was left behind. A dirty run is more
serious than a failed one: tell the user exactly what's left (the report has the requests and
ids), and fix the undo before running again.

When a test fails, decide which side is wrong:
- **The test**: wrong selector, wrong expectation about the response shape, missing setup.
  `klausur run` the request, or `log(r.body)` in the test, to see what the service really says.
- **The service**: the behaviour contradicts the requirement. Re-read the requirement, check the
  report's request and response, and if it's a real bug, report it (and don't "fix" the test).
- **The environment**: service not running, wrong env vars, a rate limit, a slow local LLM.
  Fix the setup, not the test.

## Step 5: Report

Tell the user, briefly:
- which tests you added or changed, as a table of **requirement → test → result**,
- the run's outcome (`N passed, M failed`, exit code), and for every failure whether it's a
  service bug, a test you couldn't finish, or an environment problem, with the key line of the
  report,
- requirements you couldn't test from outside, and why,
- anything left behind (a dirty run) or anything you needed to change in the environment.

If the suite keeps a `FINDINGS.md` (a list of service bugs the tests found), add yours there.
Don't create one unprompted for a single bug; the report is enough, and the user decides where
bugs are tracked.

## More

- `references/language.md`: the language, built-ins and pitfalls. `references/browser.md`:
  pages, locators, waiting, mocking. `references/requirements.md`: from ticket/MR to tests.
- The guide, for suite layout and patterns:
  https://github.com/arseeeeN/klausur/blob/main/docs/guide.md
- The full spec, when a detail isn't in the references (fetch the raw file):
  https://raw.githubusercontent.com/arseeeeN/klausur/main/SPEC.md
