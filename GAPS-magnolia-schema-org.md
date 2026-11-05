# Gaps found by testing a Magnolia site (schema.org JSON-LD)

Writing a suite for the schema.org work of `fastforward` and `magnolia-module-online-marketing`: 54 tests against a
local Magnolia author instance, rendered pages read over HTTP, content changed over Magnolia's REST API with undo, one
browser test in the Vaadin admincentral. The suite lives in `fastforward/src/test/klausur/` (branch
`feature/FF-513-schema-org-offer-product`). Experiment: cover the reviewer test steps of merge requests with tests.

What worked well: `rollback` plus undo on every content change left the repository exactly as before over many runs;
`poll(until:)` waited for Magnolia to pick up a template written with `write_file`; failure reports with `where:` and
the screenshot found every problem on the first look; strict locators caught a race (below) with a clear message.

## Bugs and rough edges

All of these are addressed in klausur 0.3; the last column says how. The suite on the branch uses them.

| Problem | Suggestion | Done |
|---|---|---|
| **`page.navigate(url)` hangs when only the `#fragment` changes**: it gave up after 30 s with "the page hasn't started loading". A fragment change is a same-document navigation, no load event follows. Needed for single page apps that route by fragment (Magnolia's admincentral, Vaadin) | Treat a same-document navigation as done (CDP `Page.navigatedWithinDocument`), or document it and offer `page.reload()` | `navigate` returns at once when only the fragment changes (Chrome reports no new loader) |
| No `page.reload()`; had to use `page.evaluate("location.hash = …; location.reload()")` | Add `page.reload()` | `page.reload()`, waiting like `navigate` |
| `html_select(...)[i].html` is the **outer** HTML; I expected the inner HTML (to parse a `<script type="application/ld+json">`). `text` works, but for elements with markup inside the inner HTML is missing | Name both: `inner_html` and `outer_html`, and say which one `html` is in the docs | `inner_html`/`outer_html` (and `inner_xml`/`outer_xml`); `html` stays the outer markup. Plus a `json_ld(html)` built-in |
| `call` as a parameter name is a load error. Hit it twice in one session (`render_macro(file, call)`, `document_of(call)`), because "macro call" is the natural word | The message is clear; maybe suggest a name ("`call` is reserved in Rhai, e.g. `macro_call`") | The load error suggests names for `call`, `match`, `case`, `default` and `new` |
| A known failed request is reported on every browser failure: Magnolia answers the login page with 401, so "failed requests: 401 GET …/admincentral" is noise in every report | A way to ignore expected failures, e.g. `browser(ignore_requests: ["**/admincentral"])`, or not listing a 401 that rendered a page | `browser(ignore_requests: [globs])` |
| URL interpolation encodes every value after the first, so every path segment that is itself a path (`/fastforward/demo/main`) needs `raw(...)`. Easy to get right once known, but every request of the suite has two or three `raw()` | A `path(x)` helper that encodes segments but keeps `/`, or keep `/` for values starting with `/` | `url_path(x)`: segments encoded, slashes kept (named `url_path`, since `path` is a common variable name) |
| `write_file` + `undo(|| remove_file(p))` is the pattern for every fixture file; if the file existed before, the test destroys it | `write_file(path, content, undo: true)` that removes a new file and restores an existing one | `write_file(path, content, undo: true)` |
| No current date/time builtin; I wanted to check the server log of the day (`catalina.<yyyy-mm-dd>.log`) for new template errors | `now()` / `today()` with formatting | `now()`, `now(format)`, `today()`, in local time |

## Wishes from the experiment

Both are in 0.3: `watch_file(path)` (`.lines()` returns what was appended since), which the suite
uses for "no new errors in the log" on every test, and `json_ld(html)`.

- **Checking the log of the service under test**: "no new errors in the log" is a reviewer step in every merge request
  template here. A modifier like `this.around(|ctx, run| { let before = …; run.call(); … })` can do it, but a built-in
  "tail this file during the case" (`watch_file(path)` returning the lines appended since) would make it one line.
- **JSON-LD / microdata**: `json_ld(html)` was the first helper of the suite. Probably common enough for SEO tests to
  be a built-in, but a five-line helper is fine too.
