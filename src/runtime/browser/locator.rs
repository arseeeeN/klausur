//! Locators, as in Playwright: a page plus a chain of steps (CSS, `nth`, text and visibility
//! filters) that is resolved again each time it's used, so it never goes stale.
//!
//! Actions and reads that target one element are strict: when the chain matches several, they
//! fail at once and list the matches. Actions wait until their element can take them.

use super::page::Page;
use super::{R, fail};
use crate::runtime::Shared;
use rhai::{Array, Dynamic};
use serde_json::{Value as Json, json};
use std::rc::Rc;

/// The in-page script that resolves chains and checks elements.
const DOM: &str = include_str!("dom.js");

#[derive(Clone)]
pub struct Locator {
    pub(super) page: Page,
    steps: Rc<Vec<Json>>,
    /// The chain as Playwright prints selectors: `.result >> nth=0`.
    desc: Rc<str>,
}

impl Locator {
    pub fn css(page: &Page, selector: &str) -> Locator {
        Locator {
            page: page.clone(),
            steps: Rc::new(vec![json!({ "css": selector })]),
            desc: selector.into(),
        }
    }

    fn then(&self, step: Json, desc: String) -> Locator {
        let mut steps = (*self.steps).clone();
        steps.push(step);
        Locator {
            page: self.page.clone(),
            steps: Rc::new(steps),
            desc: format!("{} >> {desc}", self.desc).into(),
        }
    }

    pub fn locator(&self, selector: &str) -> Locator {
        self.then(json!({ "css": selector }), selector.to_string())
    }

    pub fn nth(&self, i: i64) -> Locator {
        self.then(json!({ "nth": i }), format!("nth={i}"))
    }

    pub fn has_text(&self, text: &str, not: bool) -> Locator {
        let desc = if not { "has-not-text" } else { "has-text" };
        self.then(
            json!({ "text": text, "not": not }),
            format!("{desc}={}", json!(text)),
        )
    }

    pub fn visible(&self, visible: bool) -> Locator {
        self.then(json!({ "visible": visible }), format!("visible={visible}"))
    }

    pub fn describe(&self) -> String {
        self.desc.to_string()
    }

    /// Run an operation of the in-page script; `{error}` results fail right away.
    fn run(&self, op: &str, arg: Json) -> R<Json> {
        let js = format!("({DOM})({}, {}, {})", json!(*self.steps), json!(op), arg);
        let v = self.page.eval(&js)?;
        if let Some(msg) = v.get("invalid").and_then(Json::as_str) {
            return Err(fail(format!(
                "`{}` isn't a valid selector: {msg}",
                self.desc
            )));
        }
        if let Some(msg) = v.get("error").and_then(Json::as_str) {
            return Err(fail(format!("`{}`: {msg}", self.desc)));
        }
        Ok(v)
    }

    /// Wait until the chain matches exactly one element for which `ready` has no objection.
    fn one(
        &self,
        shared: &Shared,
        action: &str,
        op: &str,
        arg: Json,
        ready: impl Fn(&Json) -> Option<String>,
    ) -> R<Json> {
        let label = format!("{action}` on `{}", self.desc);
        self.page.wait_for(shared, &label, || {
            let v = self.run(op, arg.clone())?;
            match v["count"].as_u64().unwrap_or(0) {
                0 => Ok(Err("nothing matches it".into())),
                1 => Ok(match ready(&v) {
                    Some(why) => Err(why),
                    None => Ok(v),
                }),
                n => Err(self.strict(n, &v)),
            }
        })
    }

