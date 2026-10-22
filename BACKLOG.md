# Backlog

Ideas for after the first release. [GAPS.md](GAPS.md) lists the smaller open items found while
testing ai-search.

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
1. Spike against the ai-search frontend: open, click, fill, text, wait, and a screenshot on
   failure. A few days' work.
2. Settle the API from what the spike shows, then add frames, file uploads, and network
   interception and mocking.
3. Playwright-level robustness, meaning full checks that an element can actually be clicked,
   shadow DOM, and trace files, is a much larger project; take it on only if needed.
