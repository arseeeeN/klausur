mod support;

use std::path::{Path, PathBuf};
use support::{Project, start};

const USERS: &str = r#"
let base  = env("BASE")

@createUser(name = "alice", role = "user") {
    connection {
        POST {{base}}/users
        X-Trace: {{ "t-" + name }}
    }

    send { "name": {{name}}, "role": {{role}} }

    expect {
        status == 201
        body.id is int
    }
    let id = body.id

    undo {
        DELETE {{base}}/users/{{id}}
    }
}

@getUser(id) {
    connection { GET {{base}}/users/{{id}} }
    expect status == 200
}

@login(user, pass) {
    connection { POST {{base}}/login }
    send { "user": {{user}}, "pass": {{pass}} }
}

fn onboard(name) {
    let u = createUser(name: name);
    getUser(u.id);
    u
}
"#;

#[test]
fn run_a_single_request() {
    let server = start();
    let p = Project::new("run-single");
    p.file("users.kl", USERS);
    let out = p.run(
        &server,
        &["run", "users.kl#createUser", "--arg", "name=bob"],
    );
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("\"name\": \"bob\""), "{}", out.all());
    assert!(out.stderr.contains("201 Created"), "{}", out.all());
    assert!(out.stderr.contains("let id = 1"), "{}", out.all());
    assert!(
        out.stderr
            .contains("wasn't registered because no scope was open"),
        "{}",
        out.all()
    );
}

