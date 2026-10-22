mod support;

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
test "while holds for the whole time" { expect stays("f", 1000).polls >= 6 }
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
        "`never` polled 4 times over",
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
    expect p.starts_with("/")
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
