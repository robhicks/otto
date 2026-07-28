# Dioxus UI migration, Phase 2 — build & serve story

> **Status:** IMPLEMENTED — Phase 2 is COMPLETE (see the migration plan).
> **Implements:** Phase 2 of `docs/superpowers/plans/2026-07-22-ui-dioxus-migration.md`.
> **Depends on:** Phase 1 (COMPLETE, PRs #96–#101). **Blocks:** Phase 3 parity sign-off.

Phase 2 replaces `trunk`/`ui/dist` + Tauri's `externalBin` bundling with the `dx` equivalents, and
decides what the shipped artifacts actually are. `ui/` and `desktop/` stay shipped and untouched
until Phase 4.

---

## Premise corrections

The Phase 2 bullets in the migration plan were written from the spike, and three of their premises
do not survive contact with the repository. They are corrected here, and the migration plan should
be updated to match when this lands.

1. **"How `otto serve` finds and serves the Dioxus web bundle (today it serves `ui/dist`)"** —
   `otto serve` does not serve static files at all. Its routes are exactly `/ws`, `/workspace`,
   `/promote`, `/export` (`crates/engine/src/serve.rs:166-170`), and `tower-http` is depended on
   with only the `cors` feature (`crates/engine/Cargo.toml:39`). The browser UI is reached through
   `trunk serve` in development. The only consumer of `ui/dist` anywhere in the repo is Tauri's
   `build.frontendDist` (`desktop/src-tauri/tauri.conf.json`). So the question is not whether an
   existing static-serve path changes — it is whether `otto serve` grows one. **It does; see §1.**

2. **"CI: replace the `ui/` wasm-build + `desktop/` Tauri-build jobs"** — there is no CI in this
   repository. No `.github/`, no GitLab configuration, no `justfile`, no `Makefile`. There are no
   jobs to replace. **Phase 2 ships checked-in build scripts instead; standing up CI is a separate
   project (§3).**

3. **Desktop packaging parity is really about the sidecar binary.** Tauri stages the `otto` binary
   into the bundle via `externalBin` + `desktop/build-sidecar.sh`. The Dioxus desktop build
   currently requires `otto` on `PATH` or `OTTO_BIN` set (`ui-dioxus/src/desktop_boot.rs:70`);
   nothing stages a binary. A fresh install of a `dx`-built app would not work standalone.
   **Closed by §2.**

---

## Scope

**In:** an additive static-file route on `otto serve`; `dx bundle` desktop packaging with a staged
sidecar; three build scripts; the Fly image serving its own UI.

**Out:** CI (deliberately, per §3); embedding the bundle in the binary (§1 rejects it for now, and
it stays available as a later additive feature); any change to `ui/` or `desktop/`, which keep
building and shipping until Phase 4; any protocol, agent, or orchestrator change.

The only workspace-crate edits are `crates/engine/src/serve.rs`, `crates/engine/src/main.rs`, and
one `tower-http` feature in `crates/engine/Cargo.toml`. The offline determinism suite is
unaffected, and both UI crates stay workspace-excluded.

---

## 1. Engine — `otto serve --ui-dir <path>`

### Shape

A static-file fallback applied **after** router construction, not threaded through it:

```rust
// crates/engine/src/serve.rs
pub fn with_ui_dir(app: AxumRouter, dir: PathBuf) -> AxumRouter {
    app.fallback_service(
        ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html"))),
    )
}
```

`serve.rs` already carries two constructors that differ by a single optional argument — `app`
(`serve.rs:100`) and `app_with_base` (`serve.rs:119`), both delegating to `app_inner`. Threading a
`ui_dir` parameter through them would produce four constructor combinations and churn across all
six call sites (`loopback.rs:84`, `main.rs:704`, `tests/vps_promote.rs:380`, `tests/microvm.rs:301`,
and the two constructors themselves). A post-construction layer changes **no existing signature and
no existing call site**; `main.rs` applies it only when the flag is set.

Neither `ui/` nor `ui-dioxus/` uses a client-side router, so the `index.html` fallback exists only
to serve a bare `/`, not to support deep links.

### Configuration

- `--ui-dir <path>` on `otto serve`. **A flag only — no environment variable is read by the CLI.**
  Deployments that want to configure it through the environment do what `deploy/fly/Dockerfile`
  already does for the port and root: set a shell variable and pass it as the flag in `CMD` (§4).
  One way in, and no flag-versus-env precedence question to answer.
- **No default.** When unset, `with_ui_dir` is never applied and the route does not exist.

### Dependency

`tower-http` gains the `fs` feature alongside `cors`. No new crate.

### Security constraints

Both are load-bearing and must be stated in the code, not just here.

1. **The static route is deliberately unauthenticated.** A browser must fetch `index.html` and the
   wasm before it possesses a token to present. This is correct: the bundle is public build output.
   Every path that touches session data or the workspace — `/ws`, `/workspace`, `/promote`,
   `/export` — keeps its existing bearer check, unchanged. The code comment must say so, so that a
   later reader does not "fix" it by adding auth and break first load.

2. **`--ui-dir` must never default to `--root` and must never be inferred.** `ServeDir` does not
   consult the permission gate's sensitive-path floor. Pointed at a workspace it would serve
   `.env`, `.ssh/`, and `.git/` over plain HTTP, bypassing the single most important invariant in
   the codebase. It is operator-supplied and explicit, or it is absent.

### Testing

An integration test in `crates/engine/tests/` that:

- builds an app with `with_ui_dir` over a tempdir and asserts `GET /` returns the index **without**
  a token;
- asserts an unknown path falls back to `index.html`;
- asserts that with **no** `--ui-dir`, `GET /` is a 404 and the four existing routes behave exactly
  as before (the regression guard — this route must be inert when unconfigured).

---

## 2. Desktop — `dx bundle` with a staged sidecar

### Bundle configuration

A `[bundle]` block in `ui-dioxus/Dioxus.toml`, mapping across from `tauri.conf.json`:

```toml
[bundle]
identifier = "dev.otto.desktop"      # same identifier Tauri uses — an upgrade path, not a 2nd install
publisher  = "otto"
icon       = ["icons/32x32.png", "icons/128x128.png", "icons/128x128@2x.png",
              "icons/icon.icns", "icons/icon.ico"]
external_bin = ["binaries/otto-sidecar"]   # staged name, NOT "otto" — see below
```

`external_bin` is a real Dioxus 0.7 `BundleConfig` field, and dx *stages* it under the same
target-triple-suffix convention Tauri's `externalBin` uses — but that parity is only half the
story: **dx strips the triple suffix at install time, where Tauri kept it** (measured — see
below). The entry is named `otto-sidecar`, not `otto`, precisely because of that: an
`otto`-named entry would install as bare `/usr/bin/otto` and collide with this project's own
`otto` CLI on the user's system. So this is not a field-for-field port of Tauri's
`externalBin` convention; it diverges exactly at the one place that matters for naming.

**Sequencing catch:** the icons currently exist only under `desktop/src-tauri/icons/`, which
**Phase 4 deletes**. Phase 2 copies them to `ui-dioxus/icons/` as an explicit step. Skipping it
leaves a bundle that breaks in Phase 4, after parity has already been signed off.

**`[application] name` stays `otto-ui-dioxus` for now.** `scripts/build-web.sh`
hardcodes that name in its asset path, and that script is the only guard against silently
re-shipping the unoptimized wasm. Rename to `otto-desktop` in Phase 4, together with the one-line
script update, when nothing else is in flight.

### Sidecar staging

`ui-dioxus/scripts/stage-sidecar.sh`, closely following `desktop/build-sidecar.sh`: build
`-p otto-engine --release` against the root manifest, then copy `target/release/otto` to
`ui-dioxus/binaries/otto-sidecar-<host-triple>` (with a `.exe` suffix on Windows triples). The
staged name carries `-sidecar` for the collision reason above. `binaries/` is added to
`.gitignore`.

### Runtime resolution — the one `ui-dioxus` code change

`desktop_boot.rs:70` resolves `OTTO_BIN`, else bare `"otto"` on `PATH`. Inside an installed bundle
neither holds. The new order is:

1. `OTTO_BIN` (explicit override always wins),
2. a path resolved relative to the running executable,
3. bare `otto` on `PATH` (preserves today's dev-run behavior).

**Measured, not assumed.** The Dioxus documentation only describes `external_bin` placement for
macOS `.app` bundles; the Linux `.deb` layout was unverified until built and inspected (`ar x` +
`tar tzvf` on the package — `dpkg-deb` was not available on the build host, and produces
equivalent output). Measured layout, with `external_bin = ["binaries/otto-sidecar"]`:

```
usr/bin/otto-ui-dioxus   # app executable
usr/bin/otto-sidecar     # staged sidecar — sibling of the executable, triple suffix stripped
```

The sidecar **is** a sibling of the app executable (both directly under `/usr/bin/`), so step 2's
executable-relative resolution holds. The triple suffix does **not** survive into the installed
name — `stage-sidecar.sh` writes `otto-sidecar-<triple>`, but dx installs it as plain
`otto-sidecar`, matching the un-suffixed `external_bin` entry. Runtime resolution must look for a
bare `otto-sidecar` (or `otto-sidecar.exe`) beside the executable, never a triple-suffixed name.

### Testing

Unit tests for the resolver in the style of the existing `serve_command` tests — env override wins;
the executable-relative path is found when present; falls through to bare `otto` otherwise — driven
by a tempdir rather than a real bundle. The bundle layout itself is verified once by hand and the
finding recorded here.

---

## 3. Build scripts

Three scripts in `ui-dioxus/scripts/`:

| Script | Responsibility |
|---|---|
| `build-web.sh` | Release web build **plus the trust guards**; prints the output directory for `--ui-dir` |
| `stage-sidecar.sh` | Builds release `otto`, stages it as `binaries/otto-sidecar-<triple>` (§2) |
| `build-desktop.sh` | Runs `stage-sidecar.sh`, then `dx bundle --release --platform desktop --features desktop --package-types deb` (rpm was dropped — see the script's own comment for the dx 0.7.9 icon-collision bug) |

### Targeted improvement to `measure-web-bundle.sh`

`measure-web-bundle.sh` is already a release build plus four guards that refuse to report a figure
from an untrustworthy bundle: it wipes `target/dx` first, fails if `dx` logged a `wasm-opt` failure,
fails if the emitted wasm still carries DWARF, and fails above a size ceiling. Those guards are
precisely what a release build script needs.

Copying them into `build-web.sh` would create two copies that drift — and their not drifting is the
only thing standing between the project and silently re-shipping the 2.16 MB unoptimized wasm that
Phase 1 item 1 fixed. So:

- `build-web.sh` becomes the build-and-verify script, owning the build and all four guards;
- `measure-web-bundle.sh` becomes a thin wrapper that invokes `build-web.sh` and reports sizes.

Same guarantees, one copy of the logic, and `measure-web-bundle.sh` keeps its name and its
documented role as the sanctioned way to produce a bundle-size figure.

### Why scripts and not CI

There is no CI to extend (see Premise correction 2), so this bullet is really "create CI" — runner
setup, `dx` installation, the wasm target, system dependencies for the desktop build, caching. That
is its own project, and folding it into Phase 2 would roughly double it while delaying Phase 3.
Checked-in scripts follow the precedent already set by `measure-web-bundle.sh` and
`desktop/build-sidecar.sh`, and are what a CI job would call anyway.

---

## 4. Fly image serves its own UI

`deploy/fly/Dockerfile` is self-contained (`COPY . .`, then build). The bundle is therefore built
**in** the image: a host-built `ui-dioxus/target/dx/...` is gitignored, so copying it from the host
would yield an image that ships a stale bundle or fails on a clean checkout.

### Changes

1. **Web-bundle build stage** — add the `wasm32-unknown-unknown` target, install
   `dioxus-cli --version 0.7.9` (pinned to the `dx` this migration was verified against), then run
   `cd ui-dioxus && ./scripts/build-web.sh`. The §3 trust guards run here too, so a Fly image cannot
   ship unoptimized wasm.
2. **Runtime stage** — `COPY --from=build` the bundle to `/usr/local/share/otto/ui`, add
   `ENV OTTO_UI_DIR=/usr/local/share/otto/ui`, and add `--ui-dir "$OTTO_UI_DIR"` to the `CMD`.
   This mirrors exactly how the existing `CMD` already handles `OTTO_PORT` and `OTTO_ROOT` — an
   `ENV` that the `CMD` passes through as a flag — which is why §1 needs no CLI-level env support.
3. **`deploy/fly/README.md`** — document the browser-reachable URL and the token requirement.

**Cost:** `cargo install dioxus-cli` adds several minutes to an image build. It is its own layer, so
it caches, and the image is built rarely and by hand.

### Exposure

The Fly app already exposes `/ws` and `/workspace` publicly, guarded by the per-session bearer token
minted by `FlyTarget`. The static route adds only compiled build output — no session data, no
workspace contents, nothing token-derived — **but only for a validated, non-empty `--ui-dir`**.
An empty or nonexistent value is now a hard, fail-closed error at startup (`validate_ui_dir` in
`crates/engine/src/main.rs`), closing the gap where an empty value would have made `ServeDir`
resolve against the process's working directory instead. With that validation in place, an
unauthenticated visitor can load the UI and then do nothing with it.

The invariant that keeps this true is the same one as §1: `OTTO_UI_DIR` points at the image's bundle
directory and never at `/workspace`. That is why it is an explicit `ENV` in the Dockerfile rather
than anything inferred at runtime — and why `validate_ui_dir` now enforces, at startup, that the
value is non-empty, exists, and is a directory before it is ever handed to `ServeDir`.

### Verification

This cannot be proven by unit test. The check is a real `fly deploy` plus a browser load against a
promoted session, recorded the way the earlier Fly round-trip was. It is the one Phase 2 item gated
on external infrastructure, so it lands last.

---

## Consequences for later phases

- **Phase 3** re-runs the frozen 11-step scenario contract on both targets. It can now do so against
  `otto serve --ui-dir` for web and an installed `.deb` for desktop, rather than against dev servers
  — closer to what users actually get.
- **Phase 4** deletes `ui/` and `desktop/`. Two items in this spec are its prerequisites: the icons
  must already live in `ui-dioxus/icons/` (§2), and the `[application] name` rename plus the
  `build-web.sh` path update happen there, not here.
- The migration plan's Phase 2 bullets should be rewritten against the three premise corrections
  above when this lands.