#[test]
fn run_a_function_with_named_arguments() {
    let server = start();
    let p = Project::new("run-fn");
    p.file("users.kl", USERS);
    let out = p.run(&server, &["run", "users.kl#onboard", "-a", "name=carol"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("\"carol\""), "{}", out.all());
    assert!(
        out.stderr.contains("GET"),
        "trace shows both requests: {}",
        out.all()
    );
}

#[test]
fn passing_and_failing_tests() {
    let server = start();
    let p = Project::new("tests-basic");
    p.file("users.kl", USERS);
    p.file(
        "users.test.kl",
        r#"
import "users.kl"

test "create and fetch" rollback {
    let u = createUser(name: "dave")
    expect getUser(u.id).body.name == "dave"
}

test "login" (user, pass, ok) cases [
    #{ user: "alice", pass: "secret", ok: true },
    #{ user: "alice", pass: "wrong",  ok: false },
] {
    expect login(user: user, pass: pass).status == if ok { 200 } else { 401 }
}

test "roles" (role in ["user", "admin"]) rollback {
    expect createUser(role: role).body.role == role
}

test "this one fails" {
    expect login("alice", "nope").status == 200
}
"#,
    );
    let out = p.run(&server, &["test", "users.test.kl"]);
    assert_eq!(out.code, 1, "{}", out.all());
    let s = &out.stdout;
    assert!(s.contains("✓ create and fetch"), "{s}");
    assert!(
        s.contains(r#"✓ login [user="alice", pass="wrong", ok=false]"#),
        "{s}"
    );
    assert!(s.contains(r#"✓ roles [role="admin"]"#), "{s}");
    assert!(s.contains("✗ this one fails"), "{s}");
    assert!(s.contains("left: 401"), "{s}");
    assert!(s.contains("4 passed") || s.contains("5 passed"), "{s}");
    // rollback cleaned up every created user
    assert!(server.state.lock().unwrap().users.is_empty(), "{s}");
}

/// `cargo test --test e2e serve -- --ignored --nocapture` runs the fake API for manual testing.
#[test]
#[ignore]
fn serve() {
    let server = start();
    println!("BASE={}", server.base);
    std::thread::sleep(std::time::Duration::from_secs(3600));
}

const API: &str = r#"
let base = env("BASE")

@createUser(name = "alice", role = "user") {
    connection { POST {{base}}/users }
    send { "name": {{name}}, "role": {{role}} }
    expect status == 201
    let id = body.id
    undo { DELETE {{base}}/users/{{id}} }
}

@getUser(id) {
    connection { GET {{base}}/users/{{id}} }
}

@deleteUser(id) {
    connection { DELETE {{base}}/users/{{id}} }
    expect status == 204 || status == 404
}

@createWithCallUndo(name) {
    connection { POST {{base}}/users }
    send { "name": {{name}} }
    let id = body.id
    undo deleteUser(id)
}

@createWithStrictUndo(name) {
    connection { POST {{base}}/users }
    send { "name": {{name}} }
    let id = body.id
    undo {
        connection { DELETE {{base}}/users/{{id}} }
        expect status == 204
    }
}

@login(user, pass = "secret") {
    connection { POST {{base}}/login }
    send { "user": {{user}}, "pass": {{pass}} }
}

@me() {
    connection { GET {{base}}/me }
    expect status == 200
}

@echo(path = "x", value = "") {
    connection {
        POST {{base}}/echo/{{path}}?q={{value}}
        X-Value: {{value}}
    }
    send { "value": {{value}}, "text": "id-{{value}}", "raw": {{ raw("[1,2]") }} }
}

@flaky(key) {
    connection { GET {{base}}/flaky/{{key}} }
    expect status == 200
}

@slow(ms) {
    connection { GET {{base}}/slow?ms={{ms}} }
}

fn count() { getUsers().body.len() }

@getUsers() {
    connection { GET {{base}}/users }
}
"#;

fn project(name: &str, tests: &str) -> Project {
    let p = Project::new(name);
    p.file("api.kl", API);
    p.file("t.test.kl", &format!("import \"api.kl\"\n{tests}"));
    p
}

#[test]
fn hooks_and_custom_modifiers() {
    let server = start();
    let p = project(
        "hooks",
        r#"
fn as_user(name) {
    this.before(|ctx| {
        let s = login(user: name)
        ctx.headers["Authorization"] = `Bearer ${s.body.token}`
        ctx.user = name
    })
}

fn tag_requests() {
    this.on_request(|req| {
        req.headers["X-Extra"] = "yes"
        req
    })
}

fn skip_body() {
    this.around(|ctx, run| { ctx.log("not running") })
}

fn record() {
    this.after(|ctx, result| { ctx.log(`passed=${result.passed}`) })
}

test "authenticated" as_user("bob") {
    expect me().body.user == "bob"
    expect ctx().user == "bob"
}

test "middleware" tag_requests() {
    expect echo().body.headers["x-extra"] == "yes"
}

test "around may skip" skip_body() {
    fail("must not run")
}

test "after sees result" record() {
    expect true
}
"#,
    );
    let out = p.run(&server, &["test", "-v"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("passed=true"), "{}", out.all());
    assert!(out.stdout.contains("not running"), "{}", out.all());
}

#[test]
fn retries() {
    let server = start();
    let p = project(
        "retries",
        r#"
fn again(times) {
    this.around(|ctx, run| {
        for i in 1..=times {
            let r = run.call()
            if r.passed || i == times { return r }
        }
    })
}

test "built-in" retries(2) { flaky("a") }
test "custom" again(3) { flaky("b") }
test "not enough" retries(1) { flaky("c") }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stdout.contains("✓ built-in"), "{}", out.all());
    assert!(out.stdout.contains("✓ custom"), "{}", out.all());
    assert!(out.stdout.contains("✗ not enough"), "{}", out.all());
}

#[test]
fn transactions_and_scopes() {
    let server = start();
    let p = project(
        "transactions",
        r#"
test "failed transaction unwinds" {
    let r = "no error"
    try {
        transaction(|| {
            createUser(name: "a")
            createUser(name: "b")
            fail("boom")
        })
    } catch (e) {
        r = `${e}`
    }
    expect r == "boom"
    expect count() == 0
}

test "committed transaction without scope keeps state" {
    let u = transaction(|| createUser(name: "kept"))
    expect getUser(u.id).status == 200
}

test "committed transaction inside rollback is cleaned up" rollback {
    transaction(|| createUser(name: "temp"))
    expect count() == 2
}

test "nested transaction undone by outer failure" {
    try {
        transaction(|| {
            transaction(|| createUser(name: "inner"))
            fail("outer fails")
        })
    } catch {}
    expect count() == 1
}

test "scoped always unwinds" {
    let id = scoped(|| createUser(name: "s").id)
    expect getUser(id).status == 404
}

test "call and strict undo forms" rollback {
    createWithCallUndo("c1")
    createWithStrictUndo("c2")
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
    let users = &server.state.lock().unwrap().users;
    assert_eq!(users.len(), 1, "only the committed user remains: {users:?}");
    assert_eq!(users.values().next().unwrap()["name"], "kept");
}

#[test]
fn dirty_runs_stop_and_exit_3() {
    let server = start();
    let p = project(
        "dirty",
        r#"
test "first" rollback {
    createUser(name: "undeletable-a")
    createUser(name: "undeletable-b")
}
test "second" { expect true }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 3, "{}", out.all());
    assert!(out.stdout.contains("DIRTY"), "{}", out.all());
    assert!(out.stdout.contains("database is on fire"), "{}", out.all());
    assert!(out.stdout.contains("may be a consequence"), "{}", out.all());
    assert!(
        !out.stdout.contains("second"),
        "the run stops after a dirty test: {}",
        out.all()
    );
    // each failing undo was retried 3 times (500 is transient)
    assert_eq!(server.state.lock().unwrap().deletes.len(), 6);

    let out = p.run(&server, &["test", "--on-dirty", "continue"]);
    assert_eq!(out.code, 3, "{}", out.all());
    assert!(out.stdout.contains("✓ second"), "{}", out.all());
}

#[test]
fn generators_are_reproducible() {
    let server = start();
    let p = project(
        "generators",
        r#"
fn even() { gen::custom(|rng| rng.int(0, 50) * 2) }

test "gens" (n in gen::int(1..=100), e in even(), s in gen::string(3..=5), u in gen::map(#{ id: gen::uuid(), k: "fixed" })) runs(5) {
    expect n >= 1 && n <= 100
    expect e % 2 == 0
    expect s.len() >= 3 && s.len() <= 5
    expect u.k == "fixed" && u.id.len() == 36
}

test "fails sometimes" (n in gen::int(1..=10)) runs(20) {
    expect n < 9
}
"#,
    );
    let a = p.run(&server, &["test", "--seed", "42"]);
    let b = p.run(&server, &["test", "--seed", "42"]);
    assert_eq!(a.code, 1, "{}", a.all());
    let strip = |s: &str| {
        s.lines()
            .filter(|l| !l.contains(" ms") && !l.contains(" s)"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(strip(&a.stdout), strip(&b.stdout));
    assert!(a.stdout.contains("klausur test --seed 42"), "{}", a.all());
    assert_eq!(a.stdout.matches("✓ gens").count(), 5, "{}", a.all());
}

#[test]
fn case_sources_and_filters() {
    let server = start();
    let p = project(
        "cases",
        r#"
test "csv" (user, pass, status) cases csv("logins.csv") {
    expect login(user, pass).status == status
}

test "json" (user, ok) cases json("users.json") {
    expect (login(user).status == 200) == ok
}

test "range" (n in 1..4) { expect n < 4 }

test "tagged" tags("slow") { expect true }

test "skipped" skip("later") { fail("no") }
"#,
    );
    p.file(
        "logins.csv",
        "user,pass,status\nalice,secret,200\nalice,nope,401\n",
    );
    p.file(
        "users.json",
        r#"[{"user": "a", "ok": true}, {"user": "b", "ok": true}]"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(
        out.stdout
            .contains(r#"✓ csv [user="alice", pass="nope", status=401]"#),
        "{}",
        out.all()
    );
    assert!(out.stdout.contains("✓ range [n=3]"), "{}", out.all());
    assert!(out.stdout.contains("○ skipped — later"), "{}", out.all());

    let out = p.run(&server, &["test", "csv", "--case", "pass=nope"]);
    assert!(out.stdout.contains("1 passed"), "{}", out.all());
    let out = p.run(&server, &["test", "--tag", "slow"]);
    assert!(
        out.stdout.contains("✓ tagged") && out.stdout.contains("1 passed"),
        "{}",
        out.all()
    );
}

#[test]
fn case_labels() {
    let server = start();
    let p = project(
        "labels",
        r#"
test "rules" (name, acl, ok) cases [
    #{ name: "allow all", acl: #{ website: [#{ path: "/", allow: true }] }, ok: true },
    #{ name: "deny all", acl: #{ website: [#{ path: "/", allow: false }] }, ok: false },
] label(name) {
    expect name.len() > 0
    expect acl.website[0].allow == ok
    expect name != "deny all"
}

test "matrix" (endpoint in ["stats", "health"], role in ["admin"]) label(`${role} on ${endpoint}`) {
    expect role == "admin"
}

test "unlabelled" (acl, n) cases [#{ acl: #{ a: [1, 2] }, n: 1 }, #{ acl: #{}, n: 2 }] {
    expect n > 0
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ rules [allow all]",
        "✗ rules [deny all]",
        r#"args: name="deny all", acl={"website":[{"allow":false,"path":"/"}]}, ok=false"#,
        "✓ matrix [admin on stats]",
        "✓ matrix [admin on health]",
        "✓ unlabelled [acl={…}, n=1]",
        "✓ unlabelled [acl={}, n=2]",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }

    let out = p.run(&server, &["test", "--case", "admin on health"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("1 passed"), "{}", out.all());
    let out = p.run(&server, &["test", "rules", "--case", "ok=true"]);
    assert!(
        out.stdout.contains("✓ rules [allow all]") && out.stdout.contains("1 passed"),
        "{}",
        out.all()
    );

    p.file(
        "t.test.kl",
        r#"
test "twice" (n in [1]) label(n) label(n) { }
test "empty" (n in [1]) label() { }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(
        out.stderr.contains("`label` is given twice"),
        "{}",
        out.all()
    );
    assert!(
        out.stderr.contains("`label` needs an expression"),
        "{}",
        out.all()
    );

    p.file("t.test.kl", "test \"bad\" (n in [1]) label(missing) { }\n");
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stdout.contains("missing"), "{}", out.all());
}

#[test]
fn templated_methods() {
    let server = start();
    let p = project(
        "methods",
        r#"
@echoAs(method = "POST") {
    connection { {{method}} {{base}}/echo/m }
    expect status == 200
}
@strict(method) {
    connection { {{method}} {{base}}/echo/m }
    expect body.method == "PUT"
}

test "default" { expect echoAs().body.method == "POST" }
test "override" { expect echoAs(method: "PUT").body.method == "PUT" }
test "expression" { let update = true; expect echoAs(if update { "PATCH" } else { "POST" }).body.method == "PATCH" }
test "custom" { expect echoAs("PURGE").body.method == "PURGE" }
test "null" { echoAs(()) }
test "invalid" { echoAs("P T") }
test "lowercase" { strict("put") }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ default",
        "✓ override",
        "✓ expression",
        "✓ custom",
        "✗ null",
        "`{{method}}` must be an HTTP method like \"PUT\", got null",
        "got \"P T\"",
        "✗ lowercase",
        "the method was sent as `put`; methods are case-sensitive, did you mean `PUT`?",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }

    let out = p.run(&server, &["list"]);
    assert!(
        out.stdout.contains("{{method}} {{base}}/echo/m"),
        "{}",
        out.all()
    );

    p.file(
        "bad.kl",
        "@a(m) { connection { {{m}}X http://x } }\n@b(m) { connection { P{{m}} http://x } }\n",
    );
    let out = p.run(&server, &["test", "bad.kl"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert_eq!(
        out.stderr
            .matches("a templated method must be the whole word")
            .count(),
        2,
        "{}",
        out.all()
    );
}

#[test]
fn only_and_timeout() {
    let server = start();
    let p = project(
        "only",
        r#"
test "focused" only timeout("100ms") { slow(500) }
test "other" { fail("not run") }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(
        out.stdout.contains("test timed out after 100 ms"),
        "{}",
        out.all()
    );
    assert!(!out.stdout.contains("other"), "{}", out.all());
}

#[test]
fn rendering_and_encoding() {
    let server = start();
    let p = project("render", "");
    let out = p.run(
        &server,
        &[
            "run",
            "api.kl#echo",
            "-a",
            "path=a b/c",
            "-a",
            r#"value=say "hi""#,
            "-q",
        ],
    );
    assert_eq!(out.code, 0, "{}", out.all());
    let body: serde_json::Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(body["path"], "/echo/a%20b%2Fc");
    assert_eq!(body["query"]["q"], "say%20%22hi%22");
    assert_eq!(body["headers"]["x-value"], r#"say "hi""#);
    let sent: serde_json::Value = serde_json::from_str(body["body"].as_str().unwrap()).unwrap();
    assert_eq!(sent["value"], r#"say "hi""#);
    assert_eq!(sent["text"], r#"id-say "hi""#);
    assert_eq!(sent["raw"], serde_json::json!([1, 2]));
}

#[test]
fn bodies_raw_and_expr() {
    let server = start();
    let p = Project::new("bodies");
    p.file(
        "b.kl",
        r#"
let base = env("BASE")
@raw(name) {
    connection {
        POST {{base}}/echo
        Content-Type: text/plain
    }
    send ```
        hello {{name}}
          indented
        ```
}
@computed(items) {
    connection { POST {{base}}/echo }
    send = #{ count: items.len(), items: items }
}
test "raw" { expect raw("x").body.body == "hello x\n  indented" }
test "computed" {
    let r = computed([1, 2])
    expect r.body.headers["content-type"] == "application/json"
    expect parse_json(r.body.body).count == 2
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

#[test]
fn imports_aliases_globals_and_defaults() {
    let server = start();
    let p = Project::new("imports");
    p.file(
        "lib/users.kl",
        r#"
let users_url = base + "/users"      // uses a global defined in another file, later
@make(name, role = default_role(name)) {
    connection { POST {{users_url}} }
    send { "name": {{name}}, "role": {{role}} }
}
fn default_role(name) { if name == "root" { "admin" } else { "user" } }
"#,
    );
    p.file(
        "main.test.kl",
        r#"
import "lib/users.kl" as users
let base = env("BASE")
let unused = env("SURELY_NOT_SET_ANYWHERE")

fn pick(a, b = a + 1, c = b * 2) { [a, b, c] }

test "aliases and defaults" {
    expect users::make("root").body.role == "admin"
    expect users::make(name: "joe").body.role == "user"
    expect make(role: "x", name: "y").body.role == "x"
    expect pick(1) == [1, 2, 4]
    expect pick(1, c: 0) == [1, 2, 0]
}
"#,
    );
    let out = p.run(&server, &["test", "main.test.kl"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

#[test]
fn env_files() {
    let server = start();
    let p = Project::new("envfiles");
    p.file(".env", "WHO=default\nOTHER=from-dotenv\n");
    p.file(".env.staging", "WHO=staging\n");
    p.file(
        "e.test.kl",
        r#"
test "default" { expect env("WHO") == "default" }
"#,
    );
    p.file(
        "s.kl",
        r#"test "staging" { expect env("WHO") == "staging" && env("OTHER") == "from-dotenv" }"#,
    );
    let out = p.run(&server, &["test", "e.test.kl"]);
    assert_eq!(out.code, 0, "{}", out.all());
    let out = p.run(&server, &["test", "s.kl", "--env", "staging"]);
    assert_eq!(out.code, 0, "{}", out.all());
    let out = p.run(&server, &["test", "s.kl", "--env", "nope"]);
    assert_eq!(out.code, 2, "{}", out.all());
}

/// Regression: request functions consumed their arguments, so a variable passed as the first
/// argument was `()` on its next use.
#[test]
fn arguments_are_not_consumed() {
    let server = start();
    let p = project(
        "args",
        r#"
test "reuse" {
    let id = createUser(name: "keep").id
    expect getUser(id).status == 200
    expect getUser(id).status == 200
    let name = "again"
    createUser(name, "user")
    expect name == "again"
    let m = #{ name: "kw" }
    expect createUser(name: m.name).body.name == "kw"
    expect m.name == "kw"
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

/// A request that times out may still complete on the server, so its undo is registered.
#[test]
fn timed_out_requests_register_their_undo() {
    let server = start();
    let p = project(
        "timeout-undo",
        r#"
@slowCreate(name) {
    connection { POST {{base}}/users?slow=2500 }
    send { "name": {{name}} }
    let id = body.id
    undo { DELETE {{base}}/users/{{id}} }
}

test "slow" rollback { slowCreate("late") }
"#,
    );
    let out = p.run(&server, &["test", "--timeout", "1"]);
    assert_eq!(out.code, 3, "{}", out.all());
    assert!(
        out.stdout.contains("timed out (outcome unknown)"),
        "{}",
        out.all()
    );
    assert!(
        out.stdout.contains("its undo was registered anyway"),
        "{}",
        out.all()
    );
}

#[test]
fn per_request_timeouts() {
    let server = start();
    let p = project(
        "request-timeouts",
        r#"
@fastFail(ms) timeout("300ms") {
    connection { GET {{base}}/slow?ms={{ms}} }
}
@patient(ms) timeout(`${ms * 3}ms`) {
    connection { GET {{base}}/slow?ms={{ms}} }
}

test "fails fast" { fastFail(1000) }
test "may take longer than the default" { expect patient(1500).body.slept == 1500 }
"#,
    );
    let out = p.run(&server, &["test", "--timeout", "1"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(
        out.stdout.contains("failed: timed out after 300 ms"),
        "{}",
        out.all()
    );
    assert!(
        out.stdout.contains("✓ may take longer than the default"),
        "{}",
        out.all()
    );

    p.file(
        "bad.kl",
        "@x() retries(2) { connection { GET http://x } }\n",
    );
    let out = p.run(&server, &["test", "bad.kl"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(
        out.stderr.contains("`retries` can't be used on a request"),
        "{}",
        out.all()
    );
}

#[test]
fn polling() {
    let server = start();
    let p = project(
        "polling",
        r#"
@job(key, after = 3) poll(until: body.state == "done", every: "50ms") {
    connection { GET {{base}}/jobs/{{key}}?after={{after}} }
    expect status == 200
    let polls = body.polls
}
@unflake(key) poll(until: |r| r.status == 200, every: "50ms") {
    connection { GET {{base}}/flaky/{{key}} }
}
@never(key) poll(until: body.state == "done", every: "100ms", within: "350ms") {
    connection { GET {{base}}/jobs/{{key}}?after=1000 }
}
@warmup(key) timeout("200ms") poll(until: body.ready, every: "50ms") {
    connection { GET {{base}}/warmup/{{key}} }
}
@badUntil(key) poll(until: body.state) {
    connection { GET {{base}}/jobs/{{key}} }
}
@stays(key, after) poll(while: body.state == "pending", for: "300ms", every: "50ms") {
    connection { GET {{base}}/jobs/{{key}}?after={{after}} }
    let polls = body.polls
}

test "waits for the job" { expect job("a").polls == 3 }
test "closure condition" { expect unflake("b").body.attempts == 3 }
test "gives up" { never("c") }
test "timeouts count as not yet" { expect warmup("d").body.ready }
test "until must be a boolean" { badUntil("e") }
test "sleep honours the test timeout" timeout("200ms") { sleep(5000) }
test "while holds for the whole time" { expect stays("f", 1000).polls >= 3 }
test "while stops holding" { stays("g", 3) }
"#,
    );
    let started = std::time::Instant::now();
    let out = p.run(&server, &["test"]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "{}",
        out.all()
    );
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ waits for the job",
        "✓ closure condition",
        "✓ timeouts count as not yet",
        "✗ gives up",
        // 4 polls in theory; slow CI runners manage fewer.
        "`never` polled ",
        "but `until` never held",
        r#""state": "pending""#,
        "`until` must be true or false, got \"pending\"",
        "test timed out after 200 ms",
        "✓ while holds for the whole time",
        "✗ while stops holding",
        "`while` stopped holding for `stays` after",
        "(3 polls)",
        r#""state": "done""#,
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }

    p.file(
        "bad.kl",
        r#"
@a() poll(body.ok) { connection { GET http://x } }
@b() poll(every: "1s") { connection { GET http://x } }
@c() poll(until: true, evry: "1s") { connection { GET http://x } }
@d() poll(while: true) { connection { GET http://x } }
@e() poll(until: true, while: true) { connection { GET http://x } }
@f() poll(until: true, for: "1s") { connection { GET http://x } }
@g() poll(while: true, for: "1s", within: "2s") { connection { GET http://x } }
"#,
    );
    let out = p.run(&server, &["test", "bad.kl"]);
    assert_eq!(out.code, 2, "{}", out.all());
    for line in [
        "`poll` arguments need names",
        "`poll` needs an `until` or a `while` condition",
        "`poll(while: …)` needs `for`",
        "`poll` takes `until` or `while`, not both",
        "`for` goes with `while`; `until` has `within`",
        "`within` goes with `until`; `while` has `for`",
        "`poll` has no argument `evry`",
        "did you mean `every`?",
    ] {
        assert!(out.stderr.contains(line), "missing {line:?}\n{}", out.all());
    }
}

#[test]
fn files_and_script_undo() {
    let server = start();
    let p = project(
        "script-undo",
        r#"
fn temp_file(path, content) {
    let full = write_file(path, content)
    undo("remove the temp file", || remove_file(full))
    full
}

test "writes and cleans up" rollback {
    let p = temp_file("out/a.txt", "hello")
    expect file_exists(p)
    expect read_file(p) == "hello"
    expect p != "out/a.txt" && p.ends_with("a.txt")   // the absolute path
}
test "the file is gone" { expect !file_exists("out/a.txt") }
test "transactions keep it on success" {
    transaction(|| temp_file("out/b.txt", blob(3, 65)))
    expect read_file("out/b.txt") == "AAA"
    expect remove_file("out/b.txt")
    expect !remove_file("out/b.txt")
}
test "failing cleanup is dirty" rollback {
    let n = 41
    undo(|| { n += 1; fail(`could not clean up ${n}`) })
}
"#,
    );
    // A closure in a function captures a parameter even when another file defines a function
    // with that name (Rhai skips capturing names of functions parsed earlier in a compilation).
    p.file("a_first.kl", "fn path() { \"not a variable\" }\n");
    p.file(
        "capture.test.kl",
        r#"
fn later(path) { undo("capture", || { if path != "x" { fail(`got ${path}`) } }) }
test "closures capture parameters named like functions" rollback { later("x") }
test "functions still see each other" { expect path() == "not a variable" }
"#,
    );
    let out = p.run(&server, &["test", "--on-dirty=continue"]);
    assert_eq!(out.code, 3, "{}", out.all());
    for line in [
        "✓ closures capture parameters named like functions",
        "✓ functions still see each other",
        "✓ writes and cleans up",
        "✓ the file is gone",
        "✓ transactions keep it on success",
        "undo: script cleanup (t.test.kl:24:5)",
        "could not clean up 42",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
    assert!(!out.stdout.contains("undo: capture"), "{}", out.all());
}

#[test]
fn builtins_for_real_services() {
    let server = start();
    let p = project(
        "builtins",
        r##"
@echoAuth(jwt = ()) {
    connection {
        GET {{base}}/echo/auth
        Authorization: Bearer {{jwt}}
    }
}
@upload(fields) {
    connection { POST {{base}}/echo/upload }
    send = multipart(fields)
}
@sendFile(f) {
    connection { POST {{base}}/echo/file }
    send = f
}

test "null headers are left out" {
    expect !("authorization" in echoAuth().body.headers)
    expect echoAuth("abc").body.headers.authorization == "Bearer abc"
}

test "multipart" {
    let r = upload(#{ url: "x://1", file: file("data.md"), tags: ["a", "b"] })
    let ct = r.body.headers["content-type"]
    expect ct.starts_with("multipart/form-data; boundary=")
    let body = r.body.body
    expect body.contains(`name="file"; filename="data.md"`)
    expect body.contains("Content-Type: text/markdown")
    expect body.contains("# hello")
    expect regex_find_all(body, `name="tags"`).len() == 2
    expect body.index_of(`name="url"`) < body.index_of(`name="file"`)
}

test "send a file" {
    let r = sendFile(file_from("x.json", "{}"))
    expect r.body.headers["content-type"] == "application/json" && r.body.body == "{}"
}

test "base64, jwt and regex" {
    expect base64_encode("foobar") == "Zm9vYmFy"
    expect base64_decode("Zm9vYmFy") == "foobar"
    let jwt = base64url_encode(`{"alg":"HS256"}`) + "." + base64url_encode(`{"sub":"me","n":3}`) + ".sig"
    let d = jwt_decode(jwt)
    expect d.header.alg == "HS256" && d.claims.sub == "me" && d.claims.n == 3
    expect regex_match("code VAULT-7731-ALPHA here", `[A-Z]+-\d{4}-[A-Z]+`)
    expect regex_captures("v1.2", `v(?<major>\d+)\.(?<minor>\d+)`).minor == "2"
    expect regex_captures("v1.2", `v(\d+)\.(\d+)`)[1] == "1"
    expect regex_replace("a-b-c", "-", "+") == "a+b+c"
    expect sse_parse("event: x\ndata: {\"a\":1}\n\n")[0].data.a == 1
}
"##,
    );
    p.file("data.md", "# hello\n");
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

#[test]
fn load_errors_exit_2() {
    let server = start();
    let p = Project::new("errors");
    p.file("x.kl", "test \"t\" rolback { expect true }\n");
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(
        out.stderr.contains("did you mean `rollback`?"),
        "{}",
        out.all()
    );
}

#[test]
fn queries_and_value_helpers() {
    let server = start();
    let p = project(
        "queries",
        r##"
const PAGE = `<html><head><title>Otters</title><script>var x = 1</script></head><body>
  <nav><a href="/">Home</a></nav>
  <main><h1>Otters</h1><p class="lead">They <b>hold hands</b>.</p><p>Second</p></main>
</body></html>`;
const FEED = `<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <entry><title>One</title><link href="/1"/></entry>
  <entry><title>Two</title><link href="/2"/></entry>
</feed>`;

test "html" {
    expect html_select(PAGE, "main p").map(|p| p.text) == ["They hold hands.", "Second"]
    expect html_select(PAGE, "p.lead")[0].html.contains("<b>hold hands</b>")
    expect html_select(PAGE, "nav a")[0].attrs.href == "/"
    expect html_text(PAGE) == "Home Otters They hold hands. Second"
}
test "xml" {
    expect xml_select(FEED, "entry > title").map(|t| t.text) == ["One", "Two"]
    expect xml_select(FEED, "entry:last-child link")[0].attrs.href == "/2"
}
test "json path" {
    let v = #{a: #{url: "x"}, b: [#{url: "y"}, #{url: "z", n: 2}]}
    expect json_path(v, "$..url") == ["x", "y", "z"]
    expect json_path(v, "b[?@.n > 1].url") == ["z"]
}
test "copies instead of in place" {
    let s = "  Hi "
    expect s.trimmed() == "Hi"
    let a = [3, 1, 2]
    expect a.sorted() == [1, 2, 3]
    expect a.sorted(|x, y| y - x) == [3, 2, 1]
    expect a.reversed() == [2, 1, 3]
    expect a == [3, 1, 2]
}
test "print shows json" {
    print(#{a: [1, "b", ()]})
    print(`in text: ${[1, "c"]}`)
    expect false
}
test "bad selector" { html_select(PAGE, "p[") }
test "bad xml" { xml_select("<a>", "a") }
test "bad path" { json_path(#{}, "$[") }
"##,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ html",
        "✓ xml",
        "✓ json path",
        "✓ copies instead of in place",
        r#"{"a":[1,"b",null]}"#,
        r#"in text: [1,"c"]"#,
        "`p[` isn't a valid CSS selector",
        "not valid XML",
        "`$[` isn't a valid JSONPath",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
}

#[test]
fn expectations_explain_their_parts() {
    let server = start();
    let p = project(
        "explain",
        r##"
@job(key) {
    connection { GET {{base}}/jobs/{{key}}?after=100 }
}

test "parts are evaluated once" {
    // Each call counts a poll, so evaluating a part twice would show up here.
    expect job("once").body.polls == 1
    expect job("once").body.polls.to_string().contains("2")
    expect !job("once").body.state.contains("done") && job("once").body.polls == 4
}
test "not contains" {
    let urls = ["klausur://a", "klausur://b"]
    let ignored = "klausur://b"
    expect !urls.contains(ignored)
}
test "all and some" {
    let words = ["guest", "password", "OTTER"]
    let content = "Visit OTTER-GUEST"
    expect words.all(|w| content.contains(w))
}
test "some" { expect ![1, "SECRET-1", "ok"].some(|v| v is string && v.contains("SECRET")) }
test "property" {
    let hit = #{ ready: false, id: 7 }
    expect hit.ready
}
test "request in a chain" { expect job("chain").body.state == "done" }
"##,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ parts are evaluated once",
        "where:",
        r#"urls = ["klausur://a","klausur://b"]"#,
        r#"ignored = "klausur://b""#,
        r#"words.all(…) fails for = ["guest","password"]"#,
        r#"[1, "SECRET-1", "ok"].some(…) holds for = ["SECRET-1"]"#,
        r#"hit = {"id":7,"ready":false}"#,
        r#"left: "pending", right: "done""#,
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
}

#[test]
fn parallel_tests() {
    let server = start();
    let p = Project::new("parallel");
    p.file("users.kl", USERS);
    p.file(
        "p.test.kl",
        r#"
import "users.kl"

@hold(ms = 300) {
    connection { GET {{base}}/hold?ms={{ms}} }
    expect status == 200
}

test "a" parallel { hold() }
test "b" parallel rollback { createUser(name: "par"); hold() }
test "c" (n in 1..=3) parallel { hold() }
test "alone" { expect !hold().body.overlapped }
test "d" parallel { hold() }
test "e" parallel { expect hold().body.overlapped && hold(ms: 10).body.peak == 9 }
"#,
    );
    let started = std::time::Instant::now();
    let out = p.run(&server, &["test", "-j", "4", "-v"]);
    let took = started.elapsed();
    assert_eq!(out.code, 1, "{}", out.all());
    // 5 overlapping holds, one alone, then 2 more: ~1.2 s, not the 2.7 s of a serial run.
    assert!(
        took < std::time::Duration::from_millis(2200),
        "{took:?}\n{}",
        out.all()
    );
    let order: Vec<&str> = out
        .stdout
        .lines()
        .filter_map(|l| {
            l.trim_start()
                .strip_prefix("✓ ")
                .or(l.trim_start().strip_prefix("✗ "))
        })
        .map(|l| l.split(" (").next().unwrap())
        .collect();
    assert_eq!(
        order,
        ["a", "b", "c [n=1]", "c [n=2]", "c [n=3]", "alone", "d", "e"],
        "{}",
        out.all()
    );
    for line in [
        "✓ alone",
        "✗ e",
        // Failures from workers keep their source location and values.
        "p.test.kl:14",
        "left: 4, right: 9",
        // `-v` traces from workers go into the case's log.
        "→ GET",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
    assert!(
        server.state.lock().unwrap().users.is_empty(),
        "{}",
        out.all()
    );

    // Without `-j`, or with `-j 1`, nothing overlaps.
    let out = p.run(&server, &["test", "-j", "1", "alone"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

#[test]
fn browser_pages() {
    let server = start();
    let p = Project::new("browser");
    p.file(
        "page.html",
        r##"<!doctype html>
<html><head><title>Spike page</title></head>
<body>
  <form id="f"><input name="q"><button type="submit" id="go" disabled>Go</button></form>
  <ul id="results"></ul>
  <div id="overlay" style="position:fixed;inset:0;background:rgba(0,0,0,.3)"></div>
  <label><input type="checkbox" id="agree"> I agree</label>
  <select id="size"><option value="s">Small</option><option value="l">Large</option></select>
  <input type="file" id="doc">
  <p id="file-name"></p>
  <div id="spinner">Loading…</div>
  <search-box></search-box>
  <button id="jumpy" style="position:fixed;top:300px;left:0">Jumpy</button>
  <button id="far" style="margin-top:2000px">Far below</button>
  <script>
    customElements.define('search-box', class extends HTMLElement {
      connectedCallback() {
        const root = this.attachShadow({ mode: 'open' });
        root.innerHTML = '<input class="inner" placeholder="Search"><span class="echo"></span>';
        const input = root.querySelector('input');
        input.addEventListener('input', () => { root.querySelector('.echo').textContent = input.value; });
      }
    });
    const q = document.querySelector('input[name=q]');
    const go = document.querySelector('#go');
    q.addEventListener('input', () => { go.disabled = q.value.trim() === ''; });
    setTimeout(() => document.querySelector('#overlay').remove(), 300);
    setTimeout(() => document.querySelector('#spinner').style.display = 'none', 300);
    document.querySelector('#f').addEventListener('submit', e => {
      e.preventDefault();
      const term = q.value;
      setTimeout(() => {
        for (const t of [term + ' one', term + ' two']) {
          const li = document.createElement('li'); li.className = 'result'; li.textContent = t;
          document.querySelector('#results').appendChild(li);
        }
      }, 300);
    });
    // Jumps 40 px every 20 ms for 400 ms, so a click aimed where it was misses it.
    const jumpy = document.querySelector('#jumpy');
    let steps = 0;
    const timer = setInterval(() => {
      jumpy.style.left = (++steps * 40) + 'px';
      if (steps === 20) clearInterval(timer);
    }, 20);
    jumpy.addEventListener('click', () => { document.title = 'caught it'; });
    document.querySelector('#doc').addEventListener('change', e => {
      document.querySelector('#file-name').textContent = e.target.files[0].name;
    });
    document.querySelector('#far').addEventListener('click', () => {
      document.title = confirm('sure?') ? 'confirmed' : 'declined';
    });
    console.error('boom from the page');
  </script>
</body></html>"##,
    );
    p.file("notes.txt", "some notes");
    let path = p.path("page.html").display().to_string().replace('\\', "/");
    let url = format!("file:///{}", path.trim_start_matches('/'));
    p.file(
        "b.test.kl",
        &format!(
            r##"
let url = "{url}"

test "waits for the overlay and for results" {{
    let page = browser().open(url)
    page.fill("input[name=q]", "zebrafinch")
    page.click("#go")
    expect page.texts(".result") == ["zebrafinch one", "zebrafinch two"]
    expect page.value("input[name=q]") == "zebrafinch"
    expect page.locator(".result").first().text() == "zebrafinch one"
    expect page.locator("li").filter(has_text: "TWO").text() == "zebrafinch two"
    expect page.locator("#results").locator("li").nth(-1).text() == "zebrafinch two"
}}

test "enter submits, clicks scroll, dialogs are dismissed" {{
    let page = browser().open(url)
    page.fill("input[name=q]", "otter")
    page.press("input[name=q]", "Enter")
    expect page.count(".result") == 2
    page.click("#far")
    expect page.title == "declined"
    expect page.console().contains("confirm dismissed: sure?")
    page.accept_dialogs()
    page.click("#far")
    expect page.title == "confirmed"
}}

test "forms" {{
    let page = browser().open(url)
    page.check("#agree")
    page.check("#agree")
    expect page.checked("#agree")
    page.uncheck("#agree")
    expect !page.checked("#agree")
    expect page.select("#size", "Large") == ["l"]
    expect page.value("#size") == "l"
    page.upload("#doc", "notes.txt")
    expect page.text("#file-name") == "notes.txt"
    page.wait("#spinner", state: "hidden")
}}

test "clicks wait for moving elements" {{
    let page = browser().open(url)
    page.click("#jumpy")
    expect page.title == "caught it"
}}

test "shadow dom" {{
    let page = browser().open(url)
    page.fill("search-box .inner", "deep")
    expect page.text("search-box .echo") == "deep"
}}

test "cases don't share storage" {{
    let page = browser().open(url)
    expect page.evaluate("localStorage.getItem('k')") == null
    page.evaluate("localStorage.setItem('k', 'v')")
}}

test "cases don't share storage either" {{
    let page = browser().open(url)
    expect page.evaluate("localStorage.getItem('k')") == null
}}

test "a failed expectation shows the page" {{
    let page = browser(expect_timeout: "1s").open(url)
    expect page.title == "Another title"
}}

test "ambiguous selectors fail" {{
    let page = browser().open(url)
    page.fill("input[name=q]", "x")
    page.press("input[name=q]", "Enter")
    page.locator(".result").first().wait()
    page.text(".result")
}}

test "disabled buttons can't be clicked" {{
    let page = browser(timeout: "1s").open(url)
    page.click("#go")
}}

test "a checkbox can't be filled" {{
    browser().open(url).fill("#agree", "x")
}}

test "waits end with the test" timeout("3s") {{
    browser().open(url).click("#nope")
}}
"##
        ),
    );
    let out = p.run(&server, &["test", "b.test.kl"]);
    if out.all().contains("couldn't find Chrome") {
        eprintln!("skipping browser_pages: no Chrome on this machine");
        return;
    }
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ waits for the overlay and for results",
        "✓ enter submits, clicks scroll, dialogs are dismissed",
        "✓ forms",
        "✓ shadow dom",
        "✓ clicks wait for moving elements",
        "✓ cases don't share storage",
        "✓ cases don't share storage either",
        "✗ a failed expectation shows the page",
        r#"expectation failed: "Spike page" == "Another title" (retried for"#,
        // The path separator is the platform's.
        "b-a-failed-expectation-shows-the-page.png",
        "console.error: boom from the page",
        "`.result` matches 2 elements",
        r#"<button#go> "Go" is disabled"#,
        r#"an input of type "checkbox" can't be filled"#,
        "test timed out after 3.00 s while `click` on `#nope` waited",
        "7 passed, 5 failed",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
    assert!(
        p.path("klausur-results/b-a-failed-expectation-shows-the-page.png")
            .exists()
    );
    // Page handles stay out of `where:`; the page section describes them.
    assert!(!out.stdout.contains("\n    page = "), "{}", out.all());
}

#[test]
fn browser_routes() {
    let server = start();
    let p = Project::new("routes");
    p.file(
        "site/index.html",
        r##"<!doctype html>
<html><head><title>Mocked site</title></head>
<body>
  <button id="load">Load</button>
  <p id="out"></p>
  <script>
    document.querySelector('#load').addEventListener('click', async () => {
      const out = document.querySelector('#out');
      try {
        const r = await fetch('/api/data?n=1', { method: 'POST', body: '{"ask":1}' });
        out.textContent = r.ok ? (await r.json()).message : 'status ' + r.status;
      } catch (e) {
        out.textContent = 'network error';
      }
    });
  </script>
</body></html>"##,
    );
    p.file(
        "r.test.kl",
        r##"
fn site() {
    let b = browser()
    b.route("http://klausur.test/**", #{ path: "site/index.html" })
    b
}

test "a mocked site and api" {
    let b = site()
    b.route("**/api/data*", #{ json: #{ message: "from the mock" } })
    let page = b.open("http://klausur.test/")
    expect page.title == "Mocked site"
    page.click("#load")
    expect page.text("#out") == "from the mock"
    let sent = page.requests("**/api/data*")
    expect sent.len() == 1
    expect sent[0].method == "POST" && sent[0].body == `{"ask":1}` && sent[0].status == 200
    expect sent[0].headers["content-type"].starts_with("text/plain")
}

test "the newest route wins, and unroute removes it" {
    let page = site().open("http://klausur.test/")
    page.route("**/api/data*", #{ json: #{ message: "first" } })
    page.route("**/api/data*", #{ status: 503 })
    page.click("#load")
    expect page.text("#out") == "status 503"
    page.unroute("**/api/data*")
    page.route("**/api/data*", "abort")
    page.click("#load")
    expect page.text("#out") == "network error"
}

test "failed requests show in the report" {
    let page = site().open("http://klausur.test/")
    page.route("**/api/data*", #{ status: 500 })
    page.click("#load")
    expect page.text("#out") == "it worked"
}

test "locators take no selector" {
    let page = site().open("http://klausur.test/")
    page.locator("body").click("#load")
}

test "fragment navigation and reload" timeout("10s") {
    let page = site().open("http://klausur.test/")
    page.evaluate("window.marker = 1")
    page.navigate("http://klausur.test/#section")
    expect page.url.ends_with("#section")
    expect page.evaluate("window.marker") == 1
    page.reload()
    expect page.evaluate("window.marker ?? null") == null
    expect page.url.ends_with("#section")
}

test "expected failures stay out of the report" {
    browser(ignore_requests: ["**/api/data*"])
    let page = site().open("http://klausur.test/")
    page.route("**/api/data*", #{ status: 500 })
    page.click("#load")
    expect page.text("#out") == "it worked"
}
"##,
    );
    let out = p.run(&server, &["test", "r.test.kl"]);
    if out.all().contains("couldn't find Chrome") {
        eprintln!("skipping browser_routes: no Chrome on this machine");
        return;
    }
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ a mocked site and api",
        "✓ the newest route wins, and unroute removes it",
        "✗ failed requests show in the report",
        "500 POST http://klausur.test/api/data?n=1",
        "a locator's `click` takes no selector",
        "✓ fragment navigation and reload",
        "✗ expected failures stay out of the report",
        "3 passed, 3 failed",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
    let ignored = out
        .stdout
        .split("── ")
        .find(|s| s.starts_with("expected failures stay out of the report"))
        .unwrap_or_default();
    assert!(ignored.contains("screenshot:"), "{ignored}");
    assert!(!ignored.contains("api/data"), "{ignored}");
}

#[test]
fn helpers_from_the_magnolia_suite() {
    let server = start();
    let p = project(
        "magnolia-helpers",
        r##"
@echoPath(workspace, path) {
    connection { GET {{base}}/echo/{{workspace}}{{url_path(path)}} }
}

test "url_path keeps slashes and encodes segments" {
    expect echoPath("website", "/fast forward/demo/main").body.path == "/echo/website/fast%20forward/demo/main"
}

test "write_file with undo restores or removes" {
    write_file("fixtures/existing.txt", "original")
    scoped(|| {
        write_file("fixtures/existing.txt", "changed", undo: true)
        write_file("fixtures/new.txt", "new", undo: true)
        expect read_file("fixtures/existing.txt") == "changed"
    })
    expect read_file("fixtures/existing.txt") == "original"
    expect !file_exists("fixtures/new.txt")
}

test "dates" {
    expect regex_match(today(), "^\\d{4}-\\d{2}-\\d{2}$")
    expect now().starts_with(today())
    expect now("%Y") == today().sub_string(0, 4)
}

test "a bad date format fails" { now("%Q") }

test "watch_file returns what was appended" {
    write_file("server.log", "old line\n")
    let log = watch_file("server.log")
    expect log.lines() == []
    write_file("server.log", "old line\nERROR new one\n\nINFO fine\n")
    expect log.lines() == ["ERROR new one", "INFO fine"]
    expect log.lines().filter(|l| l.contains("ERROR")).len() == 1
}

test "json_ld and inner html" {
    let html = `<head><script type="application/ld+json">{"@type": "Organization", "name": "A  B"}</script></head><div id="d"><b>x</b></div>`
    expect json_ld(html).map(|x| x["@type"]) == ["Organization"]
    expect json_ld(html)[0].name == "A  B"
    let d = html_select(html, "#d")[0]
    expect d.inner_html == "<b>x</b>" && d.outer_html == d.html
}
"##,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    for line in [
        "✓ url_path keeps slashes and encodes segments",
        "✓ write_file with undo restores or removes",
        "✓ dates",
        "✗ a bad date format fails",
        "`%Q` isn't a valid date format",
        "✓ watch_file returns what was appended",
        "✓ json_ld and inner html",
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
}

/// A fake GitHub release page for `latest` = 99.0.0, with the asset this platform downloads.
fn release_server(
    asset_bytes: &'static [u8],
    sums: impl Fn(&str) -> String + Send + 'static,
) -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}/releases", server.server_addr().to_ip().unwrap());
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "klausur-x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "klausur-aarch64-unknown-linux-musl",
        ("macos", "aarch64") => "klausur-aarch64-apple-darwin",
        ("macos", "x86_64") => "klausur-x86_64-apple-darwin",
        _ => "klausur-x86_64-pc-windows-msvc.exe",
    };
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let url = req.url().to_string();
            let response = if url == "/releases/latest" {
                tiny_http::Response::from_data(Vec::new())
                    .with_status_code(302)
                    .with_header(
                        tiny_http::Header::from_bytes("Location", "/releases/tag/v99.0.0").unwrap(),
                    )
            } else if url == format!("/releases/download/v99.0.0/{asset}") {
                tiny_http::Response::from_data(asset_bytes.to_vec())
            } else if url == "/releases/download/v99.0.0/SHA256SUMS" {
                tiny_http::Response::from_data(sums(asset).into_bytes())
            } else {
                tiny_http::Response::from_data(Vec::new()).with_status_code(404)
            };
            let _ = req.respond(response);
        }
    });
    base
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[test]
fn update_hint_and_update() {
    const NEW: &[u8] = b"pretend this is klausur 99";
    let releases = release_server(NEW, |asset| format!("{}  {asset}\n", sha256_hex(NEW)));
    let p = Project::new("update");
    p.file("t.test.kl", "test \"t\" { expect true }\n");
    let cache = p.path("cache");
    let run = |exe: &Path, args: &[&str]| {
        std::process::Command::new(exe)
            .args(args)
            .current_dir(&p.dir)
            .env("KLAUSUR_RELEASES_URL", &releases)
            .env("KLAUSUR_CACHE_DIR", &cache)
            .env("NO_COLOR", "1")
            .env_remove("CI")
            .env_remove("KLAUSUR_NO_UPDATE_CHECK")
            .output()
            .unwrap()
    };
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_klausur"));

    // The hint, then the same from the cache (the check runs at most once a day).
    for _ in 0..2 {
        let out = run(&exe, &["test", "t.test.kl"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let hint = format!(
            "klausur 99.0.0 is out (you have {}): run `klausur update`",
            env!("CARGO_PKG_VERSION")
        );
        assert!(stderr.contains(&hint), "{stderr}");
    }
    assert!(
        std::fs::read_to_string(cache.join("latest-version"))
            .unwrap()
            .ends_with(" 99.0.0\n")
    );

    // `update` replaces a copy of the binary, never the one the tests run.
    let copy = p.path(if cfg!(windows) {
        "bin/klausur.exe"
    } else {
        "bin/klausur"
    });
    std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
    std::fs::copy(&exe, &copy).unwrap();
    let out = run(&copy, &["update"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("→ 99.0.0"), "{stdout}");
    assert_eq!(std::fs::read(&copy).unwrap(), NEW);
}

#[test]
fn update_refuses_a_bad_checksum() {
    let releases = release_server(b"tampered", |asset| {
        format!("{}  {asset}\n", "0".repeat(64))
    });
    let p = Project::new("update-bad");
    let copy = p.path(if cfg!(windows) {
        "klausur.exe"
    } else {
        "klausur"
    });
    std::fs::copy(env!("CARGO_BIN_EXE_klausur"), &copy).unwrap();
    let before = std::fs::read(&copy).unwrap();
    let out = std::process::Command::new(&copy)
        .arg("update")
        .env("KLAUSUR_RELEASES_URL", &releases)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("doesn't match its checksum; nothing was changed"),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&copy).unwrap(), before);
}

#[test]
fn parallel_groups() {
    // Each check gets its own server: `/hold` reports the most requests it ever held at once.
    for (modifier, max_peak, overlaps) in [
        (r#"parallel("slow", 1)"#, 1, false),
        (r#"parallel("slow", 2)"#, 2, true),
    ] {
        let server = start();
        let p = project(
            "groups",
            &format!(
                r#"
@hold() {{ connection {{ GET {{{{base}}}}/hold?ms=300 }} }}
test "grouped" (n in [1, 2, 3, 4]) {modifier} {{
    let r = hold()
    expect r.body.peak <= {max_peak}
    if !{overlaps} {{ expect !r.body.overlapped }}
}}
"#
            ),
        );
        let out = p.run(&server, &["test", "-j", "4"]);
        assert_eq!(out.code, 0, "{modifier}\n{}", out.all());
        assert!(out.stdout.contains("4 passed"), "{}", out.all());
    }
}

#[test]
fn fail_fast_and_junit() {
    let server = start();
    let p = project(
        "junit",
        r#"
test "first passes" { expect true }
test "second fails <&>" { expect 1 + 1 == 3 }
test "third never runs" { expect true }
test "skipped one" skip("not today") { expect true }
"#,
    );
    let out = p.run(&server, &["test", "--fail-fast"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stdout.contains("1 passed, 1 failed"), "{}", out.all());
    assert!(
        out.stdout
            .contains("stopped after the first failure (--fail-fast)"),
        "{}",
        out.all()
    );
    assert!(!out.stdout.contains("✓ third never runs"), "{}", out.all());

    let out = p.run(&server, &["test", "--junit", "report.xml"]);
    assert_eq!(out.code, 1, "{}", out.all());
    let xml = std::fs::read_to_string(p.path("report.xml")).unwrap();
    let doc = roxmltree::Document::parse(&xml).expect("well-formed XML");
    let root = doc.root_element();
    assert_eq!(root.tag_name().name(), "testsuites");
    assert_eq!(root.attribute("tests"), Some("4"));
    assert_eq!(root.attribute("failures"), Some("1"));
    assert_eq!(root.attribute("skipped"), Some("1"));
    let cases: Vec<_> = doc
        .descendants()
        .filter(|n| n.has_tag_name("testcase"))
        .collect();
    let names: Vec<_> = cases.iter().filter_map(|c| c.attribute("name")).collect();
    assert_eq!(
        names,
        [
            "first passes",
            "second fails <&>",
            "third never runs",
            "skipped one"
        ]
    );
    let failure = cases[1]
        .children()
        .find(|n| n.has_tag_name("failure"))
        .unwrap();
    assert_eq!(
        failure.attribute("message"),
        Some("expectation failed: 2 == 3")
    );
    assert!(
        failure.text().unwrap_or_default().contains("t.test.kl:4"),
        "{xml}"
    );
    assert!(!xml.contains('\x1b'), "{xml}");
    let skipped = cases[3]
        .children()
        .find(|n| n.has_tag_name("skipped"))
        .unwrap();
    assert_eq!(skipped.attribute("message"), Some("not today"));
}

#[test]
fn watch_reruns_on_change() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use std::time::Duration;

    let p = Project::new("watch");
    p.file("w.test.kl", "test \"one\" { expect true }\n");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_klausur"))
        .args(["test", "--watch", "."])
        .current_dir(&p.dir)
        .env("NO_COLOR", "1")
        .env("KLAUSUR_NO_UPDATE_CHECK", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut seen = String::new();
    let mut wait_for = |text: &str| {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !seen.contains(text) {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    seen.push_str(&line);
                    seen.push('\n');
                }
                Err(_) => return Err(seen.clone()),
            }
        }
        Ok(())
    };
    let result = wait_for("watching for changes").and_then(|()| {
        // Give the watcher a moment to settle before the change.
        std::thread::sleep(Duration::from_millis(500));
        p.file("w.test.kl", "test \"two\" { expect false }\n");
        wait_for("✗ two")
    });
    let _ = child.kill();
    let _ = child.wait();
    if let Err(seen) = result {
        panic!("watch output:\n{seen}");
    }
}

#[test]
fn init_scaffolds_a_suite_that_runs() {
    let server = start();
    let p = Project::new("init");
    let klausur = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_klausur"))
            .args(args)
            .current_dir(&p.dir)
            .env("BASE_URL", &server.base)
            .env("NO_COLOR", "1")
            .env("KLAUSUR_NO_UPDATE_CHECK", "1")
            .output()
            .unwrap();
        (
            out.status.code(),
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
        )
    };

    // The default folder; the example test passes against a running service.
    let (code, out) = klausur(&["init"]);
    assert_eq!(code, Some(0), "{out}");
    for file in [
        "api.kl",
        "lib.kl",
        "example.test.kl",
        "README.md",
        ".gitignore",
    ] {
        assert!(
            p.path(&format!("src/test/klausur/{file}")).exists(),
            "{file}\n{out}"
        );
    }
    let (code, out) = klausur(&["test", "src/test/klausur"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("✓ the service answers"), "{out}");

    // Existing files are kept.
    p.file("src/test/klausur/api.kl", "// mine\n");
    let (_, out) = klausur(&["init"]);
    assert!(out.contains("kept"), "{out}");
    assert_eq!(
        std::fs::read_to_string(p.path("src/test/klausur/api.kl")).unwrap(),
        "// mine\n"
    );

    // Basic auth: `logged_in()` sends the credentials with every request of the case.
    let (code, out) = klausur(&["init", "basic", "--auth", "basic"]);
    assert_eq!(code, Some(0), "{out}");
    p.file(
        "basic/auth.test.kl",
        r#"
import "lib.kl"
@echo() { connection { GET {{base}}/echo } }
test "credentials are sent" logged_in() {
    expect echo().body.headers.authorization == `Basic ${base64_encode("user:password")}`
}
"#,
    );
    let (code, out) = klausur(&["test", "basic"]);
    assert_eq!(code, Some(0), "{out}");

    // Bearer: the generated login request and modifier load.
    let (code, out) = klausur(&["init", "bearer", "--auth", "bearer"]);
    assert_eq!(code, Some(0), "{out}");
    let (code, out) = klausur(&["list", "bearer"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("@login(") && out.contains("fn logged_in()"),
        "{out}"
    );
}

#[test]
fn unique_is_built_in_and_overridable() {
    let server = start();
    let p = project(
        "unique",
        r#"
test "built in" { expect regex_match(unique("alice"), "^alice-[0-9a-f]{8}$") && unique().len() == 8 }
"#,
    );
    p.file(
        "own.test.kl",
        "fn unique(p) { `own-${p}` }\ntest \"own\" { expect unique(\"x\") == \"own-x\" }\n",
    );
    // One namespace across loaded files: a suite's own `unique` replaces the built-in everywhere,
    // so the two files run separately.
    for file in ["t.test.kl", "own.test.kl"] {
        let out = p.run(&server, &["test", file]);
        assert_eq!(out.code, 0, "{}", out.all());
        assert!(out.stdout.contains("1 passed"), "{}", out.all());
    }
}

/// A server like Tomcat refusing a too large upload: it answers 413 after reading only the start
/// of the body, then closes the connection.
fn refusing_server() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut seen = Vec::new();
            let mut buf = [0u8; 16 * 1024];
            // The headers and about 64 KB of the body, then the answer.
            while seen.len() < 64 * 1024 {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 413 Payload Too Large\r\nContent-Type: text/plain\r\nContent-Length: 13\r\nConnection: close\r\n\r\nfile too big\n",
            );
            let _ = stream.flush();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    base
}

#[test]
fn early_responses_to_large_uploads() {
    let server = start();
    let refusing = refusing_server();
    let p = project(
        "early",
        &format!(
            r#"
@upload() {{
    connection {{ POST {refusing}/upload }}
    send = blob(20000000, 65)
}}
test "a refused upload reports the server's answer" {{
    let r = upload()
    expect r.status == 413
    expect r.body.contains("too big")
}}
"#
        ),
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

#[test]
fn undo_paths_resolve_where_registered() {
    let server = start();
    let p = project(
        "undo-paths",
        r#"
fn fixture_file() {
    this.before(|ctx| {
        write_file("out/hook.txt", "x")
        undo("remove the hook file", || remove_file("out/hook.txt"))
    })
}
test "undo from a hook" rollback fixture_file() { expect file_exists("out/hook.txt") }
test "undo from the body" rollback {
    write_file("out/body.txt", "x")
    undo("remove the body file", || remove_file("out/body.txt"))
}
test "the files are gone" { expect !file_exists("out/hook.txt") && !file_exists("out/body.txt") }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("3 passed"), "{}", out.all());
}

#[test]
fn reserved_words_as_keys() {
    let server = start();
    let p = project(
        "reserved-keys",
        r#"
@echoJson(payload) {
    connection { POST {{base}}/echo }
    send = payload
}
test "reserved words as keys" {
    let sent = parse_json(echoJson(#{ default: 1, new: #{ case: 2 }, call: 3 }).body.body)
    expect sent.default == 1 && sent.new.case == 2 && sent.call == 3
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

#[test]
fn skip_tag() {
    let server = start();
    let p = project(
        "skip-tag",
        r#"
test "ui only" tags(["ui"]) { expect true }
test "api" tags(["api"]) { expect true }
test "untagged" { expect true }
"#,
    );
    let out = p.run(&server, &["test", "--skip-tag", "ui"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("2 passed"), "{}", out.all());
    assert!(!out.stdout.contains("ui only"), "{}", out.all());
}

#[test]
fn long_labels_are_shortened() {
    let server = start();
    let long = "a".repeat(150);
    let p = project(
        "long-labels",
        &format!(r#"test "long label" (x in ["{long}"]) label(x) {{ expect false }}"#),
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    // The list line is shortened; the failure section shows the whole label.
    let short = format!("✗ long label [{}…]", "a".repeat(79));
    assert!(out.stdout.contains(&short), "{}", out.all());
    assert!(
        out.stdout.contains(&format!("label: {long}")),
        "{}",
        out.all()
    );
}

#[test]
fn cookie_jar() {
    let server = start();
    let p = project(
        "cookies",
        r#"
@setCookie(name, value) { connection { GET {{base}}/set-cookie/{{name}}/{{value}} } }
@echoGet() { connection { GET {{base}}/echo } }
@echoWithCookie() {
    connection {
        GET {{base}}/echo
        Cookie: mine=1
    }
}

test "a jar keeps the cookies" cookies {
    setCookie("sid", "abc")
    setCookie("lang", "de")
    expect echoGet().body.headers.cookie == "lang=de; sid=abc"
    expect ctx.cookies.sid == "abc"
}
test "each case gets its own jar" (n in [1, 2]) cookies parallel {
    expect echoGet().body.headers.cookie == ()
    setCookie("n", n)
    expect echoGet().body.headers.cookie == `n=${n}`
}
test "a cookie header set by the request wins" cookies {
    setCookie("sid", "abc")
    expect echoWithCookie().body.headers.cookie == "mine=1"
}
test "set_cookie" cookies {
    set_cookie(`${base}/`, "token=xyz; Path=/")
    expect echoGet().body.headers.cookie == "token=xyz"
}
test "undo requests use the jar" cookies rollback {
    setCookie("sid", "abc")
    undo("check", || {
        let r = echoGet()
        if r.body.headers.cookie != "sid=abc" { throw "no cookie in the undo" }
    })
}
test "without the modifier, nothing is kept" {
    setCookie("sid", "abc")
    expect echoGet().body.headers.cookie == ()
    expect ctx.cookies == ()
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("7 passed"), "{}", out.all());
}

#[test]
fn setup_and_teardown() {
    let server = start();
    let p = project(
        "setup",
        r#"
// A user for the whole file: its undo deletes it after the last test.
setup let admin = createUser(name: "setup-admin").body
setup {
    write_file("out/marker.txt", "x")
    undo("remove the marker", || remove_file("out/marker.txt"))
}
// Runs first at the end: teardowns and undos unwind in reverse order.
teardown write_file("out/teardown.txt", `marker was there: ${file_exists("out/marker.txt")}`)

test "a setup value" { expect admin.name == "setup-admin" }
test "parallel cases get it too" (n in 1..=3) parallel { expect admin.id > 0 && getUser(admin.id).status == 200 }
test "setup ran once" { expect file_exists("out/marker.txt") }
"#,
    );
    let out = p.run(&server, &["test", "-j", "3"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("5 passed"), "{}", out.all());
    assert!(out.stdout.contains("· setup"), "{}", out.all());
    assert!(out.stdout.contains("· teardown"), "{}", out.all());
    // The user setup created is deleted, the marker removed, and the teardown ran before that.
    assert!(server.state.lock().unwrap().users.is_empty());
    assert!(!p.path("out/marker.txt").exists());
    assert_eq!(
        std::fs::read_to_string(p.path("out/teardown.txt")).unwrap(),
        "marker was there: true"
    );
}

#[test]
fn a_failed_setup_skips_the_files_tests() {
    let server = start();
    let p = project(
        "setup-fails",
        r#"
setup let u = createUser(name: "first").body
setup createUser(name: "conflict")
test "a" { expect true }
test "b" (n in [1, 2]) { expect true }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    for line in ["✗ setup", "○ a — setup failed", "○ b — setup failed"] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }
    assert_eq!(out.stdout.matches("○ b").count(), 1, "{}", out.all());
    assert_eq!(out.stdout.matches("✗ setup").count(), 1, "{}", out.all());
    // What the setup did before it failed is cleaned up.
    assert!(server.state.lock().unwrap().users.is_empty());
}

#[test]
fn setup_needs_tests_in_its_file() {
    let server = start();
    let p = Project::new("setup-lib");
    p.file("lib.kl", "setup log(1)\n");
    p.file(
        "t.test.kl",
        "import \"lib.kl\"\ntest \"a\" { expect true }\n",
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 2, "{}", out.all());
    assert!(
        out.all().contains("this file has no tests for `setup`"),
        "{}",
        out.all()
    );
}

#[test]
fn a_failed_teardown_is_dirty() {
    let server = start();
    let p = project(
        "teardown-fails",
        r#"
teardown { throw "the cleanup broke" }
test "a" { expect true }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 3, "{}", out.all());
    assert!(out.stdout.contains("teardown"), "{}", out.all());
    assert!(out.stdout.contains("DIRTY"), "{}", out.all());
    assert!(out.all().contains("the cleanup broke"), "{}", out.all());
}

#[test]
fn setup_values_belong_to_their_file() {
    let server = start();
    let p = Project::new("setup-own-file");
    p.file(
        "a.test.kl",
        "setup let common = 1\ntest \"a\" { expect common == 1 }\n",
    );
    p.file("b.test.kl", "test \"b\" { expect common == 1 }\n");
    // Whichever file runs first, b can't see a's value.
    for order in [["a.test.kl", "b.test.kl"], ["b.test.kl", "a.test.kl"]] {
        let out = p.run(&server, &["test", order[0], order[1]]);
        assert_eq!(out.code, 1, "{}", out.all());
        assert!(out.stdout.contains("✓ a"), "{}", out.all());
        assert!(out.stdout.contains("✗ b"), "{}", out.all());
        assert!(
            out.stdout
                .contains("`common` is set by `setup` in a.test.kl"),
            "{}",
            out.all()
        );
    }
    // `klausur run` doesn't run setup.
    p.file(
        "a.test.kl",
        "setup let common = 1\nfn value() { common }\ntest \"a\" { expect true }\n",
    );
    let out = p.run(&server, &["run", "a.test.kl#value"]);
    assert_ne!(out.code, 0, "{}", out.all());
    assert!(out.all().contains("is set by `setup`"), "{}", out.all());
}

#[test]
fn setup_has_no_ctx() {
    let server = start();
    let p = project(
        "setup-ctx",
        "setup ctx.user = 1\ntest \"a\" { expect true }\n",
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(
        out.stdout.contains("there's no `ctx` in `setup`"),
        "{}",
        out.all()
    );
}

#[test]
fn teardown_runs_when_the_run_stops_early() {
    let server = start();
    let p = project(
        "setup-fail-fast",
        r#"
setup {
    write_file("out/marker.txt", "x")
    undo(|| remove_file("out/marker.txt"))
}
test "a" { expect false }
test "b" { expect true }
"#,
    );
    let out = p.run(&server, &["test", "--fail-fast"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(!out.stdout.contains("✓ b"), "{}", out.all());
    assert!(out.stdout.contains("· teardown"), "{}", out.all());
    assert!(!p.path("out/marker.txt").exists());
}

#[test]
fn setup_runs_only_if_a_test_of_its_file_runs() {
    let server = start();
    let p = Project::new("setup-filtered");
    p.file(
        "a.test.kl",
        "setup write_file(\"out/a-setup.txt\", \"x\")\ntest \"in a\" { expect true }\n",
    );
    p.file("b.test.kl", "test \"in b\" { expect true }\n");
    let out = p.run(&server, &["test", ".", "in b"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(!out.stdout.contains("setup"), "{}", out.all());
    assert!(!p.path("out/a-setup.txt").exists());
}

#[test]
fn template_strings_are_rewritten() {
    let server = start();
    let p = project(
        "template-strings",
        r#"
let m = #{ default: 1 }
fn greet(name, greeting = "hi") { `${greeting} ${name}` }
test "reserved keys" { expect `${m.default}` == "1" }
test "named arguments" { expect `<${greet(name: "a", greeting: "yo")}>` == "<yo a>" }
test "nested" { expect `a${ `b${m.default}` }c` == "ab1c" }
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("3 passed"), "{}", out.all());
}

#[test]
fn setup_waits_for_the_cases_of_earlier_files() {
    let server = start();
    let p = Project::new("setup-waits");
    p.file("api.kl", API);
    p.file(
        "a.test.kl",
        r#"import "api.kl"
test "a" (n in 1..=2) parallel {
    write_file(`out/a${n}`, "running")
    slow(300)
    remove_file(`out/a${n}`)
}
"#,
    );
    p.file(
        "b.test.kl",
        r#"import "api.kl"
setup {
    slow(150) // the cases of a.test.kl are under way by now, unless setup waited for them
    expect !file_exists("out/a1") && !file_exists("out/a2")
}
test "b" parallel { expect true }
"#,
    );
    let out = p.run(&server, &["test", "a.test.kl", "b.test.kl", "-j", "4"]);
    assert_eq!(out.code, 0, "{}", out.all());
    assert!(out.stdout.contains("3 passed"), "{}", out.all());
}

#[test]
fn large_uploads_arrive_whole() {
    // The early-response check between body writes mustn't disturb an upload the server takes.
    let server = start();
    let p = project(
        "large-upload",
        r#"
@echoText(text) {
    connection { POST {{base}}/echo }
    send = text
}
test "8 MB" {
    let body = "0123456789abcdef"
    for i in 0..19 { body += body }
    let r = echoText(body)
    expect r.status == 200
    expect r.body.body.len() == 8 * 1024 * 1024 && r.body.body == body
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 0, "{}", out.all());
}

#[test]
fn file_handling_edge_cases() {
    let server = start();
    let p = project(
        "files",
        r#"
fn bytes(list, text = "") {
    let b = blob()
    for x in list { b.push(x) }
    b + text.to_blob()
}
test "json and csv ignore a byte order mark" {
    write_file("bom.json", bytes([0xef, 0xbb, 0xbf], `{"a": 1}`))
    write_file("bom.csv", bytes([0xef, 0xbb, 0xbf], "name,n\nx,1\n"))
    expect json("bom.json").a == 1 && csv("bom.csv")[0].name == "x"
}
test "read_file keeps line endings" {
    write_file("crlf.txt", "a\r\nb\r\n")
    expect read_file("crlf.txt") == "a\r\nb\r\n"
}
test "read_file on binary data names read_blob" {
    write_file("bin.dat", bytes([0xff, 0xfe]))
    expect read_blob("bin.dat").len() == 2
    read_file("bin.dat")
}
test "an undone write removes the directories it made" rollback {
    write_file("made/by/write/x.txt", "x", undo: true)
}
test "but not those that were there" rollback {
    write_file("kept/x.txt", "x")
    write_file("kept/new/y.txt", "y", undo: true)
}
test "after the undos" {
    expect !file_exists("made") && file_exists("kept/x.txt") && !file_exists("kept/new")
}
test "watched lines are complete lines" {
    let w = watch_file("app.log")
    write_file("app.log", "first\nhalf a li")
    expect w.lines() == ["first"] && w.text() == "first\nhalf a li"
}
"#,
    );
    let out = p.run(&server, &["test"]);
    assert_eq!(out.code, 1, "{}", out.all());
    assert!(out.stdout.contains("6 passed, 1 failed"), "{}", out.all());
    // The report wraps long lines, and where depends on the temp path: compare the words.
    let words: Vec<&str> = out
        .stdout
        .split_whitespace()
        .filter(|w| *w != "|")
        .collect();
    assert!(
        words
            .join(" ")
            .contains("isn't UTF-8 text; `read_blob(path)` reads it as bytes"),
        "{}",
        out.all()
    );
}
