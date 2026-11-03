//! Network mocking: answer requests whose URL matches a glob with a fixed response, or abort
//! them, like Playwright's `route.fulfill` and `route.abort`.
//!
//! Playwright's handlers are callbacks. klausur's routes are static responses instead: Chrome
//! holds a request until it's answered, and answering from the test thread would deadlock
//! whenever that thread is itself waiting on Chrome (e.g. during navigation). The connection's
//! reader thread answers them.

use regex::Regex;
use serde_json::{Value as Json, json};

pub struct Route {
    pub glob: String,
    regex: Regex,
    action: Action,
}

pub enum Action {
    Fulfil {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Abort,
}

impl Route {
    pub fn new(glob: &str, action: Action) -> Route {
        Route {
            glob: glob.to_string(),
            regex: glob_regex(glob),
            action,
        }
    }

    pub fn matches(&self, url: &str) -> bool {
        self.regex.is_match(url)
    }

    /// The protocol call that answers a paused request.
    pub fn answer(&self, request_id: &str) -> (&'static str, Json) {
        match &self.action {
            Action::Abort => (
                "Fetch.failRequest",
                json!({ "requestId": request_id, "errorReason": "Failed" }),
            ),
            Action::Fulfil {
                status,
                headers,
                body,
            } => (
                "Fetch.fulfillRequest",
                json!({
                    "requestId": request_id,
                    "responseCode": status,
                    "responseHeaders": headers
                        .iter()
                        .map(|(k, v)| json!({ "name": k, "value": v }))
                        .collect::<Vec<_>>(),
                    "body": crate::runtime::builtins::base64_encode(body, false),
                }),
            ),
        }
    }
}

/// Playwright's URL globs, matched against the whole URL: `*` matches within a path segment,
/// `**` across segments, `{a,b}` either; everything else, `?` included, is literal.
pub fn glob_regex(glob: &str) -> Regex {
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    let mut in_group = false;
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                re.push_str(".*");
            }
            '*' => re.push_str("[^/]*"),
            '{' => {
                in_group = true;
                re.push_str("(?:");
            }
            '}' if in_group => {
                in_group = false;
                re.push(')');
            }
            ',' if in_group => re.push('|'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).unwrap_or_else(|_| Regex::new("$^").expect("never matches"))
}

/// The pattern Chrome pauses requests for: a coarse version of the glob (Chrome's `*` matches
/// anything, `?` one character), so only likely matches stop at klausur.
pub fn cdp_pattern(glob: &str) -> String {
    let mut out = String::new();
    for c in glob.chars() {
        match c {
            '*' if out.ends_with('*') => {}
            '*' => out.push('*'),
            '?' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            // `{a,b}` has no equivalent; widen it to anything.
            '{' | '}' | ',' => {
                if !out.ends_with('*') {
                    out.push('*');
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_playwright() {
        let m = |glob: &str, url: &str| glob_regex(glob).is_match(url);
        assert!(m("**/api/chat", "http://localhost:5173/api/chat"));
        assert!(!m("**/api/chat", "http://localhost:5173/api/chat/stream"));
        assert!(m("**/api/chat*", "http://localhost:5173/api/chat?x=1"));
        assert!(m("**/search?q=*", "http://h/search?q=otter"));
        assert!(!m("**/search?q=*", "http://h/searchXq=otter"));
        assert!(m("http://h/*.png", "http://h/a.png"));
        assert!(!m("http://h/*.png", "http://h/a/b.png"));
        assert!(m("**/*.{png,jpg}", "http://h/a/b.jpg"));
        assert_eq!(cdp_pattern("**/search?q=*"), "*/search\\?q=*");
    }
}
