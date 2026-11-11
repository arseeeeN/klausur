//! A cookie jar per test case, for tests marked `cookies`: every request of the case stores the
//! cookies it gets and sends back the ones that apply, like a browser. The rules (domain, path,
//! `Secure`, expiry) are `cookie_store`'s, the crate ureq's own cookie support uses.

use super::value::map_of;
use super::{Failure, Shared, http};
use rhai::{Dynamic, EvalAltResult, Map};
use std::cell::RefCell;
use std::rc::Rc;

pub type Jar = Rc<RefCell<cookie_store::CookieStore>>;

fn jar(shared: &Shared) -> Option<Jar> {
    shared.current_case()?.ctx.0.borrow().cookies.clone()
}

/// Add the jar's cookies for the request's URL, unless the request sets `Cookie` itself.
pub fn attach(shared: &Shared, prepared: &mut http::Prepared) {
    let Some(jar) = jar(shared) else { return };
    if prepared
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("cookie"))
    {
        return;
    }
    let Ok(url) = url::Url::parse(&prepared.url) else {
        return;
    };
    let jar = jar.borrow();
    let mut cookies = jar.matches(&url);
    // A stable order: longer paths first (as browsers do), then by name.
    cookies.sort_by_key(|c| (std::cmp::Reverse(c.path().map_or(0, str::len)), c.name()));
    let value: Vec<String> = cookies
        .iter()
        .map(|c| format!("{}={}", c.name(), c.value()))
        .collect();
    if !value.is_empty() {
        prepared.headers.push(("Cookie".into(), value.join("; ")));
    }
}

/// Keep the cookies a response sets.
pub fn store(shared: &Shared, request_url: &str, headers: &[(String, String)]) {
    let Some(jar) = jar(shared) else { return };
    let Ok(url) = url::Url::parse(request_url) else {
        return;
    };
    let mut jar = jar.borrow_mut();
    for (_, value) in headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
    {
        // A cookie the rules reject (another domain, say) is dropped, as a browser would.
        let _ = jar.parse(value, &url);
    }
}

/// The jar's cookies, name → value: `ctx.cookies`.
pub fn snapshot(jar: &Jar) -> Map {
    map_of(
        jar.borrow()
            .iter_unexpired()
            .map(|c| (c.name().to_string(), Dynamic::from(c.value().to_string()))),
    )
}

/// `set_cookie(url, "name=value; Path=/")`: put a cookie into the jar as if `url` had set it.
pub fn set(shared: &Shared, url: &str, cookie: &str) -> Result<(), Box<EvalAltResult>> {
    let Some(jar) = jar(shared) else {
        return Err(
            Failure::new("`set_cookie` needs the `cookies` modifier on the test").into_err(),
        );
    };
    let parsed = url::Url::parse(url)
        .map_err(|e| Failure::new(format!("`{url}` isn't a URL: {e}")).into_err())?;
    jar.borrow_mut()
        .parse(cookie, &parsed)
        .map(|_| ())
        .map_err(|e| Failure::new(format!("can't set the cookie `{cookie}`: {e}")).into_err())
}
