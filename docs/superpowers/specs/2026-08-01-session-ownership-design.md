# Session ownership — `UserId`, the owner column, and the authorization choke points

> **Status:** IMPLEMENTED — shipped in [#123](https://github.com/robhicks/otto/pull/123) as slice 1a
> of GitHub issue #115 (multitenancy). Review closed two gaps this spec had deferred: the handover
> ownership check (`PromoteToRemote`/`DemoteToLocal`) landed here rather than in slice 1b, and
> `accept_demotion` now binds the returned bundle to the requested session id.
> **Implements:** the session-ownership half of the "Suggested first slice" of
> [#115](https://github.com/robhicks/otto/issues/115).
> **Blocks:** `docs/superpowers/specs/2026-08-01-multitenant-identity-design.md` (slice 1b —
> the `Authenticator` seam, TOTP + JWT, `Login`/`Logout`), which builds directly on this.

Sessions have no owner today: `SessionStore::create_session(goal, config)` takes no principal, the
sqlite `sessions` table has no owner column, and `replay_since`/`session_status`/`snapshot` are keyed
by `SessionId` alone — so any caller who can name a session id can read its full event log. This
slice adds the owner, the scoped reads, and the authorization choke point, **without adding a
credential and without changing any observable behavior.**

---

## Why this is its own slice

Issue #115's suggested first slice bundles ownership with identity (the `Authenticator` seam, TOTP,
JWT, `Login`/`Logout`). Three rounds of design review on the combined spec produced two findings
that split it:

1. **The identity half is not ready.** Its promote-receiver credential (`AuthMode::Machine`) could
   not be made safe without slice 3's per-session secret map — the details are recorded under "Why
   this is deferred" in the identity spec. Blocking the mechanical foundation on an unsettled
   security design helps nobody.
2. **Reviewed together, the mechanical work drowns the security work.** Ownership is ~35 call-site
   edits with no security content. Identity is every security decision in the design. A reviewer —
   and in this repo the security review reasons from the diff alone — reads a diff that is mostly
   `create_session(&owner, …)` churn and loses the signal.

Shipping ownership first makes the identity diff a pure security change.

**What this slice does not do.** It changes no security property. With no identity yet, every
served session is owned by the same reserved principal, so every ownership check passes trivially.
The value delivered is the schema, the choke points, and their tests — so that when slice 1b
introduces real principals, isolation becomes live without touching the store or `EngineService`
again. This is stated plainly so nobody reads the merged PR as "otto is now multitenant".

---

## Premise corrections

Both from the combined spec's review; both verified against the tree and both load-bearing here.

1. **There is no migration mechanism in `persistence`, so adding a `NOT NULL` column is not a no-op
   — it is a latent runtime failure.** `SqliteStore::open` calls `init_schema`
   (`crates/persistence/src/sqlite.rs:27`), which issues three bare `CREATE TABLE IF NOT EXISTS`
   statements (`sqlite.rs:31-67`). There is no migrations directory, no `user_version` use, and no
   schema probe. Against a database file that already exists, `IF NOT EXISTS` silently keeps the
   **old** table shape and every later query naming `owner` fails with a confusing sqlx column
   error. Issue Decision 3 ("existing local dev sqlite files are simply discarded") is the right
   *policy*, but discarding must be **enforced and diagnosed**, not assumed. Closed by §2.1.

2. **`export_promotion` cannot be owner-parameterized.** Its only caller is the `/export` handler
   (`crates/engine/src/serve.rs:322`), which is machine-credentialed and has no connected principal.
   Giving it an `owner` parameter would leave that caller nothing to pass but
   `store.owner_of(session)` — feeding a value derived from the session back in as the check on that
   same session. That tautology looks like an authorization check and enforces nothing. It is
   grouped with the machine-to-machine methods instead (§3.2).

---

## Scope

**In:** `UserId` in `protocol`; `sessions.owner` + the `user_version` guard + `SessionState.owner` in
`persistence`; a principal on `create_session`, `owner_of`, and owner-scoped
`replay_since`/`session_status`/`snapshot`; the owner derivation in `otto-remote`'s `promote()`;
the `authorize` choke point on `EngineService`; `UserId::local()` threaded through every existing
call site so behavior is unchanged.

**Out:** every credential-bearing thing — the `Authenticator` seam, TOTP, JWT, `Login`/`Logout`,
the WS handshake, `--single-user`, the `OTTO_TOKEN` rename, and all `ui-dioxus` changes. Those are
slice 1b. Also out: per-owner workspace roots (its own slice), and slice 3's handover credentials.

**No `ui-dioxus` change at all.** No protocol frame is added or removed, `?token=` is untouched, and
`ServerMessage` gains no variant — so `app.rs`'s exhaustive match still compiles and the desktop app
is unaffected. This is a deliberate property of the split, not a coincidence.

---

## Goal & Success Criteria

Give every session a `NOT NULL` owner and route every session-bearing operation through one
authorization choke point, so that slice 1b turns isolation on by supplying real principals rather
than by restructuring the store.

1. `cargo test --workspace` is green with no network and no environment variables set;
   `cargo clippy --workspace --all-targets` and `cargo fmt --all --check` are clean. The only
   pre-existing failure, `otto-mcp-lsp`'s `full_round_trip_against_a_real_rust_analyzer`, is
   unaffected.
2. **Observable behavior is unchanged, with one narrow documented exception.** `otto run` produces
   the same event sequence, `otto serve` authenticates exactly as before, and no wire type gains or
   loses a variant.

   The exception, found during implementation and recorded rather than glossed: a client that
   attaches with an explicit `?session=<uuid>` naming a session that **does not exist** now gets
   `no session <id>` from its first `SendPrompt`, where previously the turn ran and wrote events
   for a session with no `sessions` row. Attach itself is unchanged — `resolve_session` still
   accepts the id blind and still sends `Ready` (§3.4) — so the reconnect path `ui-dioxus` uses is
   unaffected; only a *nonexistent* id behaves differently, and it now fails fast instead of
   creating orphaned rows. That prior behavior was a latent bug, not a contract, and no test
   asserted it.

   A second, much smaller textual divergence, recorded for exhaustiveness: `abort` on an unknown
   session now fails at `authorize` with `no session <id>` where it previously fell through to
   `set_status`'s `set_status: no session <id>`. Error in both cases, so no pass/fail behavior
   changed — only the string, which nothing pattern-matches on.
3. A scoped read for the wrong owner returns a message **byte-for-byte identical** to the one for a
   nonexistent session (asserted by a test comparing the two strings), so the API is not an
   existence oracle.
4. `snapshot`/`restore`/`restore_over` preserve the owner across a promote round-trip (asserted).
5. Opening a pre-ownership database fails at `SqliteStore::open` with an actionable message naming
   the file; opening a fresh one succeeds and stamps `user_version = 1` (both asserted).
6. `ui-dioxus/` is untouched — `git diff --stat` shows no file under it.

---

## Assumptions

| # | Assumption | Rationale |
|---|---|---|
| A1 | Every session created before slice 1b is owned by a reserved built-in principal, `UserId::local()` == `"local"`. | There is no identity yet, so there is exactly one principal. A reserved id keeps the column `NOT NULL` from the start, which Decision 3 requires — no synthetic default, no nullable "unowned" state, no backfill. |
| A2 | `UserId` lives in `protocol` even though nothing puts it on the wire in this slice. | It is a wire type in slice 1b (`Credentials`, `LoggedIn`), and `SessionState` — which `PromoteBundle` serializes and ships between machines — carries it *now*. Putting it anywhere else would mean moving it one slice later. |
| A3 | The scoped reads are scoped by SQL predicate (`WHERE id = ?1 AND owner = ?2`), not by a fetch-then-compare in Rust. | The check lands in the same statement as the read, so no caller can forget it and no early-return can skip it. |
| A4 | `append_event`, `record_turn`, `next_seq`, `next_turn`, and `set_status` stay **unscoped**. | They are reachable only from inside a turn `EngineService` has already authorized, none of them returns another tenant's data, and scoping them would roughly triple the churn. A deliberate trade, recorded in the trait's module doc so it is not read as an oversight. |
| A5 | `SessionStatus`, `TurnRecord`, and the events/turns tables are untouched. | Ownership is a property of the session, and the child tables are already reachable only through it. |
| A6 | The `user_version` guard is added to `persistence` only, not to `retrieval`'s sqlite index. | The retrieval index is a derived cache in the OS cache dir, keyed by workspace root and rebuilt on demand; it has no durable user data to protect and no schema change here. |

---

## 1. `protocol` — `UserId`

```rust
/// A principal's stable identifier. Validated on construction: 1–64 characters of
/// `[a-z0-9._-]`, which keeps it safe as a sqlite key, as an `otpauth://` URI label
/// (slice 1b), and in a log line without escaping.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct UserId(String);

impl UserId {
    pub fn parse(s: &str) -> Result<Self, InvalidUserId>;
    /// The reserved principal that owns every session until slice 1b introduces real
    /// identities. Never enrollable — slice 1b's `otto auth enroll` refuses it.
    pub fn local() -> Self;
    pub fn as_str(&self) -> &str;
}
```

`try_from = "String"` matters: validation then runs **on deserialization too**, so a `SessionState`
arriving in a `PromoteBundle` from another machine cannot carry an owner that bypasses the charset
rule. Without it the newtype would be validated only on the paths that happen to call `parse`.

`protocol` gains no dependency — this is `serde` and a character check.

---

## 2. `persistence` — the owner column

### 2.1 Schema and the `user_version` guard

`sessions` gains `owner TEXT NOT NULL`, plus an index on it (ownership implies a "this principal's
sessions" query, and slice 1b adds one).

Because `CREATE TABLE IF NOT EXISTS` cannot alter an existing table (premise correction 1),
`init_schema` is preceded by a version probe:

```rust
const SCHEMA_VERSION: i64 = 1;

// PRAGMA user_version is 0 on both a brand-new file and every pre-ownership database.
// The two are told apart by whether `sessions` already exists.
match (user_version, sessions_table_exists) {
    (0, false) => { create_schema()?; set_user_version(SCHEMA_VERSION)?; }
    (0, true)  => bail!(
        "session database at {path} predates session ownership (issue #115) and has no \
         owner column. otto has no installed base, so there is no migration: delete the \
         file and let otto re-create it."
    ),
    (v, _) if v == SCHEMA_VERSION => {}
    (v, _) => bail!(
        "session database at {path} has schema version {v}, newer than this otto build \
         understands ({SCHEMA_VERSION}); upgrade otto"
    ),
}
```

Fail-closed and actionable, rather than a sqlx "no such column: owner" surfacing hours later from an
unrelated query. The forward-version arm matters because sqlite will happily let an older binary
open a newer file.

### 2.2 `SessionState`

Gains `pub owner: UserId`. `snapshot`/`restore` therefore carry ownership across a promote: a
restored session belongs to whoever owned it on the source, not to whoever pushed the bundle. This
breaks `PromoteBundle`'s serialization, which Decision 3 permits.

### 2.3 Trait changes

```rust
async fn create_session(&self, owner: &UserId, goal: &str, config: &Value)
    -> anyhow::Result<SessionId>;

/// The owner of `session`, or `Err` if there is no such session.
///
/// UNSCOPED, and a reverse existence oracle: it distinguishes "exists, owned by someone
/// else" from "does not exist" — exactly the distinction the scoped reads below hide.
/// That is required, because `EngineService::authorize` needs the comparison. It must
/// therefore NEVER back a client-facing path.
async fn owner_of(&self, session: SessionId) -> anyhow::Result<UserId>;

// Owner-scoped. Each returns the *same* error for "no such session" and "not yours".
async fn replay_since(&self, owner: &UserId, session: SessionId, after_seq: Option<u64>)
    -> anyhow::Result<Vec<Event>>;
async fn session_status(&self, owner: &UserId, session: SessionId)
    -> anyhow::Result<SessionStatus>;
async fn snapshot(&self, owner: &UserId, session: SessionId) -> anyhow::Result<SessionState>;
```

One shared error constructor produces the not-found message for both cases, so the two paths cannot
drift apart and start distinguishing themselves (success criterion 3 asserts the strings are equal).

`restore`/`restore_over` take the owner from `state.owner`; no signature change.

The `owner_of` warning is not decorative: it is reachable from outside via the public
`EngineService::store()` accessor (`crates/engine/src/service.rs:127`).

---

## 3. `engine` — the choke point

### 3.1 Client-facing methods

`run_prompt`, `run_prompt_with_controls`, `run_command_with_controls`, `run_agent_with_controls`,
and `abort` take `owner: &UserId` and call a private `authorize(owner, session)` first, which
compares against `store.owner_of(session)` and returns the shared not-found error on mismatch.
`create_session` takes the owner and passes it through.

### 3.2 Machine-to-machine methods

`accept_promotion`/`accept_demotion` read the owner from the bundle's `SessionState` (§2.2);
`export_promotion` reads it via `owner_of` — which it needs anyway, because §2.3 makes `snapshot`
owner-scoped. None of them takes an `owner` parameter, per premise correction 2.

### 3.3 Call sites that pass `UserId::local()`

This is the bulk of the diff and the reason the slice exists separately. All of it is mechanical:

| Location | Count |
|---|---|
| `crates/engine/src/lib.rs:570` (`run_goal`) | 1 |
| `crates/engine/src/serve.rs:576` (replay) | 1 |
| `crates/engine/src/service.rs`'s `#[cfg(test)]` module | 21 `create_session`, 11 `run_prompt`, 1 `abort` |
| **`crates/remote/src/lib.rs:158`** (`promote()` calls `store.snapshot`) | **library code, not tests** |
| `crates/persistence/src/{sqlite.rs,types.rs}` test modules | 23 + 3 tests |
| `SessionState` struct literals (a field is being added) | 12: `persistence/src/sqlite.rs:634,667`, `persistence/src/types.rs:87`, `engine/src/service.rs:822,861,897,1032`, `remote/src/lib.rs:370,410`, `remote/src/fly.rs:493`, `engine/tests/vps_promote.rs:29`, `engine/tests/microvm.rs:35` |
| `crates/engine/tests/{serve,promote,vps_promote,microvm}.rs` | 4 files (`cors.rs`, `ui_dir.rs`, and `remote_workspace.rs` need no edit — the last one's `snapshot()` calls are `Workspace::snapshot`, not the store's) |
| `crates/engine/src/loopback.rs:50` (`restore`) | owner rides in the bundle — no change |

**`crates/remote` is affected, and its library is.** `promote()` (`crates/remote/src/lib.rs:150-161`)
calls `store.snapshot(session)`; with `snapshot` owner-scoped it must derive the owner via
`owner_of`, exactly as `export_promotion` does and for the same reason — `promote()` is invoked from
a path that has already authorized the caller, and re-deriving the owner from the session would be a
tautology if used as a check. It is used only to satisfy the scoped read.

### 3.4 What `serve.rs` deliberately does *not* get

`serve.rs` gains **no** authentication change: `authorized`/`authorized_ws` (`:73-85`) and
`ConnectParams` (`:39-50`) are untouched. Only the replay call at `:576` passes a principal.

**`resolve_session` (in `crates/engine/src/serve.rs`) deliberately does not gain an ownership check in this slice.**
It is tempting — it is the hole issue #115 opens with — but adding it here would break success
criterion 2. Today an unknown `?session=` uuid is accepted blind and receives a `Ready` frame;
routing attach through `owner_of` turns that into an error and a closed socket, and
`ui-dioxus/src/net/url.rs:21` appends `&session=` on reconnect, so a stale client-side id would
hard-fail where it previously attached. That is a `ui-dioxus`-visible behavior change from a slice
that promises none.

Nothing is lost by waiting: enforcement lives in `EngineService::authorize`, so a connection that
attaches to a session it does not own gets the shared not-found error on its **first command** and
learns nothing in the meantime — `replay_since` at `:576` is already owner-scoped and returns empty.
With exactly one principal the attach-time check is unobservable anyway. Slice 1b adds it together
with real principals, and can fix the pre-existing "unknown session attaches blind" oddity at the
same time, where the behavior change is in scope and reviewable.

**`PromoteToRemote` / `DemoteToLocal` were the exception, and the check shipped here rather than
being deferred.** This section originally recorded them as an open gap for the identity slice.
Review argued that the reasoning above does not transfer: unlike attach, adding the check is
behaviour-neutral, and promote ships a session's whole event log off-machine while demote
overwrites the local row *including its owner*. So `serve.rs`'s handover arm now calls
`EngineService::authorize_session` before `handle_handover`, and
`crates/engine/tests/serve.rs::handover_refuses_a_session_the_connection_does_not_own` is the
regression test.

The gap existed because that path does not route through an `EngineService` method — it reaches
`otto_remote::promote` via the `store()` accessor — so `authorize` was bypassed by construction
rather than by omission.

**Still owner-blind, and genuinely deferred:** `POST /export` and `POST /promote` are bearer-only,
so any token holder can export any session id or restore a bundle naming any owner; and
`resolve_session`'s explicit `?session=` arm attaches without a check. All three are inert while
`local` is the only principal.

---

## 4. Error Handling & Edge Cases

| Case | Behavior |
|---|---|
| Pre-ownership database | `SqliteStore::open` fails with the §2.1 message naming the file. |
| Database from a newer otto | `open` fails naming both versions. |
| Fresh database | Schema created, `user_version` stamped to 1. |
| Scoped read, wrong owner | The nonexistent-session error, byte-for-byte identical. |
| `owner_of` on a missing session | `Err` — and it *is* an oracle, which is why it is documented as never client-facing. |
| Restored bundle naming an unknown owner | Accepted. Ownership is data; there is no user table to check against in this slice, and demote must work regardless. |
| `UserId` with an invalid charset, over the wire | Rejected at deserialization by `try_from` (§1). |
| `otto run` | Owner is `UserId::local()`; no other change. |

---

## 5. Testing

- **`protocol`** — `UserId::parse` accepts the legal charset and rejects empty, over-64, uppercase,
  and path/quote characters; deserializing an illegal id fails (the `try_from` guard); `local()`
  round-trips. Plain `#[test]`, matching the file's existing style.
- **`persistence`** — `create_session` records the owner; each scoped read returns the row for the
  right owner and the identical error for both wrong-owner and nonexistent (asserting string
  equality, not just `is_err`); `snapshot`/`restore`/`restore_over` preserve the owner; the
  `user_version` guard rejects a hand-built legacy database (create the old schema by hand, then
  `open`), accepts a fresh one, and rejects a forward version. Uses the existing `temp_store()`
  fixture shape (`sqlite.rs:376-382`).
- **`engine`** — `authorize` rejects a mismatched owner on each client-facing method with the shared
  error; the machine-to-machine methods work with no owner argument; a promote round-trip preserves
  the owner end to end (`tests/promote.rs`).
- **Determinism guard** — `otto run` end-to-end unchanged; the suite needs no network and no keys.

Because behavior is unchanged, the seven integration harnesses need only mechanical updates. If any
of them needs a *semantic* change, that is a signal the slice has grown beyond its scope.

---

## 6. Risks & Open Questions

1. **This slice changes no security property**, and a reader of the merged PR could mistake it for
   one that does. Mitigated by saying so in the spec's opening, in the PR body, and in the commit
   message.
2. **`owner_of` is a deliberate reverse oracle.** Safe only while its sole caller is `authorize`.
   The doc comment is the enforcement; a lint would be better and does not exist.
3. **The `user_version` guard is one-way.** Once stamped, an older otto build refuses the file. That
   is correct and intended, and worth knowing before someone bisects across this commit.
4. **`~24` test call sites in `service.rs` is the largest single edit.** Mechanical, but the volume
   is where a subtle mistake (passing the wrong principal in a test that then asserts nothing about
   ownership) could hide. The scoped-read tests are the backstop.
