# vps demote-from-remote Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let a client connected to the source serve `S` (started `--promote-vps <R>`) issue `DemoteToLocal` and have `S` pull a session's current state back from the receiver `R` and restore it locally — the symmetric *pull* inverse of the *push* promote.

**Architecture:** Add one new receiver RPC (`POST /export`, gated by the existing `--accept-promotions`) that returns a `PromoteBundle`; a `VpsTarget::export` client that pulls it; and an `EngineService::accept_demotion` that restores the bundle into `S`, overwriting `S`'s own stale copy via a new `SessionStore::restore_over`. The promote receiver's fail-closed `restore`/`accept_promotion` are left intact.

**Tech Stack:** Rust (edition 2024, async/tokio), axum + axum-server transport, reqwest client, sqlx/SQLite persistence, integration tests with `tokio-tungstenite` + `reqwest`.

**Design spec:** `docs/superpowers/specs/2026-06-22-vps-demote-from-remote-design.md`

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/persistence/src/lib.rs` | Modify | Add `restore_over` to the `SessionStore` trait |
| `crates/persistence/src/sqlite.rs` | Modify | `SqliteStore::restore_over` impl (delete-then-insert) + unit tests |
| `crates/engine/src/service.rs` | Modify | Factor a validation helper + gate-filtered snapshot helper; add `accept_demotion` and `export_promotion` + unit tests |
| `crates/engine/src/remote.rs` | Modify | `VpsTarget::export` pull client |
| `crates/engine/src/serve.rs` | Modify | `POST /export` route + `export_handler`; wire the vps-demote branch of `handle_handover` |
| `crates/engine/tests/vps_promote.rs` | Modify | `/export` gating tests, demote round-trip, replace the stale `handover_vps_demote_is_unsupported` test (reuses the file's existing harness helpers) |
| `docs/ARCHITECTURE.md`, `CLAUDE.md`, the design spec | Modify | Record shipped; shrink `UnsupportedTarget` boundary to machine provisioning only |

`SqliteStore` is the only `SessionStore` impl, so the new trait method forces no other updates.

---

### Task 1: `SessionStore::restore_over` (overwrite-own-copy)

**Files:**
- Modify: `crates/persistence/src/lib.rs:61` (add trait method after `restore`)
- Modify: `crates/persistence/src/sqlite.rs:295` (add impl after `restore`)
- Test: `crates/persistence/src/sqlite.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/persistence/src/sqlite.rs` (alongside `restore_into_existing_session_is_error` at line 584):

```rust
    #[tokio::test]
    async fn restore_over_overwrites_an_existing_session() {
        let (store, _dir) = temp_store().await;
        let id = store
            .create_session("old goal", &serde_json::json!({}))
            .await
            .unwrap();
        // A different snapshot for the SAME id: new goal, fresh event, Done status.
        let advanced = SessionState {
            id,
            goal: "new goal".to_string(),
            status: SessionStatus::Done,
            config: serde_json::json!({ "m": "y" }),
            events: vec![log_event(id, 0, "fresh")],
            turns: vec![turn(0, true)],
        };
        let returned = store.restore_over(&advanced).await.unwrap();
        assert_eq!(returned, id);
        // The stale row was replaced, not duplicated or rejected.
        assert_eq!(store.snapshot(id).await.unwrap(), advanced);
        assert_eq!(store.session_status(id).await.unwrap(), SessionStatus::Done);
    }

    #[tokio::test]
    async fn restore_over_into_a_fresh_store_inserts() {
        let (source, _d1) = temp_store().await;
        let id = source.create_session("g", &serde_json::json!({})).await.unwrap();
        source.append_event(id, &log_event(id, 0, "a")).await.unwrap();
        let snap = source.snapshot(id).await.unwrap();

        let (target, _d2) = temp_store().await;
        // No existing row: restore_over behaves like restore (insert).
        assert_eq!(target.restore_over(&snap).await.unwrap(), id);
        assert_eq!(target.snapshot(id).await.unwrap(), snap);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-persistence restore_over`
Expected: FAIL to compile — `no method named restore_over found`.

- [ ] **Step 3: Add the trait method**

In `crates/persistence/src/lib.rs`, immediately after the `restore` method (ends at line 61), add:

```rust
    /// Like `restore`, but overwrites any existing rows for the session id (delete-then-insert in
    /// one transaction) instead of failing on a duplicate. This is the demote primitive: the source
    /// engine refreshes its own stale copy with the advanced state pulled back from the receiver.
    /// `restore` stays fail-on-conflict — only an explicit demote uses this.
    async fn restore_over(&self, state: &SessionState) -> anyhow::Result<SessionId>;
```

- [ ] **Step 4: Implement `restore_over` on `SqliteStore`**

In `crates/engine`... no — in `crates/persistence/src/sqlite.rs`, add this method to the `impl SessionStore for SqliteStore` block, immediately after `restore` (which closes at line 295). It mirrors `restore` but deletes the three tables' rows for this id first, inside the same transaction:

```rust
    async fn restore_over(
        &self,
        state: &crate::SessionState,
    ) -> anyhow::Result<otto_protocol::SessionId> {
        // Atomic overwrite: delete any prior rows for this id, then re-insert the whole session.
        // Either the replacement lands completely or nothing changes. Timestamps regenerated.
        let now = now_millis();
        let mut tx = self.pool.begin().await?;

        let id = state.id.0.to_string();
        sqlx::query("DELETE FROM events WHERE session_id = ?1")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM turns WHERE session_id = ?1")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE id = ?1")
            .bind(&id)
            .execute(&mut *tx)
            .await?;

        sqlx::query(
            "INSERT INTO sessions (id, goal, status, created_at, updated_at, config)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&id)
        .bind(&state.goal)
        .bind(state.status.as_db_str())
        .bind(now)
        .bind(now)
        .bind(serde_json::to_string(&state.config)?)
        .execute(&mut *tx)
        .await?;

        for event in &state.events {
            sqlx::query("INSERT INTO events (session_id, seq, kind) VALUES (?1, ?2, ?3)")
                .bind(&id)
                .bind(event.seq as i64)
                .bind(serde_json::to_string(&event.kind)?)
                .execute(&mut *tx)
                .await?;
        }

        for turn in &state.turns {
            sqlx::query(
                "INSERT INTO turns (session_id, turn_index, goal, outcome, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(&id)
            .bind(turn.turn_index as i64)
            .bind(&turn.goal)
            .bind(serde_json::to_string(&turn.outcome)?)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(state.id)
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p otto-persistence restore_over`
Expected: PASS (both tests). Also run `cargo test -p otto-persistence restore` to confirm `restore_into_existing_session_is_error` still passes (the fail-on-conflict guard is untouched).

- [ ] **Step 6: Commit**

```bash
git add crates/persistence/src/lib.rs crates/persistence/src/sqlite.rs
git commit -m "feat(persistence): add SessionStore::restore_over (overwrite-own-copy)"
```

---

### Task 2: `EngineService::accept_demotion` (restore into S, overwriting)

**Files:**
- Modify: `crates/engine/src/service.rs` (factor a helper out of `accept_promotion`; add `accept_demotion`)
- Test: `crates/engine/src/service.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

The existing tests build a service via `build_test_service` (service.rs:405) and bundles via a `one_file_bundle` helper (service.rs:502) and `PromoteBundle`. Add these two tests to the `tests` module:

```rust
    #[tokio::test]
    async fn accept_demotion_overwrites_an_existing_session() {
        use crate::remote::PromoteBundle;
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;

        // Seed S with an original copy of the session.
        let id = service.create_session("old", &serde_json::json!({})).await.unwrap();

        // A bundle for the SAME id carrying advanced state + a new workspace file.
        let bundle = PromoteBundle {
            session: SessionState {
                id,
                goal: "advanced".to_string(),
                status: otto_persistence::SessionStatus::Done,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot {
                files: vec![(std::path::PathBuf::from("out.txt"), b"PULLED".to_vec())],
            },
        };

        // accept_demotion overwrites S's stale row (no AlreadyExists), and writes the file.
        let restored = service.accept_demotion(&bundle).await.unwrap();
        assert_eq!(restored, id);
        assert_eq!(service.store().snapshot(id).await.unwrap().goal, "advanced");
        assert_eq!(
            service.workspace().read(std::path::Path::new("out.txt")).await.unwrap(),
            b"PULLED"
        );
    }

    #[tokio::test]
    async fn accept_demotion_refuses_sensitive_workspace_entry() {
        use crate::remote::PromoteBundle;
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;
        let bundle = PromoteBundle {
            session: SessionState {
                id: SessionId::new(),
                goal: "g".to_string(),
                status: otto_persistence::SessionStatus::Active,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot {
                files: vec![(std::path::PathBuf::from(".env"), b"SECRET=1".to_vec())],
            },
        };
        assert!(matches!(
            service.accept_demotion(&bundle).await,
            Err(crate::service::AcceptError::Refused(_))
        ));
    }
```

> Note: check the exact imports/aliases already present in the `tests` module (e.g. `WorkspaceSnapshot`, `SessionState`, `SessionId`, `WorkspaceRead`/`Workspace` for `.read`). Match the existing test style; add `use` lines only if the symbol isn't already in scope. `build_test_service` and `PromoteBundle` are the established fixtures.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-engine accept_demotion`
Expected: FAIL to compile — `no method named accept_demotion found`.

- [ ] **Step 3: Factor the validation helper out of `accept_promotion`**

In `crates/engine/src/service.rs`, the per-file validation loop currently inside `accept_promotion` (lines 252-294, building `edits: Vec<Edit>`) is needed verbatim by `accept_demotion`. Extract it into a private method on `impl EngineService`. Add this method (place it just above `accept_promotion`):

```rust
    /// Validate every workspace file in `bundle` through the permission gate and return the edits
    /// to apply. The inviolable sensitive-path floor is enforced here (fail-closed): a non-UTF-8
    /// path, a path escaping the root, a sensitive path, or non-UTF-8 contents aborts with nothing
    /// applied. Shared by `accept_promotion` (receiver restore) and `accept_demotion` (source pull).
    fn validate_workspace_edits(
        &self,
        bundle: &crate::remote::PromoteBundle,
    ) -> Result<Vec<Edit>, AcceptError> {
        let mut edits = Vec::with_capacity(bundle.workspace.files.len());
        for (path, bytes) in &bundle.workspace.files {
            let Some(path_str) = path.to_str() else {
                return Err(AcceptError::Refused(format!(
                    "restore refused non-UTF-8 path: {}",
                    path.display()
                )));
            };
            if path.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            }) {
                return Err(AcceptError::Refused(format!(
                    "restore refused path escaping workspace root: {path_str}"
                )));
            }
            if self.tools.check("fs.write", &json!({ "path": path_str })) == Decision::Deny {
                return Err(AcceptError::Refused(format!(
                    "restore refused sensitive path: {path_str}"
                )));
            }
            let new_contents = String::from_utf8(bytes.clone()).map_err(|_| {
                AcceptError::Refused(format!("restore: non-UTF-8 contents for {path_str}"))
            })?;
            edits.push(Edit {
                path: path.clone(),
                new_contents,
            });
        }
        Ok(edits)
    }
```

Then replace lines 252-294 of `accept_promotion` (the whole `let mut edits = … edits.push(…) }` block) with:

```rust
        // Validate the WHOLE workspace snapshot through the gate before writing anything: a
        // sensitive-path entry is refused (fail-closed) and nothing lands.
        let edits = self.validate_workspace_edits(bundle)?;
```

Leave the rest of `accept_promotion` (the `AlreadyExists` probe above, and the `store.restore` + apply-edits below) unchanged.

- [ ] **Step 4: Add `accept_demotion`**

Immediately after `accept_promotion` (closes at line 308), add:

```rust
    /// Restore a bundle pulled back FROM a remote (demote) into this (source) engine, OVERWRITING
    /// this engine's own prior copy of the session. Unlike `accept_promotion`, a present session id
    /// is expected (the source kept its copy when it promoted) and is replaced via `restore_over`.
    /// The sensitive-path floor is still enforced up front (fail-closed) before anything is written.
    pub async fn accept_demotion(
        &self,
        bundle: &crate::remote::PromoteBundle,
    ) -> Result<SessionId, AcceptError> {
        let id = bundle.session.id;
        let edits = self.validate_workspace_edits(bundle)?;
        // Overwrite the source's own (stale) session row, then the pre-validated workspace files.
        self.store
            .restore_over(&bundle.session)
            .await
            .map_err(AcceptError::Failed)?;
        for edit in &edits {
            self.workspace
                .apply_edit(edit)
                .await
                .map_err(AcceptError::Failed)?;
        }
        Ok(id)
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p otto-engine accept_demotion`
Expected: PASS (both tests).

Run: `cargo test -p otto-engine accept_promotion`
Expected: PASS — the refactor preserved promote behavior (`accept_promotion_restores_session_and_workspace`, `accept_promotion_refuses_sensitive_workspace_entry`, the parent-dir/escape tests).

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): EngineService::accept_demotion restores a pulled bundle, overwriting"
```

---

### Task 3: `EngineService::export_promotion` (receiver builds the bundle)

**Files:**
- Modify: `crates/engine/src/service.rs` (add a gate-filtered snapshot helper + `export_promotion`)
- Test: `crates/engine/src/service.rs` (`#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `tests` module:

```rust
    #[tokio::test]
    async fn export_promotion_returns_bundle_without_sensitive_files() {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        // Put a normal file and a sensitive file on disk in the workspace.
        std::fs::write(ws.path().join("out.txt"), b"KEEP").unwrap();
        std::fs::write(ws.path().join(".env"), b"SECRET=1").unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;
        let id = service.create_session("g", &serde_json::json!({})).await.unwrap();

        let bundle = service.export_promotion(id).await.unwrap();
        assert_eq!(bundle.session.id, id);
        let paths: Vec<_> = bundle.workspace.files.iter().map(|(p, _)| p.clone()).collect();
        assert!(paths.contains(&std::path::PathBuf::from("out.txt")));
        // The sensitive-path floor filtered .env out of the export — it never leaves the receiver.
        assert!(!paths.contains(&std::path::PathBuf::from(".env")));
    }

    #[tokio::test]
    async fn export_promotion_unknown_session_is_error() {
        let ws = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let service = build_test_service(ws.path(), db.path().join("s.db")).await;
        assert!(service.export_promotion(SessionId::new()).await.is_err());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-engine export_promotion`
Expected: FAIL to compile — `no method named export_promotion found`.

- [ ] **Step 3: Add a gate-filtered snapshot helper (DRY with `workspace_rpc`)**

The `WorkspaceRequest::Snapshot` arm (service.rs:375-386) gate-filters the workspace snapshot. Extract that into a private method so `export_promotion` reuses it. Add to `impl EngineService`:

```rust
    /// The workspace snapshot with gate-denied paths removed: any path the read gate denies (the
    /// inviolable sensitive-path floor) is omitted, so secrets never leave this engine. Shared by
    /// the `/workspace` Snapshot RPC and `export_promotion`.
    async fn filtered_workspace_snapshot(
        &self,
    ) -> anyhow::Result<otto_engine_core::types::WorkspaceSnapshot> {
        let snap = self.workspace.snapshot().await?;
        let files = snap
            .files
            .into_iter()
            .filter(|(p, _)| {
                self.tools
                    .check("fs.read", &json!({ "path": p.to_string_lossy() }))
                    == Decision::Allow
            })
            .collect();
        Ok(otto_engine_core::types::WorkspaceSnapshot { files })
    }
```

> Confirm the exact path of `WorkspaceSnapshot` (it is `otto_engine_core::types::WorkspaceSnapshot`, already used by `PromoteBundle` in `remote.rs`). If the `tests`/module already imports it under a shorter alias, use that.

Then simplify the `WorkspaceRequest::Snapshot` arm (service.rs:375-391) to reuse it:

```rust
            WorkspaceRequest::Snapshot => match self.filtered_workspace_snapshot().await {
                Ok(snap) => WorkspaceResponse::Snapshot { files: snap.files },
                Err(e) => WorkspaceResponse::Error {
                    message: e.to_string(),
                },
            },
```

- [ ] **Step 4: Add `export_promotion`**

Add to `impl EngineService` (place it just after `accept_demotion`):

```rust
    /// Build a `PromoteBundle` for `session` so a demoting source can pull it back. The workspace
    /// snapshot is gate-filtered (sensitive paths excluded — secrets never leave this engine).
    /// Errors if the session is unknown.
    pub async fn export_promotion(
        &self,
        session: SessionId,
    ) -> anyhow::Result<crate::remote::PromoteBundle> {
        Ok(crate::remote::PromoteBundle {
            session: self.store.snapshot(session).await?,
            workspace: self.filtered_workspace_snapshot().await?,
        })
    }
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p otto-engine export_promotion`
Expected: PASS (both tests).

Run: `cargo test -p otto-engine workspace_rpc`
Expected: PASS — the Snapshot-arm refactor preserved the filtering behavior (existing snapshot/list filtering tests stay green).

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/service.rs
git commit -m "feat(engine): EngineService::export_promotion builds a gate-filtered bundle"
```

---

### Task 4: `POST /export` route + handler (receiver RPC)

**Files:**
- Modify: `crates/engine/src/serve.rs:124` (register the route) and add `export_handler`
- Test: `crates/engine/tests/vps_promote.rs` (reuses `start_receiver`, `TOKEN`)

- [ ] **Step 1: Write the failing tests**

Add to `crates/engine/tests/vps_promote.rs`. First a helper next to `post_promote` (line 81):

```rust
async fn post_export(base: &str, token: Option<&str>, session: &str) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{base}/export"))
        .json(&serde_json::json!({ "session": session }));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap()
}
```

Then the gating tests:

```rust
#[tokio::test]
async fn export_without_accept_flag_is_forbidden() {
    let (base, _w, _d) = start_receiver(false).await;
    let resp = post_export(&base, Some(TOKEN), &SessionId::new().0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn export_without_bearer_is_unauthorized() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = post_export(&base, None, &SessionId::new().0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn export_unknown_session_is_not_found() {
    let (base, _w, _d) = start_receiver(true).await;
    let resp = post_export(&base, Some(TOKEN), &SessionId::new().0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn export_existing_session_returns_a_bundle() {
    // Promote a session onto the receiver, then export it back out.
    let (base, _w, _d) = start_receiver(true).await;
    let id = SessionId::new();
    let body = sample_bundle(id, vec![("out.txt", b"HI")]);
    assert_eq!(
        post_promote(&base, Some(TOKEN), &body).await.status(),
        reqwest::StatusCode::OK
    );
    let resp = post_export(&base, Some(TOKEN), &id.0.to_string()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let bundle: PromoteBundle = resp.json().await.unwrap();
    assert_eq!(bundle.session.id, id);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-engine --test vps_promote export_`
Expected: FAIL — `/export` returns `404`/`405` (route not registered) so the status assertions fail.

- [ ] **Step 3: Register the route**

In `crates/engine/src/serve.rs`, in `app` (line 121-126), add the `/export` route after `/promote`:

```rust
    AxumRouter::new()
        .route("/ws", get(ws_handler))
        .route("/workspace", post(workspace_handler))
        .route("/promote", post(promote_handler))
        .route("/export", post(export_handler))
        .layer(cors)
        .with_state(state)
```

- [ ] **Step 4: Add `export_handler`**

Add after `promote_handler` (ends at line 212). It mirrors `promote_handler`'s gating, parses `{ session }`, and maps errors to status:

```rust
/// Outbound export RPC (receiver role): returns a session's `PromoteBundle` so a demoting source
/// can pull it back. Same gate as `/promote`: `403` unless `--accept-promotions`, `401` without a
/// valid bearer. The bundle's workspace snapshot is gate-filtered (secrets never leave here).
async fn export_handler(
    headers: HeaderMap,
    State(state): State<Arc<ServeState>>,
    body: axum::body::Bytes,
) -> Response {
    if !state.accept_promotions {
        return (StatusCode::FORBIDDEN, "promotion acceptance disabled").into_response();
    }
    if !authorized(&headers, &state.token) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response();
    }
    #[derive(serde::Deserialize)]
    struct ExportRequest {
        session: String,
    }
    let req: ExportRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad request: {e}")).into_response(),
    };
    let session = match uuid::Uuid::parse_str(&req.session) {
        Ok(u) => SessionId(u),
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad session id: {e}")).into_response(),
    };
    match state.service.export_promotion(session).await {
        Ok(bundle) => axum::Json(bundle).into_response(),
        // Unknown session (snapshot errors when the row is absent) → 404, not a 500.
        Err(_) => (StatusCode::NOT_FOUND, "unknown session").into_response(),
    }
}
```

> `SessionId` and `uuid` are already used in `serve.rs` (`resolve_session` parses a UUID at line 619). Confirm `SessionId` is in scope; if not, it is `otto_protocol::SessionId`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p otto-engine --test vps_promote export_`
Expected: PASS (all four export tests).

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/serve.rs crates/engine/tests/vps_promote.rs
git commit -m "feat(serve): POST /export returns a session bundle (gated by --accept-promotions)"
```

---

### Task 5: `VpsTarget::export` (source pulls the bundle)

**Files:**
- Modify: `crates/engine/src/remote.rs` (add `export` to `impl VpsTarget`)
- Test: covered by the end-to-end round-trip in Task 7 (needs a live receiver; matches how `VpsTarget::provision` is tested in the integration file, not in `remote.rs` unit tests).

- [ ] **Step 1: Add `VpsTarget::export`**

In `crates/engine/src/remote.rs`, add a method to the **inherent** `impl VpsTarget` block (the one with `new` and `http_base`, lines 196-220), after `http_base`:

```rust
    /// Pull a session's `PromoteBundle` back from the receiver (the demote primitive). POSTs the
    /// session id to `/export`; surfaces the receiver's status + body on a non-2xx, symmetric with
    /// how `provision` reports a rejected push.
    pub async fn export(&self, session: SessionId) -> anyhow::Result<PromoteBundle> {
        let url = format!("{}/export", self.http_base());
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "session": session.0.to_string() }))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("export rejected by remote: HTTP {status}: {body}");
        }
        Ok(resp.json().await?)
    }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p otto-engine`
Expected: builds clean. (`SessionId`, `PromoteBundle`, `serde_json` are already imported in `remote.rs`.)

- [ ] **Step 3: Commit**

```bash
git add crates/engine/src/remote.rs
git commit -m "feat(remote): VpsTarget::export pulls a session bundle back from the receiver"
```

---

### Task 6: Wire the vps-demote branch of `handle_handover`

**Files:**
- Modify: `crates/engine/src/serve.rs:544-555` (replace the honest-refusal branch)
- Test: `crates/engine/tests/vps_promote.rs` (replace `handover_vps_demote_is_unsupported`)

- [ ] **Step 1: Replace the now-stale unsupported-demote test**

In `crates/engine/tests/vps_promote.rs`, the test `handover_vps_demote_is_unsupported` (lines 279-311) asserts demote errors. That behavior is changing. Replace the whole test with one asserting a vps demote now **pulls back and succeeds**. It promotes a session source→receiver, then demotes it back:

```rust
#[tokio::test]
async fn handover_vps_demote_pulls_session_back_to_source() {
    use otto_engine_core::traits::WorkspaceRead;
    use otto_workspace::RemoteWorkspace;

    // Receiver accepts promotions; source promotes to it in vps mode.
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");
    let (src_ws, src_w, _sd) = start_source_vps(recv_ws.clone()).await;

    // Connect to the source (creates a session), promote it to the receiver.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let ready = next_json(&mut ws).await.unwrap();
    let session = ready["session"].as_str().unwrap().to_string();

    let promote = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&promote).unwrap().into()))
        .await
        .unwrap();
    loop {
        let frame = next_json(&mut ws).await.expect("a frame");
        if frame["type"] == "promoted" {
            break;
        }
        assert_ne!(frame["type"], "error", "promote must not error: {frame:?}");
    }

    // Advance the session on the RECEIVER: write a file via its /workspace RPC, so demote has
    // newer state to pull back than the source's pre-promote copy.
    let recv_remote_ws = RemoteWorkspace::new(recv_http.clone(), TOKEN);
    recv_remote_ws
        .apply_edit(&otto_engine_core::types::Edit {
            path: std::path::PathBuf::from("remote_only.txt"),
            new_contents: "FROM_RECEIVER".to_string(),
        })
        .await
        .unwrap();

    // Demote: the source pulls the session (and the receiver's workspace) back to local.
    let demote = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap().into()))
        .await
        .unwrap();
    loop {
        let frame = next_json(&mut ws).await.expect("a frame");
        if frame["type"] == "demoted" {
            assert_eq!(frame["endpoint"].as_str().unwrap(), src_ws);
            break;
        }
        assert_ne!(frame["type"], "error", "demote must not error: {frame:?}");
    }

    // The receiver-only file now exists in the SOURCE's on-disk workspace.
    assert_eq!(
        std::fs::read(src_w.path().join("remote_only.txt")).unwrap(),
        b"FROM_RECEIVER"
    );
}
```

> Notes: (1) `start_source_vps` returns `(src_ws, ws_dir, db_dir)`; bind the second element (here `src_w`) instead of `_sw` so the test can read the source's on-disk workspace. (2) The exact `Message::Text(...)` construction (`.into()` on the string) must match how other tests in this file build text frames — copy the existing `ws.send(Message::Text(...))` call shape verbatim (see `handover_vps_promote_points_at_receiver`, line 266) rather than the snippet above if they differ. (3) `Edit` is `otto_engine_core::types::Edit`; confirm the field names (`path`, `new_contents`).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p otto-engine --test vps_promote handover_vps_demote_pulls_session_back_to_source`
Expected: FAIL — the source still replies `error` ("demote-from-remote not supported"), so the loop hits the `assert_ne!(... "error" ...)`.

