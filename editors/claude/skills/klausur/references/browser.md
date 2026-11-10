# Browser tests

klausur 0.2+ drives Chrome (or Chromium) over the DevTools protocol. The API follows
Playwright's; the full description is SPEC §14
(https://raw.githubusercontent.com/arseeeeN/klausur/main/SPEC.md, search "## 14.").

Browser tests combine with everything else: set data up through API requests (fast, with undo),
check it in the UI, and let `rollback` clean up.

```
test "a new user shows up in the admin list" rollback logged_in() tags(["ui"]) {
    let name = unique("alice")
    createUser(name, jwt: ctx.jwt)                        // setup through the API
    let page = browser().open(`${ui}/admin/users`)
    page.fill("input[type=search]", name)
    page.press("input[type=search]", "Enter")
    expect page.locator(".user-row").texts().contains(name)   // retried until it holds
}
```

Requirements: Chrome or Chromium installed (found in the usual places or on `PATH`; `CHROME`
overrides), and the app's UI running. Tag UI tests (`tags(["ui"])`) so API-only runs can skip
them: `--tag ui` runs only tagged tests. `--headed` shows the window.

## Browser and pages

- `browser()`: the case's browser context (like an incognito window). Cases share no cookies or
  storage; pages close when the case ends, after its undo actions.
- `browser(timeout: "10s", expect_timeout: "30s")`: actions wait up to `timeout` (default 30 s),
  page expectations retry for `expect_timeout` (default 5 s). Raise `expect_timeout` for slow
  things such as LLM answers. `ignore_requests: ["**/login*"]` (0.3+) keeps expected
  failures (a login page served with 401) out of failure reports.
- `browser().open(url)`: a new page, loaded, with the network quiet for a moment (frameworks
  attach click handlers after `load`).
- `page.navigate(url)` (when only the `#fragment` changes it returns at once: same document, no
  load; before 0.3 it hung), `page.reload()` (0.3+; for apps that read the fragment only on load,
  such as Vaadin: `navigate` to the fragment, then `reload`), `page.url`, `page.title`, `page.html()` (feed it to `html_select`),
  `page.evaluate(js)` (result as a value; promises awaited), `page.console()` (errors, warnings,
  dialogs), `page.screenshot(path)`, `page.close()`.
- Dialogs (`alert`, `confirm`) are dismissed unless `page.accept_dialogs()` was called.

`goto` and `eval` are reserved words in Rhai, hence `navigate` and `evaluate`.

## Locators

`page.locator(css)` describes elements and is re-resolved on every use, so it never goes stale.
Narrow it with `.locator(css)`, pick with `.first()`, `.last()`, `.nth(i)` (negative from the
end), filter with `.filter(has_text: "…", has_not_text: "…", visible: true)` (text matching is
case-insensitive). CSS reaches into open shadow roots: `my-widget input` finds the input inside
`<my-widget>`'s shadow DOM.

Every element method exists on locators and, with a selector first, on pages:
`page.click(".x")` is `page.locator(".x").click()`. A locator's methods take **no** selector:
`widget.locator("textarea").fill(q)`, not `widget.fill("textarea", q)`.

| Method | Does | Waits until the element is |
|---|---|---|
| `click()` | Real mouse click at its centre, after scrolling into view | visible, enabled, still, not covered |
| `hover()` | Moves the mouse over it | visible, still, not covered |
| `fill(text)` | Replaces an input's/textarea's/editable's text, as typing | visible, enabled, editable |
| `press(key)` | Focuses it and presses `"Enter"`, `"a"`, `"Shift+Tab"`, `"Control+a"` | attached |
| `check()`, `uncheck()` | Clicks a checkbox/radio unless already in that state | visible, enabled, still, not covered |
| `select(v)` | Selects `<select>` options by value or label (string or array); returns values | visible, enabled, options present |
| `upload(path)` | Sets an `<input type=file>`'s files (path or array, relative to the test file) | attached |
| `wait()` / `wait(state: "hidden")` | States: `visible` (default), `hidden`, `attached`, `detached` | in that state |
| `text()` | Rendered text (`innerText`), trimmed | attached |
| `value()` | An input's/textarea's/select's value | attached |
| `checked()` | Checkbox/radio state | attached |
| `texts()` | Texts of all matches, right away | (no wait) |
| `count()` | Number of matches, right away | (no wait) |
| `visible()` | Whether it's visible, right away; false if nothing matches | (no wait) |

`page.press(key)` presses at the current focus.

**Strict**: a method that targets one element fails at once when the locator matches several,
listing the matches. Fix the selector, or pick one with `.first()` / `.nth(i)`; use `texts()` or
`count()` for lists. Mistakes such as filling a checkbox also fail at once instead of waiting.

## Waiting and expectations

Don't `sleep`. Actions wait for their element. `expect` **retries** an expectation that read the
page (until `expect_timeout`) while it's false, so these wait for the page to catch up:

```
expect page.texts(".result").contains(word)
expect page.count(".row") == 3
expect !page.visible(".spinner")
expect page.url.ends_with("/done")
```

It isn't retried if it also sent an API request (which would be sent again): do API calls
before the `expect`.

## Network

- `page.requests()` / `page.requests("**/api/search*")`: what the page requested so far, as
  `#{method, url, body, status, error}`; with `expect` this waits for a request to happen.
- `page.route(glob, response)` answers matching requests without the network, for things the
  test doesn't own (a third-party API, an endpoint another system serves, an error case):

```
page.route("**/api/chat*", #{ status: 500, json: #{ error: "boom" } })
page.route("**/*.png", "abort")
browser().route("http://app.test/**", #{ path: "fixtures/site/index.html" })   // all pages
```

Responses: `#{ status (200), headers, content_type, json | body | path }`, or `"abort"`.
Globs are matched against the whole URL: `*` within a path segment, `**` across, `{a,b}`;
everything else, `?` included, is literal, so `**/search?q=*` matches a query. The newest route
wins; page routes come before browser routes. Routes are fixed responses, not callbacks.

## Failures

A failed case with an open page reports the URL and title, a screenshot in
`klausur-results/<file>-<test>.png` (taken before undo), console errors and failed requests.
`klausur-results/` is emptied at the start of each `klausur test`; suggest adding it to
`.gitignore`. Look at the screenshot (read the PNG) before guessing: it usually shows a wrong
page, an error banner, or a still-loading state.

Common causes:
- `nothing matches it`: wrong selector, wrong page, or the element is rendered by something that
  didn't run (a widget bundle not built, a failed request: check "failed requests").
- `matches N elements`: make the locator specific, or `.first()`.
- `is covered by <div.overlay>`: a modal or cookie banner; close it first.
- An `expect` that "retried for 5 s": the page never got there; the `where:` values show what
  it had instead. For slow backends raise `expect_timeout`.
