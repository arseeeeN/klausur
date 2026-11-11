# Backlog

Ideas for what comes next.

## Streaming assertions

SSE bodies are read completely before they're parsed, so the time to the first event can't be
measured or asserted. Reading the stream as it arrives would allow `expect` on the first event's
timing, and fits the stream type in the tools concept below.

## Browser end-to-end tests

Drive a real browser from klausur tests, so a test can set up data with requests, check it in
the UI, and still have `rollback` clean up after it.

```
test "a search shows the indexed page" rollback writer() {
    indexContent(ctx.writer, "klausur://ui", "The zebrafinch nests here.", "/klausur/ui")
    let page = browser().open(`${frontend}/search`)
    page.fill("input[name=q]", "zebrafinch")
    page.click("button[type=submit]")
    expect page.text(".result:first-child").contains("zebrafinch")   // waits until it holds
}
```

**Approach: control the browser from klausur, not run klausur in the browser.** A runner
inside the page (Cypress's approach, or klausur compiled to WebAssembly) dies on navigation, is
bound by CORS and the page's origin, sends synthetic events (`isTrusted: false`), and can't take
real screenshots or use several tabs.

**Backend: an opaque API, starting with the Chrome DevTools Protocol.**
- Tests only see klausur's own `browser()` / `page` API, never protocol details, so the backend
  can change without touching tests.
- Start with CDP through a synchronous client such as the `headless_chrome` crate. It's mature,
  has screenshots, network, input and console access, and needs no Node. It's Chromium only,
  since Firefox dropped CDP.
- Move to WebDriver BiDi, the W3C cross-browser standard, once its Rust clients mature.
- Fallback if cross-browser is needed sooner: speak Playwright's driver protocol, the JSON-over-
  stdio protocol its Java/Python/.NET bindings use. That brings Playwright's auto-waiting and
  browser support, at the cost of a Node dependency and a protocol not meant for third parties.

**What should carry over from klausur:**
- **Selectors:** the same CSS syntax as `html_select`.
- **Waiting:** actions wait until the element is visible and enabled, and `expect` on page
  state retries until the test's `timeout`, reusing the `poll` machinery. That includes Ctrl-C
  and deadline handling.
- **Failure reports:** a screenshot, the page text around the selector, console errors and the
  last network requests, the way HTTP failures show their exchange.
- **Parallel tests:** one browser per worker thread, and a fresh incognito-like context per case.
- **Getting a browser:** use a local Chrome, or download a pinned "Chrome for Testing" build.

**Steps:**
1. ~~Spike against the ai-search frontend: open, click, fill, text, wait, and a screenshot on
   failure.~~ Done on the `browser-spike` branch (SPEC §14). What it showed:
   - The `headless_chrome` crate costs about 100 ms per protocol call: its WebSocket loop holds
     one lock around a read with a 100 ms timeout, and every write needs that lock. klausur now
     has its own small CDP client (`src/runtime/browser/cdp.rs`): a read thread and a separate
     write handle, about 1 ms per call, and no async runtime.
   - SvelteKit attaches event handlers after `load`, so the first click hit a dead button.
     Navigation now also waits for the network to be quiet.
   - Retrying `expect` when it read a page, and only then, reads naturally:
     `expect page.texts(".result").contains(word)`.
   - `goto` and `eval` are reserved words in Rhai; the methods are `navigate` and `evaluate`.
2. ~~Settle the API from what the spike shows, then add frames, file uploads, and network
   interception and mocking.~~ Done, following Playwright (SPEC §14): locators, strictness,
   form controls, uploads, shadow DOM, routes and `requests(glob)`. Frames are still open.
3. Playwright-level robustness, meaning full checks that an element can actually be clicked,
   shadow DOM, and trace files, is a much larger project; take it on only if needed. Done so
   far: waiting for elements to stop moving before clicks, and open shadow roots. Open: trace
   files, frames, closed shadow roots.

## TLS in the end-to-end tests

Every test server in `tests/` speaks plain HTTP, so nothing checks klausur's HTTPS path: in
particular the early-response check (`src/runtime/early.rs`), which wraps the TLS connection
too, and a server refusing a large upload over HTTPS. A local TLS server with a certificate made
at test time (e.g. `rcgen` and `rustls` as dev dependencies) would cover it, and also custom CA
or self-signed setups if klausur grows options for them. Not urgent: no problem seen so far.

## Testing local tools: processes, files and pipes (concept)

So far klausur tests services over HTTP. The same model (named calls with `expect`, captures
and `undo`, scopes, generators, readable failures) fits command-line tools and local daemons
just as well: run a tool, feed it input, check its output, its exit code and the files it
leaves, and clean up. These are ideas to settle before building anything.

### Commands as a protocol of request blocks

`connection` already picks the protocol by its first word. A command is one more:

```
@convert(input, format = "json") {
    connection {
        EXEC mytool convert --format {{format}} {{input}}
        Cwd: {{work}}
        Env-LANG: C
    }
    send = read_blob(input)            // stdin; no `send` means empty stdin
    expect {
        exit == 0
        stderr == ""
    }
    let result = json_of(stdout)       // a capture, like `let id = body.id`
    undo { EXEC mytool delete {{result.id}} }
}
```

- **No shell by default.** The line is split into arguments by klausur, and each `{{x}}` is
  exactly one argument (an array is several), so values never need quoting and can't inject
  anything. `SHELL …` runs the line through `sh -c` / `cmd /c` when pipes or globs are wanted,
  with the platform differences that brings.
- **Result:** `exit`, `stdout`, `stderr` (text, lossy UTF-8), `stdout_blob`, `duration`, and
  `body` = stdout parsed as JSON when it is JSON, so `expect body.ok` reads like HTTP.
- **Reported like an exchange:** the command line, cwd, the env that was set, stdin, exit code,
  and both outputs, shortened like response bodies.
- `timeout`, `poll` and `retries` work as they do for requests. A timed-out process is killed.
- A plain `exec(["mytool", "--version"])` for one-off calls, like `request` blocks vs. ad-hoc
  code. (`EXEC`, not `RUN`, so it doesn't read like `klausur run`.)

### Long-running processes

`spawn(…)` starts a daemon or a tool that keeps running and returns a handle:

- `p.output` is a **stream** (below), `p.write(text)` and `p.close_input()` feed stdin,
  `p.wait()` returns the result above, `p.kill()` stops it.
- Spawning registers an undo that stops the process (gracefully, then forcefully) at the end of
  the scope: no orphans, even when a test fails. With `setup`, a daemon runs for a whole file.
- Children die with the parent: a process group on Unix, a Job object on Windows.

### Streams

One type for everything that grows over time, so the same few methods work everywhere:
`watch_file` (exists), a process's output, SSE and later WebSocket messages.

- `s.text()`, `s.lines()` (complete lines only, as `watch_file` now does), and
  `s.wait_for(|line| …, within: "5s")` for "wait until the server says it's ready".
- Reading never blocks forever; everything has a deadline, like `poll`.

### Pipes

Mostly not needed as real pipes: `send = first.stdout` passes one command's output to the next,
buffered, which is what a test usually wants and what reports can show. Real streaming
(`spawn(a).pipe_to(spawn(b))`) only if a use case needs it.

### Files

- **A temporary directory per case**, `temp_dir()`, removed with the case's undo scope and
  unique per parallel case. The natural `Cwd` for commands under test.
- **Directory snapshots**: `dir("out")` as a map of path → content (or hash for binaries), so
  `expect dir("out") == dir("expected")` shows a diff of files. Later golden files, updated
  with a flag, like other snapshot test tools.
- Open files with read/write positions: only if a real case needs them; whole-file reads,
  `watch_file` streams and temp dirs cover what tool tests usually do.
- `write_file(…, atomic: true)`: write a temporary file and rename it over the target, for
  folders a service watches that could otherwise read a half-written file. Not the default:
  renaming fails on Docker single-file bind mounts and on Windows while the file is open.

### Platform quirks to design around (the team uses macOS, Linux and Windows)

- **Line endings:** `read_file` is exact, so a fixture checked out with `core.autocrlf` on
  Windows differs from the same file on macOS. `klausur init` could write a `.gitattributes`
  that keeps fixtures as they are; maybe a `text(eol: "lf")` option for comparisons.
- **Case-insensitive file systems** (macOS, Windows by default): `file_exists("Out/A.txt")` is
  true there and false on Linux CI. klausur could warn when a path only matches in another case.
- **Windows file locking:** a file another process has open can't be removed or replaced;
  antivirus scanners hold new files briefly. File undos could retry these like transient HTTP
  errors.
- **Finding executables:** `PATHEXT` on Windows (`tool` → `tool.exe` / `tool.cmd`); scripts
  with a shebang don't run on Windows; quoting rules differ (solved by not using a shell).
- **Stopping processes:** SIGTERM vs. `CTRL_BREAK` / `TerminateProcess`; process groups vs. Job
  objects.
- **Output encoding:** some Windows tools write the console code page, not UTF-8; text is
  decoded lossily, `stdout_blob` has the bytes.
- **Timestamps:** mtime granularity differs (ext4 nanoseconds, HFS+ 1 s, FAT 2 s); a tool that
  checks "has this changed?" by mtime may miss a change written within the same second.
- **Also:** symlinks need privileges on Windows, paths over 260 characters on Windows,
  environment variable names are case-insensitive on Windows, no executable bit on Windows.

### Order

1. `EXEC` blocks and `exec()`, buffered, with `temp_dir()`: enough to test a CLI.
2. `spawn` and streams, with stop-on-undo: daemons and "wait until ready".
3. Directory snapshots and golden files.
4. Streaming pipes and open-file handles, only on demand.