- [ ] **Step 3: Replace the honest-refusal branch with the pull**

In `crates/engine/src/serve.rs`, replace the vps-demote refusal block (lines 544-555, the `if !to_remote && matches!(cfg.mode, …Vps…)` that sends the "not supported" error) with a pull-and-restore:

```rust
    // Vps demote: pull the session's current bundle back off the receiver and restore it into THIS
    // (source) engine, overwriting our own stale copy. Symmetric inverse of the promote push.
    if !to_remote {
        if let crate::remote::PromoteMode::Vps { endpoint } = &cfg.mode {
            let target = crate::remote::VpsTarget::new(endpoint.clone(), cfg.token.clone());
            let bundle = match target.export(session).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = send_msg(
                        writer,
                        &ServerMessage::Error {
                            message: e.to_string(),
                        },
                    )
                    .await;
                    return;
                }
            };
            if let Err(e) = state.service.accept_demotion(&bundle).await {
                let msg = match e {
                    crate::service::AcceptError::Refused(m) => m,
                    crate::service::AcceptError::Failed(err) => err.to_string(),
                    crate::service::AcceptError::AlreadyExists => {
                        "demote restore conflict".to_string()
                    }
                };
                let _ = send_msg(writer, &ServerMessage::Error { message: msg }).await;
                return;
            }
            // The session is local again; the client reconnects to this serve. We report our own
            // ws base — the URL the client already used to reach us is the reconnect target.
            let _ = send_msg(
                writer,
                &ServerMessage::Demoted {
                    session,
                    endpoint: state.public_ws_base.clone(),
                },
            )
            .await;
            return;
        }
    }
```

