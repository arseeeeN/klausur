//! Minimal terminal styling that respects `NO_COLOR` and non-terminal output.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};

static COLOR: AtomicBool = AtomicBool::new(false);

pub fn init() {
    let enabled = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
    COLOR.store(enabled, Ordering::Relaxed);
}

fn paint(code: &str, text: impl std::fmt::Display) -> String {
    if COLOR.load(Ordering::Relaxed) {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn red(t: impl std::fmt::Display) -> String {
    paint("31", t)
}
pub fn green(t: impl std::fmt::Display) -> String {
    paint("32", t)
}
pub fn yellow(t: impl std::fmt::Display) -> String {
    paint("33", t)
}
pub fn cyan(t: impl std::fmt::Display) -> String {
    paint("36", t)
}
pub fn dim(t: impl std::fmt::Display) -> String {
    paint("2", t)
}
pub fn bold(t: impl std::fmt::Display) -> String {
    paint("1", t)
}
