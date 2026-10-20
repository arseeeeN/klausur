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
| The HTTP method couldn't be templated, so POST and PUT variants of an endpoint needed two request blocks | `{{method}} https://…` in `connection` |
| Nothing to query XML or HTML with | `html_select(html, css)`, `xml_select(xml, css)`, `html_text(html)` |
| "Every `url` anywhere in the body" needed a recursive helper | `json_path(body, "$..url")` |
| `print` showed Rhai syntax (`#{"a": 1}`), `log` JSON | Both show JSON, and so does `${…}` |
| `trim()`, `sort()` and `reverse()` change in place and return `()`, so `let t = s.trim()` is `()` | `trimmed()`, `sorted()`, `reversed()` return copies |
| `poll` could only wait for something to happen, not check that it doesn't | `poll(while: …, for: "15s")` |
| A failed `expect xs.all(…)` or `expect !x.contains(y)` only said "the expression is false" | The report lists the parts' values under `where:`, and the elements that broke `all` or matched `some` |
| Tests ran one at a time, so the watcher tests' waits added up | `parallel` marks tests that may overlap; `-j N` sets how many. Unmarked tests still run alone |
| The request timeout was a fixed 30 s, and a timed-out request left server state behind without an undo | `--timeout SECS`, the per-request `timeout("90s")` modifier, and timed-out requests register their undo |

## Still open

1. **Rhai quirks still worth documenting:** `call` is a reserved word, and `try` is only a
   statement.
2. **Streaming assertions.** SSE bodies are read completely before parsing, so time to first
   event can't be measured or asserted.
3. **Per-group concurrency limits.** `-j` applies to the whole run. LLM tests and
   indexing tests would want different limits (`parallel("llm", 1)`), and so would a service
   with a rate limiter. Running ai-search's suite showed the limiter, not test concurrency,
   was the bottleneck.