> **`endpoint` reported on `Demoted`.** The client already knows the source's URL (it's connected to it), but the protocol's `Demoted { endpoint }` needs a value. Check whether `ServeState` already carries the serve's own public ws base. If a field like `public_ws_base` exists, use it. If **not**, the simplest correct value is the source's configured/bound base — add a `public_ws_base: String` field to `ServeState` and to `app(...)`'s parameters, threaded from the binary's `serve` wiring (the bind address → `ws://host:port`). If threading a new field is heavier than warranted, fall back to echoing the receiver endpoint is WRONG (that points back at R); instead reuse whatever base the existing `Promoted`/loopback path reports. Inspect the loopback `Demoted` path (the `handle.endpoint` at line 612) to mirror exactly how an endpoint string is sourced, and prefer that mechanism over adding a field. Resolve this concretely during implementation; do not leave a placeholder.

The existing loopback-demote path (the `match existing { … }` block that calls `promote(...)`) stays unchanged and is reached only for `PromoteMode::Loopback`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p otto-engine --test vps_promote handover_vps_demote_pulls_session_back_to_source`
Expected: PASS.

Run: `cargo test -p otto-engine --test vps_promote handover_vps_promote_points_at_receiver`
Expected: PASS — promote is untouched.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/serve.rs crates/engine/tests/vps_promote.rs
git commit -m "feat(serve): wire vps DemoteToLocal to pull the session back to the source"
```

