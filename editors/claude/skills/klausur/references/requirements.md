# From ticket or merge request to klausur tests

The requirement says what must be true afterwards; the tests check exactly that, from the
outside. Work in four steps: gather, list behaviours, plan, write and run.

## 1. Gather the requirements

Collect the actual text, not a summary from memory:

- **A ticket** (a key like `PROJ-123` or `#123`, often in the branch name `feature/PROJ-123-…`):
  fetch it with whatever is connected: a Jira or Linear tool, `gh issue view 123`, or
  `glab issue view 123`. Take its description, acceptance criteria, comments (later comments
  often change the scope) and linked issues. If nothing can read it, ask the user to paste it.
- **A pull or merge request**: `gh pr view <n>` / `gh pr diff <n>` on GitHub, `glab mr view <n>`
  / `glab mr diff <n>` on GitLab, with `--comments` for review decisions. For the current
  branch: `git log --oneline origin/main..HEAD` and `git diff origin/main...HEAD`.
- **The user's own words**: if that's all there is, restate the behaviours you'll test in one or
  two lines before writing them, so a misunderstanding costs a sentence, not a suite.

Use the **code** for details the requirement leaves out (endpoint paths, field names, status
codes, roles in `@RolesAllowed` or route guards), never as the definition of correct: tests that
merely restate the implementation pass even when the implementation is wrong. Where code and
requirement disagree, that's a finding: say so and ask, rather than picking one.

## 2. List the behaviours

For each requirement, write down observable behaviours: *this request / UI action, with this
input, gives this response or state*. Then walk this checklist for what the requirement implies
but doesn't say. Not every item applies; the point is to consider each one:

| Area | Ask |
|---|---|
| Happy path | The main success case, with realistic data |
| Validation | Missing, empty, too long, wrong type, boundary values (0, max, max+1), invalid formats |
| Auth | Anonymous, wrong role, another user's resource, expired/forged token |
| Effects | Is the change visible where it should be (get, list, search, counts, other endpoints)? Is nothing else changed? |
| Repeats | Doing it twice: idempotent, a conflict (409), or a duplicate? |
| Not found | Unknown ids, deleted things |
| Async | Background work: is it eventually done (`poll(until:)`), or never done when it shouldn't be (`poll(while:)`)? |
| Regression | Behaviour next to the change that must keep working (the diff shows what was touched) |
| UI | If the ticket is about the UI: the user-visible result, empty and error states |

**Bug fix tickets** get a regression test that reproduces the bug as the ticket describes it.
It should fail without the fix and pass with it; if you can run it against both (`main` and the
branch), do, and say so.

## 3. Plan

Map behaviours to tests, one behaviour per test, named as the behaviour (a sentence a product
owner would recognise). Matrices become one test with parameters:
`(role in ["user", "anonymous"])`, or a `cases [ #{…} ]` table with `label(…)`.

```
| Requirement (PROJ-123)                         | Test                                        |
|----------------------------------------------|---------------------------------------------|
| Admins can deactivate users                  | an admin can deactivate a user              |
| Deactivated users can't log in               | a deactivated user can't log in             |
| Their data stays                             | deactivating keeps the user's data          |
| Only admins may deactivate (implied: 403)    | only admins can deactivate [user, anonymous]|
| (implied) twice                              | deactivating twice is harmless              |
| (implied) unknown user                       | deactivating an unknown user is a 404       |
| Audit log entry is written                   | not testable from the API: no read endpoint |
```

Show this table to the user before writing when the scope is large or unclear. List what can't
be tested from outside and why; don't drop it silently.

Group a ticket's tests in the area's test file under a comment naming the ticket
(`// PROJ-123: deactivating users`), or in a new `<area>.test.kl` if the area is new. Keep ticket
keys out of test names: names describe behaviour, which outlives tickets.

## 4. Write and run

1. Reuse requests from `api.kl`; add missing ones there, with the success invariants in
   `expect` and an `undo` for anything that creates state. Error cases get a raw variant without
   `expect`.
2. Write the tests with `rollback` and unique data, run them, read failures fully.
3. Sort each failure: test bug (fix the test), service bug (keep the test failing, report it,
   add it to `FINDINGS.md` if the suite has one), environment (fix the setup).
4. Run the whole suite before handing over, so the new tests didn't break others (shared names,
   global counts, parallel interference).

## Worked example

**PROJ-123:** *"Admins can deactivate users (`POST /users/{id}/deactivate`). Deactivated users
can't log in, but their data is kept. Only admins may do this."*

`api.kl` additions:

```
@deactivate(id, jwt) {
    connection {
        POST {{base}}/users/{{id}}/deactivate
        Authorization: Bearer {{jwt}}
    }
    expect status == 200
    undo reactivate(id, jwt)
}

// For permission checks: no expectations, no undo.
@tryDeactivate(id, jwt = ()) {
    connection {
        POST {{base}}/users/{{id}}/deactivate
        Authorization: Bearer {{jwt}}
    }
}

@reactivate(id, jwt) {
    connection {
        POST {{base}}/users/{{id}}/reactivate
        Authorization: Bearer {{jwt}}
    }
}
```

`users.test.kl`:

```
// PROJ-123: deactivating users

test "a deactivated user can't log in" rollback logged_in("admin") {
    let name = unique("dora")
    let user = createUser(name, jwt: ctx.jwt)
    login(name)                             // works before: `login` expects 200 itself
    deactivate(user.id, ctx.jwt)
    expect tryLogin(name).status == 401     // raw variant: the test checks the status
}

test "deactivating keeps the user's data" rollback logged_in("admin") {
    let user = createUser(unique("dora"), jwt: ctx.jwt)
    deactivate(user.id, ctx.jwt)
    let fetched = getUser(user.id, ctx.jwt)
    expect fetched.status == 200
    expect fetched.body.name == user.body.name
    expect fetched.body.active == false
}

test "only admins can deactivate" (who in ["user", "anonymous"]) rollback logged_in("admin") {
    let user = createUser(unique("dora"), jwt: ctx.jwt)
    let jwt = if who == "anonymous" { () } else { login(who).token }
    let r = tryDeactivate(user.id, jwt)
    expect r.status == if who == "anonymous" { 401 } else { 403 }
    expect getUser(user.id, ctx.jwt).body.active == true
}

test "deactivating twice is harmless" rollback logged_in("admin") {
    let user = createUser(unique("dora"), jwt: ctx.jwt)
    deactivate(user.id, ctx.jwt)
    expect tryDeactivate(user.id, ctx.jwt).status == 200
}

test "deactivating an unknown user is a 404" logged_in("admin") {
    expect tryDeactivate(999999999, ctx.jwt).status == 404
}
```

Notes on the choices: the request blocks carry the invariants (`deactivate` expects 200 and knows
how to undo itself); permission and error checks use the raw variant; every case that creates a
user uses `rollback`, so the users are deleted again, and the deactivation is undone before that;
the matrix of non-admins is one test with an `in` parameter.

## Report

End with the requirement → test → result table, the run's summary line and exit code, every
failure classified (service bug / test / environment) with the key line of its report, the
untestable requirements, and anything left behind.
