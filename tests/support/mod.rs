//! A small in-memory API used by the end-to-end tests.

#![allow(dead_code)]

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct State {
    pub next_id: u64,
    pub users: HashMap<u64, Value>,
    pub flaky: HashMap<String, u32>,
    pub deletes: Vec<u64>,
    /// `/hold`: requests in progress, how many ever started, and the most at once.
    pub active: u32,
    pub started: u32,
    pub peak: u32,
}

pub struct Server {
    pub base: String,
    pub state: Arc<Mutex<State>>,
}

pub fn start() -> Server {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    let state = Arc::new(Mutex::new(State::default()));
    let st = state.clone();
    std::thread::spawn(move || {
        // One thread per request, so parallel tests can overlap.
        for mut req in server.incoming_requests() {
            let st = st.clone();
            std::thread::spawn(move || {
                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);
                let (status, value) = handle(&st, &req, &body);
                let text = value.map(|v| v.to_string()).unwrap_or_default();
                let mut response = tiny_http::Response::from_string(text).with_status_code(status);
                response.add_header(
                    tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
                );
                let _ = req.respond(response);
            });
        }
    });
    Server {
        base: format!("http://127.0.0.1:{port}"),
        state,
    }
}

fn header(req: &tiny_http::Request, name: &str) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.to_string())
}

fn handle(state: &Mutex<State>, req: &tiny_http::Request, body: &str) -> (u16, Option<Value>) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let query: HashMap<&str, &str> = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .collect();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let method = req.method().as_str().to_string();
    let json_body: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let mut s = state.lock().unwrap();
    match (method.as_str(), segments.as_slice()) {
        ("GET", ["health"]) => (200, Some(json!({"ok": true}))),
        ("POST", ["users"]) => {
            if let Some(ms) = query.get("slow").and_then(|m| m.parse().ok()) {
                drop(s);
                std::thread::sleep(std::time::Duration::from_millis(ms));
                s = state.lock().unwrap();
            }
            let Some(name) = json_body.get("name").and_then(Value::as_str) else {
                return (400, Some(json!({"error": "name is required"})));
            };
            if name == "conflict" {
                return (409, Some(json!({"error": "conflict"})));
            }
            s.next_id += 1;
            let id = s.next_id;
            let role = json_body.get("role").cloned().unwrap_or(json!("user"));
            let user = json!({"id": id, "name": name, "role": role, "tags": []});
            s.users.insert(id, user.clone());
            (201, Some(user))
        }
        ("GET", ["users"]) => (200, Some(Value::Array(s.users.values().cloned().collect()))),
        ("GET", ["users", id]) => match id.parse().ok().and_then(|id: u64| s.users.get(&id)) {
            Some(u) => (200, Some(u.clone())),
            None => (404, Some(json!({"error": "not found"}))),
        },
        ("DELETE", ["users", id]) => {
            let Ok(id) = id.parse::<u64>() else {
                return (404, None);
            };
            s.deletes.push(id);
            match s.users.get(&id) {
                Some(u) if u["name"].as_str().unwrap_or("").starts_with("undeletable") => {
                    (500, Some(json!({"error": "database is on fire"})))
                }
                Some(_) => {
                    s.users.remove(&id);
                    (204, None)
                }
                None => (404, Some(json!({"error": "not found"}))),
            }
        }
        ("POST", ["login"]) => {
            if json_body["pass"] == "secret" {
                (
                    200,
                    Some(
                        json!({"token": format!("token-{}", json_body["user"].as_str().unwrap_or(""))}),
                    ),
                )
            } else {
                (401, Some(json!({"error": "bad credentials"})))
            }
        }
        ("GET", ["me"]) => match header(req, "Authorization") {
            Some(auth) if auth.starts_with("Bearer token-") => {
                (200, Some(json!({"user": &auth[13..]})))
            }
            _ => (401, Some(json!({"error": "unauthorized"}))),
        },
        ("GET", ["flaky", key]) => {
            let n = s.flaky.entry(key.to_string()).or_insert(0);
            *n += 1;
            if *n <= 2 {
                (503, Some(json!({"error": "try again"})))
            } else {
                (200, Some(json!({"attempts": *n})))
            }
        }
        (_, ["echo", ..]) => {
            let headers: serde_json::Map<String, Value> = req
                .headers()
                .iter()
                .map(|h| {
                    (
                        h.field.as_str().as_str().to_ascii_lowercase(),
                        json!(h.value.as_str()),
                    )
                })
                .collect();
            (
                200,
                Some(
                    json!({"method": method, "path": path, "query": query, "headers": headers, "body": body}),
                ),
            )
        }
        // A job that is done on the `after`-th poll.
        ("GET", ["jobs", key]) => {
            let n = s.flaky.entry(format!("job-{key}")).or_insert(0);
            *n += 1;
            let after: u32 = query.get("after").and_then(|m| m.parse().ok()).unwrap_or(3);
            let state = if *n >= after { "done" } else { "pending" };
            (200, Some(json!({"state": state, "polls": *n})))
        }
        // Slow on the first call, fast afterwards.
        ("GET", ["warmup", key]) => {
            let n = s.flaky.entry(format!("warmup-{key}")).or_insert(0);
            *n += 1;
            let first = *n == 1;
            drop(s);
            if first {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            (200, Some(json!({"ready": true})))
        }
        // Held for `ms`; says whether another `/hold` ran at any point meanwhile.
        ("GET", ["hold"]) => {
            s.active += 1;
            s.peak = s.peak.max(s.active);
            let alone_at_start = s.active == 1;
            let started = s.started;
            s.started += 1;
            drop(s);
            let ms: u64 = query.get("ms").and_then(|m| m.parse().ok()).unwrap_or(200);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            let mut s = state.lock().unwrap();
            let overlapped = !alone_at_start || s.started != started + 1;
            s.active -= 1;
            (200, Some(json!({"overlapped": overlapped, "peak": s.peak})))
        }
        ("GET", ["slow"]) => {
            drop(s);
            let ms: u64 = query.get("ms").and_then(|m| m.parse().ok()).unwrap_or(100);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            (200, Some(json!({"slept": ms})))
        }
        _ => (
            404,
            Some(json!({"error": format!("no route for {method} {path}")})),
        ),
    }
}

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }
}

/// A temporary directory with `.kl` files.
pub struct Project {
    pub dir: PathBuf,
}

impl Project {
    pub fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("klausur-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Project { dir }
    }

    pub fn file(&self, name: &str, content: &str) -> &Self {
        let path = self.dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        self
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn run(&self, server: &Server, args: &[&str]) -> Output {
        run_in(&self.dir, server, args)
    }
}

pub fn run_in(dir: &Path, server: &Server, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_klausur"))
        .args(args)
        .current_dir(dir)
        .env("BASE", &server.base)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}