---

### Task 7: End-to-end demote round-trip (advanced state)

**Files:**
- Test: `crates/engine/tests/vps_promote.rs` (one comprehensive test, mirroring `vps_promote_resumes_session_and_workspace_on_receiver` at line 313)

- [ ] **Step 1: Write the end-to-end test**

This proves the full inverse: a session is promoted source→receiver, *advanced on the receiver by running a turn there*, then demoted back, and the source's store + workspace hold the receiver's advanced state. Add to `crates/engine/tests/vps_promote.rs`:

```rust
#[tokio::test]
async fn vps_demote_round_trip_brings_advanced_state_back_to_source() {
    use otto_engine_core::traits::WorkspaceRead;

    // --- Receiver, acceptance enabled. ---
    let (recv_http, _rw, _rd) = start_receiver(true).await;
    let recv_ws = recv_http.replace("http://", "ws://");

    // --- Source serve in vps mode pointed at the receiver. ---
    let (src_ws, src_w, _sd) = start_source_vps(recv_ws.clone()).await;

    // Connect to the source, create+promote a session.
    let (mut ws, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!("{src_ws}/ws")))
        .await
        .unwrap();
    let session = next_json(&mut ws).await.unwrap()["session"]
        .as_str()
        .unwrap()
        .to_string();
    let promote = serde_json::json!({ "PromoteToRemote": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&promote).unwrap().into()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "promoted" {
            break;
        }
        assert_ne!(f["type"], "error", "{f:?}");
    }

    // Advance the session ON THE RECEIVER: connect there with the same id and write a file via its
    // /workspace RPC (the receiver is now the live copy).
    let recv_remote_ws = otto_workspace::RemoteWorkspace::new(recv_http.clone(), TOKEN);
    recv_remote_ws
        .apply_edit(&otto_engine_core::types::Edit {
            path: std::path::PathBuf::from("advanced.txt"),
            new_contents: "ADVANCED_ON_RECEIVER".to_string(),
        })
        .await
        .unwrap();

    // Demote back to the source.
    let demote = serde_json::json!({ "DemoteToLocal": { "session": session } });
    ws.send(Message::Text(serde_json::to_string(&demote).unwrap().into()))
        .await
        .unwrap();
    loop {
        let f = next_json(&mut ws).await.expect("frame");
        if f["type"] == "demoted" {
            break;
        }
        assert_ne!(f["type"], "error", "{f:?}");
    }
    drop(ws);

    // The source's ON-DISK workspace now has the receiver's advanced file.
    assert_eq!(
        std::fs::read(src_w.path().join("advanced.txt")).unwrap(),
        b"ADVANCED_ON_RECEIVER"
    );

    // And the source can reconnect to its now-local session: a fresh /ws connection with the id
    // yields a Ready for the same session (it lives in the source store again).
    let (mut ws2, _) = tokio_tungstenite::connect_async(authed_ws_request(&format!(
        "{src_ws}/ws?session={session}&last_seq=0"
    )))
    .await
    .unwrap();
    let ready2 = next_json(&mut ws2).await.unwrap();
    assert_eq!(ready2["type"], "ready");
    assert_eq!(ready2["session"].as_str().unwrap(), session);
}
```