    fn strict(&self, n: u64, v: &Json) -> Box<rhai::EvalAltResult> {
        let matches: Vec<&str> = v["matches"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .collect();
        fail(format!(
            "`{}` matches {n} elements, but this needs one; narrow it down, or pick one with \
             `.first()`, `.last()` or `.nth(i)`. It matches {}{}",
            self.desc,
            matches.join(", "),
            if n as usize > matches.len() {
                ", …"
            } else {
                ""
            }
        ))
    }

    // --- Actions ----------------------------------------------------------------------------

    pub fn click(&self, shared: &Shared) -> R<()> {
        let stable = Stable::default();
        let v = self.one(shared, "click", "point", Json::Null, |v| {
            not_visible(v)
                .or_else(|| disabled(v))
                .or_else(|| stable.moving(v))
                .or_else(|| covered(v))
        })?;
        let (x, y) = point(&v);
        self.page.mouse("mouseMoved", x, y, 0)?;
        self.page.mouse("mousePressed", x, y, 1)?;
        self.page.mouse("mouseReleased", x, y, 1)
    }

    pub fn hover(&self, shared: &Shared) -> R<()> {
        let stable = Stable::default();
        let v = self.one(shared, "hover", "point", Json::Null, |v| {
            not_visible(v)
                .or_else(|| stable.moving(v))
                .or_else(|| covered(v))
        })?;
        let (x, y) = point(&v);
        self.page.mouse("mouseMoved", x, y, 0)
    }

    pub fn fill(&self, shared: &Shared, text: &str) -> R<()> {
        self.one(shared, "fill", "fill", Json::Null, |v| {
            not_visible(v).or_else(|| disabled(v)).or_else(|| {
                (v["editable"] == false).then(|| format!("{} is read-only", element(v)))
            })
        })?;
        // The script selected the current text; typing replaces it, with real input events.
        if text.is_empty() {
            self.page.press("Backspace")
        } else {
            self.page
                .call("Input.insertText", json!({ "text": text }))
                .map(|_| ())
        }
    }

    /// Focus the element and press a key there.
    pub fn press(&self, shared: &Shared, key: &str) -> R<()> {
        self.one(shared, "press", "focus", Json::Null, |_| None)?;
        self.page.press(key)
    }

    /// Check or uncheck a checkbox or radio button by clicking it, unless it already is.
    pub fn set_checked(&self, shared: &Shared, want: bool) -> R<()> {
        let action = if want { "check" } else { "uncheck" };
        let stable = Stable::default();
        let v = self.one(shared, action, "point", json!("check"), |v| {
            if v["checked"] == want {
                return None;
            }
            not_visible(v)
                .or_else(|| disabled(v))
                .or_else(|| stable.moving(v))
                .or_else(|| covered(v))
        })?;
        if v["checked"] == want {
            return Ok(());
        }
        let (x, y) = point(&v);
        self.page.mouse("mouseMoved", x, y, 0)?;
        self.page.mouse("mousePressed", x, y, 1)?;
        self.page.mouse("mouseReleased", x, y, 1)?;
        if self.run("checked", Json::Null)?["checked"] != want {
            return Err(fail(format!(
                "`{action}` on `{}`: clicking it didn't change its state",
                self.desc
            )));
        }
        Ok(())
    }

    /// Select options of a `<select>` by value or label; returns the selected values.
    pub fn select(&self, shared: &Shared, values: Vec<String>) -> R<Array> {
        let v = self.one(shared, "select", "select", json!(values), |v| {
            not_visible(v).or_else(|| disabled(v)).or_else(|| {
                v["missing"].as_array().map(|m| {
                    let m: Vec<String> = m.iter().map(|o| o.to_string()).collect();
                    format!("it has no option with the value or label {}", m.join(", "))
                })
            })
        })?;
        Ok(v["selected"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(|s| Dynamic::from(s.to_string())))
            .collect())
    }

    /// Set the files of an `<input type="file">`, as choosing them in the file dialog would.
    pub fn upload(&self, shared: &Shared, paths: Vec<String>) -> R<()> {
        let files: Vec<String> = paths
            .iter()
            .map(|p| {
                let full = shared.resolve_path(p);
                if !full.is_file() {
                    return Err(fail(format!("can't upload {p}: there's no such file")));
                }
                Ok(dunce::canonicalize(&full)
                    .unwrap_or(full)
                    .display()
                    .to_string())
            })
            .collect::<R<_>>()?;
        self.one(shared, "upload", "file", json!(files.len()), |_| None)?;
        let js = format!("({DOM})({}, 'node', null)", json!(*self.steps));
        let node = self.page.evaluate_raw(&js, false)?;
        let Some(object) = node["result"]["objectId"].as_str() else {
            return Err(fail(format!(
                "`upload` on `{}`: the element went away",
                self.desc
            )));
        };
        self.page
            .call(
                "DOM.setFileInputFiles",
                json!({ "files": files, "objectId": object }),
            )
            .map(|_| ())
    }

    /// Wait for a state: "visible" (the default), "hidden", "attached" or "detached".
    pub fn wait(&self, shared: &Shared, state: &str) -> R<()> {
        if !["visible", "hidden", "attached", "detached"].contains(&state) {
            return Err(fail(format!(
                "`wait` takes the state \"visible\", \"hidden\", \"attached\" or \"detached\", not {}",
                json!(state)
            )));
        }
        let label = format!("wait` on `{}", self.desc);
        self.page.wait_for(shared, &label, || {
            let v = self.run("state", Json::Null)?;
            let n = v["count"].as_u64().unwrap_or(0);
            if n > 1 {
                return Err(self.strict(n, &v));
            }
            let visible = n == 1 && v["visible"] == true;
            Ok(match state {
                "visible" if visible => Ok(()),
                "hidden" if !visible => Ok(()),
                "attached" if n == 1 => Ok(()),
                "detached" if n == 0 => Ok(()),
                "visible" | "attached" if n == 0 => Err("nothing matches it".into()),
                "visible" => Err(format!("{} isn't visible", element(&v))),
                "hidden" => Err(format!("{} is visible", element(&v))),
                _ => Err(format!("{} is still there", element(&v))),
            })
        })
    }

    // --- Reads ------------------------------------------------------------------------------

    /// The element's rendered text (`innerText`), once there is one.
    pub fn text(&self, shared: &Shared) -> R<String> {
        let v = self.one(shared, "text", "text", Json::Null, |_| None)?;
        Ok(v["text"].as_str().unwrap_or_default().to_string())
    }

    /// The value of an input, textarea or select, once there is one.
    pub fn value(&self, shared: &Shared) -> R<String> {
        let v = self.one(shared, "value", "value", Json::Null, |_| None)?;
        Ok(v["value"].as_str().unwrap_or_default().to_string())
    }

    pub fn checked(&self, shared: &Shared) -> R<bool> {
        let v = self.one(shared, "checked", "checked", Json::Null, |_| None)?;
        Ok(v["checked"] == true)
    }

    /// The texts of all matches, right away.
    pub fn texts(&self, shared: &Shared) -> R<Array> {
        shared.page_reads.set(shared.page_reads.get() + 1);
        Ok(self.run("texts", Json::Null)?["texts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str().map(|s| Dynamic::from(s.to_string())))
            .collect())
    }

    /// How many elements match, right away.
    pub fn count(&self, shared: &Shared) -> R<i64> {
        shared.page_reads.set(shared.page_reads.get() + 1);
        Ok(self.run("count", Json::Null)?["count"]
            .as_i64()
            .unwrap_or(0))
    }

    /// Whether the element is visible, right away; false when nothing matches.
    pub fn is_visible(&self, shared: &Shared) -> R<bool> {
        shared.page_reads.set(shared.page_reads.get() + 1);
        let v = self.run("state", Json::Null)?;
        match v["count"].as_u64().unwrap_or(0) {
            0 => Ok(false),
            1 => Ok(v["visible"] == true),
            n => Err(self.strict(n, &v)),
        }
    }
}

/// Playwright clicks an element only once its box stayed the same for two animation frames, so
/// a click doesn't land where a sliding or growing element was a moment ago. klausur compares
/// the boxes of consecutive probes (25 ms apart) instead: animation frames stop in background
/// tabs, which `--headed` runs with several pages have.
#[derive(Default)]
struct Stable(std::cell::RefCell<Option<Json>>);

impl Stable {
    fn moving(&self, v: &Json) -> Option<String> {
        let now = v.get("box").cloned();
        let before = self.0.replace(now.clone());
        (now.is_none() || before != now).then(|| format!("{} is still moving", element(v)))
    }
}

fn element(v: &Json) -> String {
    v["element"].as_str().unwrap_or("the element").to_string()
}

fn not_visible(v: &Json) -> Option<String> {
    (v["visible"] != true).then(|| format!("{} isn't visible", element(v)))
}

fn disabled(v: &Json) -> Option<String> {
    (v["enabled"] != true).then(|| format!("{} is disabled", element(v)))
}

fn covered(v: &Json) -> Option<String> {
    v["covered_by"].as_str().map(|by| {
        format!(
            "{} is covered by {by}, which would get the click",
            element(v)
        )
    })
}

fn point(v: &Json) -> (f64, f64) {
    (
        v["x"].as_f64().unwrap_or_default(),
        v["y"].as_f64().unwrap_or_default(),
    )
}
