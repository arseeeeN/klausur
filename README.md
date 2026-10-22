# klausur

An API client and test runner in one small language. Use it to send a single request, nicer than
curl, or to run integration suites with parameterized and generated cases, and with automatic
cleanup of everything a test created.

```
@createUser(name, role = "user") {
    connection {
        POST {{base}}/users
        Content-Type: application/json
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
    connection {
        GET {{base}}/users/{{id}}
    }
}

test "a new user can be fetched" (role in ["user", "admin"]) rollback parallel {
    let user = createUser("alice", role: role)
    expect getUser(user.id).body.role == role
}

test "names are kept as sent" (name in gen::string(1..20)) rollback runs(20) {
    expect getUser(createUser(name).id).body.name == name
}
```

- **Requests are declarations:** a connection line with headers, a body, expectations,
  captures, and how to undo the request.
- **Everything else is code.** Functions and test bodies are [Rhai](https://rhai.rs) scripts, and
  requests are called like functions.
- **Tests never leave a mess silently.** With `rollback`, every undo a test registered runs when
  the case ends, in reverse order. If an undo fails, the run stops and reports exactly what is
  left behind.
- **Failures explain themselves.** You see the source, the values involved, and the request and
  response:

  ```
  ── new users are tagged
    x expectation failed: the expression is false
      ,-[users.test.kl:16:12]
   15 |     let user = createUser("bob")
   16 |     expect getUser(user.id).body.tags.contains("new")
      :            ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   17 | }
      `----
    where:
      user.id = 47
      getUser(user.id).body.tags = []
  ```

- **Test cases come from many sources:** lists, tables, CSV and JSON files, and random
  generators with reproducible seeds.
- **Modifiers you can write yourself.** `rollback`, `timeout`, `retries`, `parallel` and
  `poll` are built in, and your own are plain functions with `before`/`after`/`around` hooks.
- **Helpers:** CSS selectors over HTML and XML, JSONPath, multipart uploads, SSE, JWT, base64,
  regex and files.

## Install

**Linux and macOS**

```sh
curl -fsSL https://github.com/arseeeeN/klausur/releases/latest/download/install.sh | sh
```

**Windows** (PowerShell)

```powershell
irm https://github.com/arseeeeN/klausur/releases/latest/download/install.ps1 | iex
```

The scripts install the latest release to `~/.local/bin` or `%LOCALAPPDATA%\Programs\klausur`,
after checking it against the release checksums. Run them again to upgrade. You can also
download `klausur-<target>` for your platform from the
[releases page](https://github.com/arseeeeN/klausur/releases).

**From source:** install [Rust](https://rustup.rs), then run
`cargo install --git https://github.com/arseeeeN/klausur`.

## Usage

```sh
klausur run api.kl#createUser -a name=alice   # send one request; the body goes to stdout
klausur test                                  # every *.kl file under the current directory
klausur test tests/users.test.kl "fetched"    # files and test-name filters
klausur test --seed 503069000                 # replay the generated cases of an earlier run
klausur test -j 8 -v                          # 8 parallel cases at a time, with request traces
klausur list                                  # requests, functions and tests
```

`env("NAME")` reads the environment, then `.env.<name>` (with `--env <name>`), then `.env`.

Tests run one at a time unless they're marked `parallel`, so a test that checks global state
can't be broken by another test running alongside it.

Exit codes: 0 when everything passed, 1 for failed tests, 2 for usage and load errors, and 3
when a rollback failed and may have left state behind.

## Documentation

- [SPEC.md](SPEC.md) describes the language and the CLI in full, and §11 lists what's
  implemented.
- [GAPS.md](GAPS.md) lists what testing a real service turned up.
- [BACKLOG.md](BACKLOG.md) has the plans for after this release, such as browser tests.

## Editor support

[`editors/tree-sitter-klausur`](editors/tree-sitter-klausur) is a tree-sitter grammar with
highlight, fold and indent queries. It's also a Neovim plugin, and its README shows the setup.

## License

[MIT](LICENSE)