> If this test substantially overlaps `handover_vps_demote_pulls_session_back_to_source` from Task 6, keep both: Task 6's proves the *handover wiring* (Demoted endpoint), this one proves *advanced state + reconnect*. If they feel redundant during review, fold Task 6's assertions into this one and delete the smaller test — but only after both pass.

- [ ] **Step 2: Run the test**

Run: `cargo test -p otto-engine --test vps_promote vps_demote_round_trip_brings_advanced_state_back_to_source`
Expected: PASS.

- [ ] **Step 3: Run the whole integration file**

Run: `cargo test -p otto-engine --test vps_promote`
Expected: PASS — promote and demote tests all green.

- [ ] **Step 4: Commit**

```bash
git add crates/engine/tests/vps_promote.rs
git commit -m "test(remote): end-to-end vps demote round-trip brings advanced state back"
```

---

### Task 8: Docs + full-suite verification

**Files:**
- Modify: `docs/ARCHITECTURE.md`, `CLAUDE.md`, `docs/superpowers/specs/2026-06-22-vps-demote-from-remote-design.md`

- [ ] **Step 1: Update `docs/ARCHITECTURE.md`**

Find the remote-axis section (search for `UnsupportedTarget`, `vps`, `demote`). Record that vps **demote** is shipped: a client on the source serve issues `DemoteToLocal`; the source pulls the session via a gated `POST /export` on the receiver and restores it locally with `accept_demotion` (overwriting its own copy via `SessionStore::restore_over`). The residual `UnsupportedTarget` boundary is now **only machine provisioning** (SSH / cloud-SDK VM creation).

