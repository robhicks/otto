# `Workspace::snapshot` must apply the sensitive-path floor

> **Status:** IMPLEMENTED — shipped in [#128](https://github.com/robhicks/otto/pull/128), closing
> [#127](https://github.com/robhicks/otto/issues/127). Review added three things beyond this
> design: `RemoteWorkspace::snapshot` re-applies the floor rather than trusting the peer, the check
> fails closed on a non-UTF-8 path instead of lossy-converting (matching the explicit warning in
> `validate_workspace_edits`), and `LocalWorkspace::restore` — the ingress mirror — applies the
> floor too. Deeper hardening, including making the invariant hold by type rather than per-impl
> convention, is tracked in [#129](https://github.com/robhicks/otto/issues/129).
> **Found by:** the independent security review on [#124](https://github.com/robhicks/otto/pull/124).

`otto_remote::promote` builds its `PromoteBundle` from a raw `workspace.snapshot()`, while the
opposite direction (`EngineService::export_promotion`) filters through the permission gate. The
asymmetry was assumed harmless because the workspace walk skips dotfiles. It is not: the floor
matches on **substrings**, so floor-sensitive files whose names do not begin with `.` survive the
walk and are serialized into a promote request body.

---

## The bug, demonstrated

`crates/workspace/src/lib.rs`'s recursive walk skips `.git`, `target`, `node_modules`, and any
name beginning with `.`. Its comment (`lib.rs:84-87`) claims this "also covers the gate's
sensitive-path floor."

It does not. `SENSITIVE_MARKERS` (`crates/protocol/src/sensitive.rs:12-14`) is
`[".env", ".ssh/", ".ssh", ".git/", ".git", "id_rsa", ".aws/", ".aws"]`, matched as a
case-insensitive **substring** of the whole path. Two families slip through:

- `id_rsa` — a marker with no leading dot at all. A private key at the workspace root is the
  single most likely instance of this bug in the wild.
- anything containing `.env` that is not *named* `.env`: `production.env`, `config/local.env`,
  `secrets.env.bak`.

Measured on `origin/main` (`0d1972f`) by writing those files into a tempdir and calling
`Workspace::snapshot`:

```
SNAPSHOT CONTAINS: ["config/local.env", "id_rsa", "ok.txt", "production.env"]
  config/local.env  is_sensitive=true
  id_rsa            is_sensitive=true
  ok.txt            is_sensitive=false
  production.env    is_sensitive=true
```

`.env` itself was correctly excluded, which is exactly why the gap went unnoticed — the obvious
test case passes.

## Why it matters, and its real limits

`promote()` sends the bundle to the remote. The receiver's `accept_promotion` refuses sensitive
entries fail-closed — but only **after** the bytes have crossed the wire, so the refusal protects
the receiver's disk, not the secret. `CLAUDE.md`'s claim that "the export is gate-filtered so
secrets never leave" is true of `/export` and of the `/workspace` RPC, and false of the promote
push.

**Bounding it honestly:** this requires an operator to have run a promote, and the file must be in
the workspace root subtree. It is not remotely triggerable and no credential is bypassed. It is a
real secret-egress path, not an exploit chain.

---

## Scope

**In:** apply the floor inside `LocalWorkspace::snapshot`, so **every** caller of
`Workspace::snapshot` inherits it; correct the false comment on the walk; regression tests.

**Out:** changing `SENSITIVE_MARKERS` (the floor is not widened — it is *applied* in one more
place); changing `list()`'s behavior; the gate; the sandbox; anything in `remote`.

---

## Goal & Success Criteria

Make it impossible for a floor-sensitive file to leave an engine inside a `Workspace::snapshot`,
regardless of which caller built it.

1. A workspace containing `id_rsa`, `production.env`, and `config/local.env` snapshots to none of
   them, and still contains ordinary files.
2. `cargo test --workspace -- --skip rust_analyzer_integration` green; `cargo clippy --workspace
   --all-targets -- -D warnings` clean; `cargo fmt --all --check` clean.
3. `promote()` is unchanged — it inherits the fix rather than filtering separately, so no future
   `RemoteTarget` can reintroduce the gap by forgetting to filter.
4. The `list()` comment no longer claims the dotfile skip covers the floor.

---

## Design

### Where the filter goes, and why not the alternatives

**Chosen: `LocalWorkspace::snapshot` (`crates/workspace/src/lib.rs:148-156`).**

```rust
async fn snapshot(&self) -> anyhow::Result<WorkspaceSnapshot> {
    let paths = self.list("**").await?;
    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        // The floor, applied at the seam every caller shares. `list`'s dotfile skip is NOT
        // equivalent: the markers match as substrings, so `id_rsa` and `production.env` pass
        // it. A snapshot is the one Workspace operation that reads whole file *contents* for
        // shipment off-machine, so this is where the floor has to be re-asserted.
        if otto_protocol::is_sensitive(&path.to_string_lossy()) {
            continue;
        }
        let bytes = self.read(&path).await?;
        files.push((path, bytes));
    }
    Ok(WorkspaceSnapshot { files })
}
```

Rejected alternatives:

- **Filter in `promote()`** — the reviewer's other suggestion. It fixes the one caller that is
  broken today and leaves the seam unsafe, so the next `RemoteTarget` or bundle-builder
  reintroduces it. `remote` also has no gate, so it would duplicate the floor rather than share it.
- **Leave `RemoteWorkspace::snapshot` as a pure proxy** — rejected during review. It satisfied the
  seam's contract by *delegation*: it trusted the peer to be an up-to-date otto that filters. That
  is the same shape of assumption that caused this bug (one control believed to cover another), so
  `snapshot` now re-applies the floor to what the peer returned, via a shared
  `strip_sensitive_files` helper. Normally a no-op against an otto peer; locally true regardless.

- **Filter in `list()`** — broader than needed and changes what agents see. `fs.list` returns
  paths, not contents, and a subsequent `fs.read` is gate-denied, so listing is a much weaker
  disclosure. Changing it would perturb the ContextFinder for no security gain here. Out of scope.
- **Drop the engine's `filtered_workspace_snapshot`** now that the seam is safe — no. It filters
  through `tools.check("fs.read", …)`, which is **strictly broader** than the floor: it also
  honours `PolicyGate` deny/ask rules from a workspace's `permissions` block. The two compose;
  the floor is the inviolable minimum, the gate adds policy.

### Dependency check

`crates/workspace` already depends on `otto-protocol` (`Cargo.toml`), and `is_sensitive` is
already public and re-exported from `engine-core`. No new dependency, no direction change. This is
the same pattern `retrieval` uses — it mirrors the floor precisely because it reads files directly
and bypasses the gated `fs.read`.

### Consequence for `restore`

`LocalWorkspace::restore` writes a snapshot back through the gated `apply_edit`, and has path
containment only — it never had a floor check of its own. The fail-closed refusal for a bundle
carrying a sensitive entry lives in `EngineService::accept_promotion`, and is unchanged: it still
guards a bundle arriving from a peer that has not been upgraded. (An earlier draft of this section
attributed that check to `restore`; the behavior described was right, the location was not.)

---

## Error Handling & Edge Cases

| Case | Behavior |
|---|---|
| `id_rsa` / `production.env` / `config/local.env` in the workspace | Silently omitted from the snapshot. Not an error: a snapshot is a best-effort capture, and failing the whole promote because a workspace contains a key would be worse than omitting it. |
| `.env`, `.ssh/`, `.git/` | Already excluded by the walk; now excluded twice. |
| A promoted session whose workspace *needed* an omitted file | The remote gets a workspace without it, exactly as `/export` already behaves. Consistency between the two directions is the point. |
| A `PromoteBundle` from an un-upgraded peer carrying a sensitive entry | Unchanged — `accept_promotion`'s existing fail-closed refusal still catches it. |

---

## Testing

- **`crates/workspace`** — the measured leak above becomes a regression test: write `id_rsa`,
  `production.env`, `config/local.env`, `.env`, and an ordinary file; assert the snapshot contains
  the ordinary file and **no path for which `is_sensitive` is true**. Asserting against
  `is_sensitive` rather than a hardcoded list means the test tracks the floor if a marker is ever
  added.
- **`crates/remote`** — assert `promote()`'s bundle inherits the filtering, so the guarantee is
  pinned at the layer that actually ships bytes off-machine, not only at the workspace.
- Both must fail if the filter is removed. Verify by mutation, not by inspection.

---

## Risks

1. **A caller may depend on snapshots being complete.** `promote`/`restore` round-trips are the
   only consumers; both already tolerate a gate-filtered snapshot from the `/export` direction, so
   asymmetry was the anomaly, not completeness.
2. **The floor is a substring match, so it is blunt.** A legitimately-named file containing
   `id_rsa` as a substring is omitted. That is the existing floor's trade-off everywhere else in
   the codebase; this change inherits it rather than introducing it.
3. **Symlink escapes remain out of scope** — `sensitive.rs`'s own note says they are handled at
   the sandbox layer, not by the string floor. Unchanged here.
