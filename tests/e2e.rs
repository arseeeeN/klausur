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

test "waits for the job" { expect job("a").polls == 3 }
test "closure condition" { expect unflake("b").body.attempts == 3 }
test "gives up" { never("c") }
test "timeouts count as not yet" { expect warmup("d").body.ready }
test "until must be a boolean" { badUntil("e") }
test "sleep honours the test timeout" timeout("200ms") { sleep(5000) }
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
    ] {
        assert!(out.stdout.contains(line), "missing {line:?}\n{}", out.all());
    }

    p.file(
        "bad.kl",
        r#"
@a() poll(body.ok) { connection { GET http://x } }
@b() poll(every: "1s") { connection { GET http://x } }
@c() poll(until: true, evry: "1s") { connection { GET http://x } }
"#,
    );
    let out = p.run(&server, &["test", "bad.kl"]);
    assert_eq!(out.code, 2, "{}", out.all());
    for line in [
        "`poll` arguments need names",
        "`poll` needs an `until` condition",
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