- [ ] **Step 2: Update `CLAUDE.md`**

In the distribution-axis paragraph, the sentence about demote-from-remote being "the next follow-up" is now stale. Update it: vps promote **and demote** both work against a running receiver; `UnsupportedTarget` marks only machine provisioning. Also extend the `serve.rs`/`service.rs`/`remote.rs` crate-table descriptions to mention `POST /export`, `accept_demotion`/`export_promotion`, `restore_over`, and `VpsTarget::export`.

- [ ] **Step 3: Mark the design spec shipped**

At the top of `docs/superpowers/specs/2026-06-22-vps-demote-from-remote-design.md`, change **Status** to `Shipped 2026-06-22 (plan: docs/superpowers/plans/2026-06-22-vps-demote-from-remote.md)`.

- [ ] **Step 4: Full-suite verification (determinism invariant)**

Run: `cargo test --workspace`
Expected: PASS — both flags remain opt-in; the default offline suite is unchanged.

Run: `cargo fmt --all && cargo clippy --workspace --all-targets`
Expected: clean — no warnings introduced.

- [ ] **Step 5: Commit**

```bash
git add docs/ARCHITECTURE.md CLAUDE.md docs/superpowers/specs/2026-06-22-vps-demote-from-remote-design.md
git commit -m "docs: record vps demote-from-remote shipped; shrink UnsupportedTarget boundary"
```

---

## Spec coverage check

| Spec requirement | Task |
|---|---|
| Client→S demote pulls from R; reconnect to S | 6 |
| `POST /export` on R, bearer-authed, gated by `--accept-promotions` (403/401/404/200) | 4 |
| `export_promotion` builds the bundle; gate-filtered (secrets never leave R) | 3 |
| Sensitive floor enforced on export (R) and restore (S) | 3, 2 |
| `VpsTarget::export` pull client | 5 |
| `accept_demotion` + `SessionStore::restore_over` overwrite S's own copy | 1, 2 |
| `accept_promotion`'s fail-on-conflict `restore` left intact | 1, 2 |
| End-to-end advanced-state round-trip + unit tests | 7 (+ 1, 2, 3, 4) |
| Determinism suite untouched | 8 |
| Copy semantics (R keeps its copy); move-from-remote a non-goal | inherent — demote never mutates R |

**Intentionally not implemented (spec non-goals):** machine provisioning, the `microvm` target, the `remote` crate split, multi-session receivers, move-from-remote (R retains its copy after demote).
