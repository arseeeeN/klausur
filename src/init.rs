//! `klausur init`: scaffold a suite that loads and runs right away, so the first minutes go to
//! the API under test rather than to klausur's layout.

use crate::style::{bold, dim, green, yellow};
use clap::ValueEnum;
use std::path::Path;

#[derive(Clone, Copy, ValueEnum)]
pub enum Auth {
    /// `Authorization: Basic …` from `API_USER` / `API_PASSWORD`.
    Basic,
    /// A token from a login request, sent as `Authorization: Bearer …`.
    Bearer,
}

const API_HEAD: &str = r#"// Requests against the service under test, one block per endpoint.
// BASE_URL is a placeholder: rename it after the service when a suite talks to several.

let base = env("BASE_URL", "http://localhost:8080")
"#;

const HEALTH: &str = r#"
// An example to replace: an endpoint any running instance answers.
@health() {
    connection {
        GET {{base}}/health
    }
    expect status == 200
}
"#;

const BASIC: &str = r#"
// Basic auth for `logged_in()` in lib.kl. The defaults are for a local instance only.
let auth = `Basic ${base64_encode(env("API_USER", "user") + ":" + env("API_PASSWORD", "password"))}`
"#;

const BEARER: &str = r#"
// A token for `logged_in()` in lib.kl. Adjust the path and the fields to the service's login;
// the defaults are for a local instance only.
@login(user = env("API_USER", "user"), password = env("API_PASSWORD", "password")) {
    connection {
        POST {{base}}/login
    }
    send { "username": {{user}}, "password": {{password}} }
    expect status == 200
    let token = body.token
}
"#;

const LIB_HEAD: &str = r#"// Helpers, fixtures and modifiers the test files share. All loaded files share one namespace,
// so shared functions belong here. Built in, no need to define: unique("name") → "name-3f9c2a1b".

import "api.kl"
"#;

const LOGGED_IN_BASIC: &str = r#"
// Sends the credentials with every request of a case: test "…" rollback logged_in() { … }
fn logged_in() {
    this.before(|ctx| { ctx.headers["Authorization"] = auth })
}
"#;

const LOGGED_IN_BEARER: &str = r#"
// Logs in before each case and sends the token with every request of the case:
// test "…" rollback logged_in() { … }
fn logged_in() {
    this.before(|ctx| { ctx.headers["Authorization"] = `Bearer ${login().token}` })
}
"#;

const LOGGED_IN_EXAMPLE: &str = r#"
// A login modifier to adapt (or `klausur init --auth basic|bearer` in an empty folder): it logs
// in before each case and sends the token with every request of the case.
//
// fn logged_in() {
//     this.before(|ctx| { ctx.headers["Authorization"] = `Bearer ${login().token}` })
// }
"#;

const EXAMPLE_TEST: &str = r#"// An example to replace. Name tests after the behaviour they check; use `rollback` for any test
// that creates state, so its requests' undo actions clean up after it.

import "lib.kl"

test "the service answers" {
    expect health().status == 200
}
"#;

fn readme(dir: &str, auth: Option<Auth>) -> String {
    let auth_rows = if auth.is_some() {
        "| `API_USER` | `user` | Login for `logged_in()` |\n| `API_PASSWORD` | `password` | |\n"
    } else {
        ""
    };
    format!(
        r#"# API tests

End-to-end tests for the service, written for [klausur](https://github.com/arseeeeN/klausur).
They run against a running instance and undo what they create.

## Running

Start the service, then:

```bash
klausur test {dir}               # everything
klausur test {dir} "answers"     # tests whose name contains "answers"
klausur test {dir} --watch       # again on every change, while writing tests
```

| Variable | Default | Used for |
|---|---|---|
| `BASE_URL` | `http://localhost:8080` | The service under test |
{auth_rows}
Set them in the environment, or in a `.env` file where you run klausur.

## Writing tests

- `api.kl`: one request block per endpoint, with what a success looks like (`expect`) and how
  to undo it (`undo`).
- `lib.kl`: helpers and modifiers the test files share.
- `*.test.kl`: the tests, one file per area of the API.

The [guide](https://github.com/arseeeeN/klausur/blob/main/docs/guide.md) shows the patterns, and
how to turn a ticket into tests.
"#
    )
}

/// Write the scaffold into `dir`; existing files are kept. Returns the exit code.
pub fn init(dir: &Path, auth: Option<Auth>) -> i32 {
    let mut api = String::from(API_HEAD);
    let mut lib = String::from(LIB_HEAD);
    match auth {
        Some(Auth::Basic) => {
            api.push_str(BASIC);
            lib.push_str(LOGGED_IN_BASIC);
        }
        Some(Auth::Bearer) => {
            api.push_str(BEARER);
            lib.push_str(LOGGED_IN_BEARER);
        }
        None => lib.push_str(LOGGED_IN_EXAMPLE),
    }
    api.push_str(HEALTH);
    let shown = dir.display().to_string();
    let files = [
        ("api.kl", api),
        ("lib.kl", lib),
        ("example.test.kl", EXAMPLE_TEST.to_string()),
        ("README.md", readme(&shown, auth)),
        (".gitignore", "klausur-results/\n".to_string()),
    ];
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("can't create {shown}: {e}");
        return 2;
    }
    let mut skipped = Vec::new();
    for (name, content) in files {
        let path = dir.join(name);
        if path.exists() {
            skipped.push(name);
            continue;
        }
        if let Err(e) = std::fs::write(&path, content) {
            eprintln!("can't write {}: {e}", path.display());
            return 2;
        }
        println!("  {} {}", green("created"), path.display());
    }
    for name in &skipped {
        println!(
            "  {} {} {}",
            yellow("kept"),
            dir.join(name).display(),
            dim("(exists)")
        );
    }
    println!(
        "\n{}\n  1. Set BASE_URL (or start the service on localhost:8080)\n  \
         2. klausur test {shown}\n  3. Replace the example with requests and tests of your own",
        bold("Next:")
    );
    0
}
