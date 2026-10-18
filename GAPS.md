# Gaps found by testing ai-search

Writing a full suite for `../ai-search` (JWT roles, multipart uploads, SSE chat streams, LLM
answers, path-based ACLs) showed what klausur was missing. The suite lives in
`ai-search/src/test/klausur/`.

## Fixed while writing the suite

| Problem | Fix |
|---|---|
| **Bug:** a request consumed its arguments. A variable passed as the first argument was `()` on its next use, so the second request with the same token sent `Bearer ` | Arguments are cloned (`tests/e2e.rs: arguments_are_not_consumed`) |
| **Bug:** call traces showed bogus locations for calls inside generated wrappers (`name__kw`) | Trace entries are only taken from user code |
| **Bug:** a closure in a function didn't capture a parameter when any file defined a function with the same name (`fn writer()` in one file broke `\|\| deleteByUrl(writer, path)` in another; Rhai skips capturing names of functions parsed earlier in a compilation) | Each function is compiled separately, then merged into the library |
| Hooks resolved relative paths against the working directory, not the test file | Hooks run with the test file's directory |
| No multipart bodies; only text files could be uploaded, by hand-building the body | `send = multipart(#{ field: "x", file: file("a.pdf") })`, `file(path)`, `file_from(name, content[, type])`, `read_blob(path)`; `send = file(...)` sends raw bytes with a guessed content type |
| Couldn't leave out a header; `Authorization: Bearer {{jwt}}` with no token sent `Bearer ` | A header with a null interpolation is not sent |
| No SSE support | `text/event-stream` responses get `events`: `[#{event, data, id}]` with JSON data parsed. `sse_parse(text)` for other text |
| No base64 / JWT decoding (a 25-line Rhai decoder was needed) | `base64_encode`, `base64url_encode`, `base64_decode`, `base64_decode_blob`, `jwt_decode(token)` → `#{header, claims, signature}` |
| No regular expressions | `regex_match`, `regex_find`, `regex_find_all`, `regex_captures` (named groups become a map), `regex_replace` |
| `ctx().answer = x` isn't valid Rhai (can't assign to a call result) | `ctx` is a variable in test bodies |
| No way to wait for background work; a test needed a hand-written loop with `sleep` | The `poll(until:, every:, within:)` request modifier |
| No way to write files, e.g. into a folder the service watches | `write_file`, `remove_file`, `file_exists` |
| Only requests could register cleanup | `undo(\|\| …)` puts a closure on the same undo stack |
| `sleep` ignored Ctrl-C and the test's `timeout` | It wakes up for both |
| Map-valued case arguments made labels unreadable (`acl={"uri":[{"allow":…`) | The `label(expr)` test modifier; maps and lists shrink to `{…}`/`[…]` otherwise; `--case` matches labels |
| The request timeout was a fixed 30 s, and a timed-out request left server state behind without an undo | `--timeout SECS`, the per-request `timeout("90s")` modifier, and timed-out requests register their undo |

## Still open

Roughly by how much they'd help:

1. **The HTTP method can't be templated.** POST and PUT variants of the same endpoint need two
   request blocks (`indexFile` / `updateFile`).
2. **XML / HTML traversal.** Not needed for this JSON API, but a service returning XML, or a test
   wanting to read a fixture page's text, has nothing to query with. Candidates: `xml_parse`
   into maps, or CSS selectors over HTML (`html_select(text, "main p")`).
3. **Deep JSON queries.** Closures work (`body.chatResponse.documents.map(|d| d.url)`), but
   "every `url` anywhere" needs a recursive helper. A small JSONPath (`json_path(body, "$..url")`)
   would read better.
4. **`print` shows Rhai syntax** (`#{"a": 1}`), while `log` shows JSON. They should match.
5. **Rhai quirks that bit while writing tests:**
   - `trim()`, `make_lower()` and `sort()` mutate in place and return `()`
   - `call` is a reserved word
   - `try` is only a statement

   Worth documenting, or wrapping with value-returning helpers (`lower(s)`, `trimmed(s)`).
6. **Parallelism.** The suite takes ~2 min, almost all of it waiting on the LLM and on the
   service's rate limiter. Running independent tests concurrently would cut that down, given a
   per-suite concurrency limit so the limiter isn't hit harder. Most of the file watcher
   tests' time is the worker's 5 s schedule.
7. **Streaming assertions.** SSE bodies are read completely before parsing, so time to first
   event can't be measured or asserted.
8. **Waiting for absence.** `poll` waits until something holds. "This file is never indexed"
   can't be awaited; the watcher tests drop a second file and wait for that one instead.
