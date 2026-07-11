# FlyTarget Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add on-demand remote execution on Fly.io — promoting a session provisions a fresh Fly Machine running `otto serve`, the client reconnects to it, and the machine is destroyed on demote/stop.

**Architecture:** A new always-compiled `crates/remote/src/fly.rs` with a thin `FlyApi` reqwest client (create app → allocate shared IP → create machine → poll → delete) and a `FlyTarget` that implements the existing `RemoteTarget` seam by mirroring `VpsTarget` (explicit async teardown, no drop-magic — correct for a long-lived billed machine). One Fly app per session (`wss://<app>.fly.dev`), a freshly-minted per-session bearer token injected via the machine's `env`, Fly-native idle backstop (`autostop=suspend` + `auto_destroy`). Wired into `otto serve` behind a new `--promote-fly` flag. A companion container image under `deploy/fly/` makes it deployable.

**Tech Stack:** Rust (edition 2024), `reqwest` (rustls, already a dep), `uuid` (already a workspace dep — token/app-name generation), `serde_json`, `wiremock` (new dev-dep for `otto-remote`), Fly Machines REST API + Fly GraphQL API, Docker.

## Global Constraints

- **Dependencies flow strictly inward.** `otto-remote` may depend only on `otto-protocol`, `otto-engine-core`, `otto-persistence`, and leaf utility crates. `fly.rs` uses only `uuid` (workspace), `reqwest`, `serde_json`, `anyhow`, `async-trait`, `tokio` — no new external crate beyond `uuid` (already workspace) and `wiremock` (dev-dep).
- **Always-compiled, no cargo feature.** Unlike `FirecrackerProvisioner`, `FlyTarget` has no host-specific code; do NOT put it behind a feature.
- **Determinism suite untouched.** `fly.rs` performs I/O only when its methods are called. Its tests use `wiremock` against `localhost` — no network, no API keys. `cargo build --workspace` and the default offline test suite must stay green with no env vars set.
- **Bearer auth + error convention.** Every Fly API call sends `Authorization: Bearer <api_token>`. Every non-2xx bails with exactly `HTTP {status}: {body}` (matches `push_promote_bundle`/`export_bundle` in `lib.rs`).
- **Fly Machines schema (verified):** `auto_destroy` is a machine-config **top-level** field; `autostop`/`autostart`/`min_machines_running` are **per-service** fields. IP allocation is **GraphQL-only** (`allocateIpAddress`, `type: shared_v4`).
- **Token model:** fresh per session, injected via machine `env` (`OTTO_TOKEN`). Never reuse the source token.
- **Reachability:** one app per session; endpoint is `wss://<app>.fly.dev` (no routing header — works for the browser UI).
- **No self-attribution in commits.** No `Co-Authored-By`, no "Generated with Claude", no emoji, no analogous credit — in commits, code comments, or docs.
- **Testability seam:** `FlyApi` holds injectable `machines_base`, `graphql_base`, and `public_base_override` so wiremock can mock every call, including readiness polling and the `/promote` POST.

---

### Task 1: Module scaffold, deps, and pure identity helpers

**Files:**
- Modify: `crates/remote/Cargo.toml`
- Modify: `crates/remote/src/lib.rs`
- Create + Test: `crates/remote/src/fly.rs`

**Interfaces:**
- Produces:
  - `pub struct FlyConfig { pub api_token: String, pub org_slug: String, pub region: String, pub image: String, pub vm_cpus: u32, pub vm_mem_mib: u32, pub app_prefix: String, pub internal_port: u16, pub boot_timeout: std::time::Duration, pub api_base: String, pub graphql_base: String, pub public_base_override: Option<String> }` (`#[derive(Clone)]`)
  - `pub(crate) fn mint_token() -> String`
  - `pub(crate) fn gen_app_name(prefix: &str) -> String`
  - `pub(crate) fn app_name_from_endpoint(endpoint: &str) -> Option<String>`

- [ ] **Step 1: Add deps to `crates/remote/Cargo.toml`**

Under `[dependencies]` add:
```toml
uuid = { workspace = true }
```
Under `[dev-dependencies]` add:
```toml
wiremock = "0.6"
```

- [ ] **Step 2: Declare the module and re-export `FlyConfig` in `crates/remote/src/lib.rs`**

Near the top of `lib.rs`, after the `#[cfg(feature = "firecracker")]` block (around line 11), add:
```rust
mod fly;
pub use fly::{FlyConfig, FlyTarget};
```
(`FlyTarget` is defined in Task 4; declaring the re-export now will not compile until then, so for Task 1 use `pub use fly::FlyConfig;` and widen it to include `FlyTarget` in Task 4.)

- [ ] **Step 3: Write the failing tests in `crates/remote/src/fly.rs`**

Create `crates/remote/src/fly.rs` with the test module first:
```rust
//! `FlyTarget` — provisions a fresh Fly Machine per session (one Fly app each), runs `otto serve`
//! on it, and disposes it on demote/stop. A `RemoteTarget` shaped like `VpsTarget` (explicit async
//! teardown, no drop-magic) because a billed remote machine must outlive the promote RPC. Always
//! compiled: it only does HTTP, so the whole flow is wiremock-tested in CI.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_name_from_endpoint_extracts_app() {
        assert_eq!(
            app_name_from_endpoint("wss://otto-session-abc123.fly.dev"),
            Some("otto-session-abc123".to_string())
        );
    }

    #[test]
    fn app_name_from_endpoint_rejects_malformed() {
        assert_eq!(app_name_from_endpoint("wss://.fly.dev"), None);
        assert_eq!(app_name_from_endpoint("wss://x.example.com"), None);
        assert_eq!(app_name_from_endpoint("http://otto-session-x.fly.dev"), None);
        assert_eq!(app_name_from_endpoint("otto-session-x"), None);
    }

    #[test]
    fn gen_app_name_is_prefixed_and_dns_safe() {
        let name = gen_app_name("otto-session");
        assert!(name.starts_with("otto-session-"), "{name}");
        assert!(
            name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name}"
        );
        assert_ne!(gen_app_name("otto-session"), gen_app_name("otto-session"));
    }

    #[test]
    fn mint_token_is_unique_hex() {
        let t = mint_token();
        assert_eq!(t.len(), 32, "{t}");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()), "{t}");
        assert_ne!(mint_token(), mint_token());
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p otto-remote fly::tests -- --nocapture`
Expected: FAIL to compile — `mint_token`, `gen_app_name`, `app_name_from_endpoint`, `FlyConfig` not defined.

- [ ] **Step 5: Implement the config struct and helpers in `crates/remote/src/fly.rs`**

Above the `#[cfg(test)]` module add:
```rust
use uuid::Uuid;

/// Fly provisioning parameters, read from `OTTO_FLY_*` / `FLY_API_TOKEN` at the CLI edge (never in
/// this crate) and carried as plain data in `PromoteMode::Fly`.
#[derive(Clone)]
pub struct FlyConfig {
    pub api_token: String,
    pub org_slug: String,
    pub region: String,
    pub image: String,
    pub vm_cpus: u32,
    pub vm_mem_mib: u32,
    pub app_prefix: String,
    pub internal_port: u16,
    pub boot_timeout: std::time::Duration,
    /// Machines REST base. Default `https://api.machines.dev/v1`; overridable for wiremock.
    pub api_base: String,
    /// GraphQL base (IP allocation). Default `https://api.fly.io/graphql`; overridable for wiremock.
    pub graphql_base: String,
    /// Test/advanced only: overrides `wss://<app>.fly.dev` so readiness polling and the `/promote`
    /// POST target a mock server. `None` in production.
    pub public_base_override: Option<String>,
}

/// A fresh 32-hex per-session bearer token. Blast radius of a leak is one ephemeral session.
pub(crate) fn mint_token() -> String {
    Uuid::new_v4().simple().to_string()
}

/// A globally-unique, DNS-safe Fly app name: `{prefix}-{12 hex}`.
pub(crate) fn gen_app_name(prefix: &str) -> String {
    let suffix: String = Uuid::new_v4().simple().to_string().chars().take(12).collect();
    format!("{prefix}-{suffix}")
}

/// Extract `<app>` from a `wss://<app>.fly.dev` endpoint (the inverse of the endpoint we hand out).
/// Returns `None` for any other shape so `teardown` fails loudly rather than deleting the wrong app.
pub(crate) fn app_name_from_endpoint(endpoint: &str) -> Option<String> {
    let host = endpoint.strip_prefix("wss://")?;
    let app = host.strip_suffix(".fly.dev")?;
    if app.is_empty() || app.contains('/') || app.contains('.') {
        return None;
    }
    Some(app.to_string())
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p otto-remote fly::tests -- --nocapture`
Expected: PASS (4 tests).

- [ ] **Step 7: Commit**

```bash
git add crates/remote/Cargo.toml crates/remote/src/lib.rs crates/remote/src/fly.rs
git commit -m "feat(remote): scaffold fly module — FlyConfig + identity helpers"
```

---

### Task 2: `FlyApi` struct and pure request builders

**Files:**
- Modify + Test: `crates/remote/src/fly.rs`

**Interfaces:**
- Consumes: `FlyConfig` (Task 1).
- Produces:
  - `pub(crate) struct FlyApi { machines_base: String, graphql_base: String, public_base_override: Option<String>, api_token: String, http: reqwest::Client }`
  - `impl FlyApi { pub(crate) fn from_config(cfg: &FlyConfig) -> Self }`
  - `pub(crate) fn create_machine_body(cfg: &FlyConfig, token: &str) -> serde_json::Value`
  - `impl FlyApi { fn session_endpoint(&self, app: &str) -> String }` (returns `public_base_override` if set, else `wss://{app}.fly.dev`)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` module in `fly.rs`:
```rust
    fn sample_cfg() -> FlyConfig {
        FlyConfig {
            api_token: "fly-tok".into(),
            org_slug: "personal".into(),
            region: "iad".into(),
            image: "registry.fly.io/otto-serve:latest".into(),
            vm_cpus: 1,
            vm_mem_mib: 1024,
            app_prefix: "otto-session".into(),
            internal_port: 8787,
            boot_timeout: std::time::Duration::from_secs(30),
            api_base: "https://api.machines.dev/v1".into(),
            graphql_base: "https://api.fly.io/graphql".into(),
            public_base_override: None,
        }
    }

    #[test]
    fn create_machine_body_has_image_env_guest_and_services() {
        let body = create_machine_body(&sample_cfg(), "sess-tok");
        assert_eq!(body["config"]["image"], "registry.fly.io/otto-serve:latest");
        assert_eq!(body["config"]["auto_destroy"], true); // machine-level
        assert_eq!(body["config"]["env"]["OTTO_TOKEN"], "sess-tok");
        assert_eq!(body["config"]["env"]["OTTO_PORT"], "8787");
        assert_eq!(body["config"]["env"]["OTTO_ROOT"], "/workspace");
        assert_eq!(body["config"]["guest"]["cpus"], 1);
        assert_eq!(body["config"]["guest"]["memory_mb"], 1024);
        let svc = &body["config"]["services"][0];
        assert_eq!(svc["internal_port"], 8787);
        assert_eq!(svc["autostop"], "suspend"); // service-level
        assert_eq!(svc["autostart"], true);
        assert_eq!(svc["min_machines_running"], 0);
        assert_eq!(svc["ports"][0]["port"], 443);
        assert_eq!(svc["ports"][0]["handlers"][0], "tls");
        assert_eq!(svc["ports"][0]["handlers"][1], "http");
        assert_eq!(body["region"], "iad");
    }

    #[test]
    fn session_endpoint_uses_fly_dev_or_override() {
        let api = FlyApi::from_config(&sample_cfg());
        assert_eq!(api.session_endpoint("otto-session-x"), "wss://otto-session-x.fly.dev");
        let mut cfg = sample_cfg();
        cfg.public_base_override = Some("ws://127.0.0.1:9999".into());
        let api = FlyApi::from_config(&cfg);
        assert_eq!(api.session_endpoint("ignored"), "ws://127.0.0.1:9999");
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p otto-remote fly::tests -- --nocapture`
Expected: FAIL — `FlyApi`, `create_machine_body`, `session_endpoint` not defined.

- [ ] **Step 3: Implement `FlyApi` and the body builder**

Add to `fly.rs` (above the test module):
```rust
use async_trait::async_trait;

/// The only unit that talks to Fly. `machines_base`/`graphql_base`/`public_base_override` are
/// injectable so wiremock can mock every call.
pub(crate) struct FlyApi {
    machines_base: String,
    graphql_base: String,
    public_base_override: Option<String>,
    api_token: String,
    http: reqwest::Client,
}

impl FlyApi {
    pub(crate) fn from_config(cfg: &FlyConfig) -> Self {
        Self {
            machines_base: cfg.api_base.clone(),
            graphql_base: cfg.graphql_base.clone(),
            public_base_override: cfg.public_base_override.clone(),
            api_token: cfg.api_token.clone(),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("build reqwest client"),
        }
    }

    /// The `ws`/`wss` base the client reconnects to. Overridable for tests.
    fn session_endpoint(&self, app: &str) -> String {
        self.public_base_override
            .clone()
            .unwrap_or_else(|| format!("wss://{app}.fly.dev"))
    }
}

/// Build the create-machine request body. `auto_destroy` is machine-level; `autostop`/`autostart`/
/// `min_machines_running` are per-service (verified against the Fly Machines schema).
pub(crate) fn create_machine_body(cfg: &FlyConfig, token: &str) -> serde_json::Value {
    serde_json::json!({
        "region": cfg.region,
        "config": {
            "image": cfg.image,
            "auto_destroy": true,
            "env": {
                "OTTO_TOKEN": token,
                "OTTO_PORT": cfg.internal_port.to_string(),
                "OTTO_ROOT": "/workspace",
            },
            "guest": { "cpus": cfg.vm_cpus, "memory_mb": cfg.vm_mem_mib },
            "services": [{
                "protocol": "tcp",
                "internal_port": cfg.internal_port,
                "autostop": "suspend",
                "autostart": true,
                "min_machines_running": 0,
                "ports": [{ "port": 443, "handlers": ["tls", "http"] }],
            }],
        },
    })
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p otto-remote fly::tests -- --nocapture`
Expected: PASS (6 tests total).

- [ ] **Step 5: Commit**

```bash
git add crates/remote/src/fly.rs
git commit -m "feat(remote): FlyApi struct + create-machine body builder"
```

---

### Task 3: `FlyApi` async methods with wiremock tests

**Files:**
- Modify + Test: `crates/remote/src/fly.rs`

**Interfaces:**
- Consumes: `FlyApi` (Task 2), `create_machine_body` (Task 2).
- Produces (all on `impl FlyApi`, all `async`, all `-> anyhow::Result<()>` unless noted):
  - `async fn create_app(&self, app: &str, org: &str) -> anyhow::Result<()>`
  - `async fn allocate_shared_ip(&self, app: &str) -> anyhow::Result<()>`
  - `async fn create_machine(&self, app: &str, cfg: &FlyConfig, token: &str) -> anyhow::Result<()>`
  - `async fn wait_ready(&self, app: &str, timeout: std::time::Duration) -> anyhow::Result<()>`
  - `async fn delete_app(&self, app: &str) -> anyhow::Result<()>`
  - Helper `async fn bail_on_error(resp: reqwest::Response) -> anyhow::Result<()>`

Note: `wait_ready` polls `http_base(session_endpoint(app)) + "/"`. Reuse the existing `crate::http_base` (make it `pub(crate)` — it is currently a private fn in `lib.rs`; change `fn http_base` to `pub(crate) fn http_base`).

- [ ] **Step 1: Make `http_base` reachable from `fly.rs`**

In `crates/remote/src/lib.rs`, change `fn http_base(endpoint: &str) -> String {` to `pub(crate) fn http_base(endpoint: &str) -> String {`.

- [ ] **Step 2: Write the failing wiremock tests**

Add to the `#[cfg(test)]` module in `fly.rs`:
```rust
    use wiremock::matchers::{method, path, path_regex, header};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg_for(server: &MockServer) -> FlyConfig {
        let mut c = sample_cfg();
        c.api_base = server.uri();
        c.graphql_base = format!("{}/graphql", server.uri());
        c.public_base_override = Some(server.uri().replacen("http", "ws", 1));
        c.boot_timeout = std::time::Duration::from_secs(5);
        c
    }

    #[tokio::test]
    async fn create_app_posts_bearer_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps"))
            .and(header("authorization", "Bearer fly-tok"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        api.create_app("otto-session-x", "personal").await.unwrap();
    }

    #[tokio::test]
    async fn create_app_surfaces_non_2xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps"))
            .respond_with(ResponseTemplate::new(422).set_body_string("name taken"))
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        let err = api.create_app("x", "personal").await.unwrap_err().to_string();
        assert!(err.contains("422") && err.contains("name taken"), "{err}");
    }

    #[tokio::test]
    async fn allocate_shared_ip_posts_graphql() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{}}"#))
            .expect(1)
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        api.allocate_shared_ip("otto-session-x").await.unwrap();
    }

    #[tokio::test]
    async fn create_machine_posts_to_app_machines() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/otto-session-x/machines"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"abc"}"#))
            .expect(1)
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        let api = FlyApi::from_config(&cfg);
        api.create_machine("otto-session-x", &cfg, "sess-tok").await.unwrap();
    }

    #[tokio::test]
    async fn wait_ready_returns_on_any_http_status() {
        let server = MockServer::start().await;
        // Any HTTP response (even 401) means "serve is listening".
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        api.wait_ready("otto-session-x", std::time::Duration::from_secs(5)).await.unwrap();
    }

    #[tokio::test]
    async fn delete_app_issues_delete() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/apps/otto-session-x"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        api.delete_app("otto-session-x").await.unwrap();
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p otto-remote fly::tests -- --nocapture`
Expected: FAIL — the five async methods are not defined.

- [ ] **Step 4: Implement the async methods**

Add to `impl FlyApi` in `fly.rs`:
```rust
    async fn bail_on_error(resp: reqwest::Response) -> anyhow::Result<()> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("HTTP {status}: {body}");
        }
        Ok(())
    }

    async fn create_app(&self, app: &str, org: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(format!("{}/apps", self.machines_base))
            .bearer_auth(&self.api_token)
            .json(&serde_json::json!({ "app_name": app, "org_slug": org }))
            .send()
            .await?;
        Self::bail_on_error(resp).await
    }

    /// Allocate a shared IPv4 (GraphQL-only) so `<app>.fly.dev` resolves. Fly's GraphQL accepts the
    /// app name as `appId` for app-scoped mutations.
    async fn allocate_shared_ip(&self, app: &str) -> anyhow::Result<()> {
        let query = "mutation($input: AllocateIPAddressInput!) { \
                     allocateIpAddress(input: $input) { ipAddress { address } } }";
        let resp = self
            .http
            .post(&self.graphql_base)
            .bearer_auth(&self.api_token)
            .json(&serde_json::json!({
                "query": query,
                "variables": { "input": { "appId": app, "type": "shared_v4" } },
            }))
            .send()
            .await?;
        Self::bail_on_error(resp).await
    }

    async fn create_machine(&self, app: &str, cfg: &FlyConfig, token: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .post(format!("{}/apps/{app}/machines", self.machines_base))
            .bearer_auth(&self.api_token)
            .json(&create_machine_body(cfg, token))
            .send()
            .await?;
        Self::bail_on_error(resp).await
    }

    /// Poll the public endpoint until any HTTP response (every otto route is gated, so 401/404 mean
    /// "serve is up") or the boot timeout elapses.
    async fn wait_ready(&self, app: &str, timeout: std::time::Duration) -> anyhow::Result<()> {
        let url = format!("{}/", crate::http_base(&self.session_endpoint(app)));
        let poll = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?;
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if poll.get(&url).send().await.is_ok() {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!("Fly machine did not become reachable within boot timeout");
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }

    async fn delete_app(&self, app: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(format!("{}/apps/{app}", self.machines_base))
            .bearer_auth(&self.api_token)
            .send()
            .await?;
        Self::bail_on_error(resp).await
    }
```

Note: `wait_ready` and `tokio::time::sleep` need the `tokio` `time` feature. Add `time` to the `tokio` features used by `otto-remote` — in `crates/remote/Cargo.toml` change the `tokio` line to `tokio = { workspace = true, features = ["time"] }` (the workspace default already includes `sync`/`rt`/`macros`; `features` here are additive).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p otto-remote fly::tests -- --nocapture`
Expected: PASS (12 tests total).

- [ ] **Step 6: Commit**

```bash
git add crates/remote/Cargo.toml crates/remote/src/lib.rs crates/remote/src/fly.rs
git commit -m "feat(remote): FlyApi async methods (create/allocate-ip/machine/wait/delete) + wiremock tests"
```

---

### Task 4: `FlyTarget` — `RemoteTarget` impl with end-to-end wiremock tests

**Files:**
- Modify + Test: `crates/remote/src/fly.rs`
- Modify: `crates/remote/src/lib.rs`

**Interfaces:**
- Consumes: `FlyApi` + all its methods (Task 3), `FlyConfig` (Task 1), and from `lib.rs`: `RemoteTarget`, `RemoteHandle`, `PromoteBundle`, `push_promote_bundle`, `promote`.
- Produces:
  - `pub struct FlyTarget { api: FlyApi, cfg: FlyConfig }`
  - `impl FlyTarget { pub fn new(cfg: FlyConfig) -> Self }`
  - `#[async_trait] impl RemoteTarget for FlyTarget` with `provision(&self, &PromoteBundle) -> anyhow::Result<RemoteHandle>` and `teardown(&self, RemoteHandle) -> anyhow::Result<()>`
  - New enum variant `PromoteMode::Fly { config: FlyConfig }` in `lib.rs`.

- [ ] **Step 1: Add the `PromoteMode::Fly` variant in `crates/remote/src/lib.rs`**

In the `PromoteMode` enum (around lines 33–42), add after the `Microvm` variant:
```rust
    /// Provision a fresh Fly Machine (one Fly app per session), restore the bundle into it, and
    /// destroy the app on demote/stop. `config` is read from `OTTO_FLY_*` by the CLI.
    Fly { config: FlyConfig },
```
Also widen the re-export from Task 1 to: `pub use fly::{FlyConfig, FlyTarget};`

- [ ] **Step 2: Write the failing end-to-end tests**

Add to the `#[cfg(test)]` module in `fly.rs`:
```rust
    use otto_persistence::{SessionState, SessionStatus};
    use otto_protocol::SessionId;
    use otto_engine_core::types::WorkspaceSnapshot;

    fn empty_bundle() -> crate::PromoteBundle {
        crate::PromoteBundle {
            session: SessionState {
                id: SessionId::new(),
                goal: "g".into(),
                status: SessionStatus::Active,
                config: serde_json::json!({}),
                events: vec![],
                turns: vec![],
            },
            workspace: WorkspaceSnapshot { files: vec![] },
        }
    }

    async fn mount_happy_path(server: &MockServer) {
        Mock::given(method("POST")).and(path("/apps"))
            .respond_with(ResponseTemplate::new(201)).mount(server).await;
        Mock::given(method("POST")).and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{}}"#)).mount(server).await;
        Mock::given(method("POST")).and(path_regex(r"^/apps/.+/machines$"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"m"}"#)).mount(server).await;
        Mock::given(method("GET")).and(path("/"))
            .respond_with(ResponseTemplate::new(401)).mount(server).await; // readiness
        Mock::given(method("POST")).and(path("/promote"))
            .respond_with(ResponseTemplate::new(200)).mount(server).await;
    }

    #[tokio::test]
    async fn provision_creates_app_machine_and_pushes_bundle() {
        let server = MockServer::start().await;
        mount_happy_path(&server).await;
        let target = FlyTarget::new(cfg_for(&server));
        let handle = target.provision(&empty_bundle()).await.unwrap();
        // In tests the endpoint is the override; the token is freshly minted (32 hex).
        assert!(handle.endpoint.starts_with("ws://"), "{}", handle.endpoint);
        assert_eq!(handle.token.len(), 32);
    }

    #[tokio::test]
    async fn provision_deletes_app_when_a_step_fails() {
        let server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/apps"))
            .respond_with(ResponseTemplate::new(201)).mount(&server).await;
        Mock::given(method("POST")).and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{}}"#)).mount(&server).await;
        // create_machine fails → provision must clean up.
        Mock::given(method("POST")).and(path_regex(r"^/apps/.+/machines$"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom")).mount(&server).await;
        let delete = Mock::given(method("DELETE")).and(path_regex(r"^/apps/.+$"))
            .respond_with(ResponseTemplate::new(200)).expect(1);
        server.register(delete).await;

        let target = FlyTarget::new(cfg_for(&server));
        assert!(target.provision(&empty_bundle()).await.is_err());
        // On drop, MockServer verifies the DELETE .expect(1) was satisfied.
    }

    #[tokio::test]
    async fn teardown_deletes_the_app_parsed_from_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE")).and(path("/apps/otto-session-abc"))
            .respond_with(ResponseTemplate::new(200)).expect(1).mount(&server).await;
        // Endpoint carries the app name; delete goes to the (mock) machines_base.
        let target = FlyTarget::new(cfg_for(&server));
        let handle = crate::RemoteHandle::new("wss://otto-session-abc.fly.dev", "t");
        target.teardown(handle).await.unwrap();
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p otto-remote fly::tests -- --nocapture`
Expected: FAIL — `FlyTarget` not defined.

- [ ] **Step 4: Implement `FlyTarget`**

Add to `fly.rs` (above the test module):
```rust
/// A `RemoteTarget` that provisions a fresh Fly Machine per session and disposes it explicitly.
/// Shaped like `VpsTarget` (returns a task-less `RemoteHandle`) because the machine must outlive
/// the promote RPC — it lives until an explicit `teardown` (demote/stop).
pub struct FlyTarget {
    api: FlyApi,
    cfg: FlyConfig,
}

impl FlyTarget {
    pub fn new(cfg: FlyConfig) -> Self {
        Self { api: FlyApi::from_config(&cfg), cfg }
    }
}

#[async_trait]
impl crate::RemoteTarget for FlyTarget {
    async fn provision(&self, bundle: &crate::PromoteBundle) -> anyhow::Result<crate::RemoteHandle> {
        let token = mint_token();
        let app = gen_app_name(&self.cfg.app_prefix);

        // Everything after create_app must clean up on failure so a half-provisioned app is never
        // left billing. `run` collects the fallible steps; on Err we best-effort delete the app.
        let endpoint = self.api.session_endpoint(&app);
        let run = async {
            self.api.create_app(&app, &self.cfg.org_slug).await?;
            self.api.allocate_shared_ip(&app).await?;
            self.api.create_machine(&app, &self.cfg, &token).await?;
            self.api.wait_ready(&app, self.cfg.boot_timeout).await?;
            crate::push_promote_bundle(&endpoint, &token, bundle).await?;
            Ok::<(), anyhow::Error>(())
        };
        if let Err(e) = run.await {
            let _ = self.api.delete_app(&app).await; // best-effort; original error wins
            return Err(e);
        }
        Ok(crate::RemoteHandle::new(endpoint, token))
    }

    async fn teardown(&self, handle: crate::RemoteHandle) -> anyhow::Result<()> {
        let app = app_name_from_endpoint(&handle.endpoint)
            .ok_or_else(|| anyhow::anyhow!("cannot parse Fly app from endpoint {}", handle.endpoint))?;
        self.api.delete_app(&app).await
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p otto-remote fly:: -- --nocapture`
Expected: PASS (15 tests total).

- [ ] **Step 6: Run the whole remote crate + clippy**

Run: `cargo test -p otto-remote && cargo clippy -p otto-remote --all-targets`
Expected: all green, no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/remote/src/fly.rs crates/remote/src/lib.rs
git commit -m "feat(remote): FlyTarget RemoteTarget impl + PromoteMode::Fly (wiremock end-to-end)"
```

---

### Task 5: CLI wiring — `--promote-fly` and `OTTO_FLY_*`

**Files:**
- Modify: `crates/engine/src/main.rs` (flag parsing ~538–581; token ~584; promote match ~614–641; add a `fly_config_from_env` helper near `microvm_config_from_env` ~112–137; usage strings ~2, ~31)
- Modify: `crates/engine/src/lib.rs` (re-export `FlyConfig`/`FlyTarget` from `otto_remote`, alongside the existing `PromoteMode`/`PromoteConfig`/`VpsTarget` re-exports)

**Interfaces:**
- Consumes: `otto_engine::PromoteMode::Fly { config }`, `otto_engine::FlyConfig` (Task 4).
- Produces: a `--promote-fly` flag and `fly_config_from_env() -> otto_engine::FlyConfig`.

- [ ] **Step 1: Confirm the engine re-exports the remote seam**

Run: `grep -n "pub use otto_remote" crates/engine/src/lib.rs`
Expected: a line re-exporting the remote seam (e.g. `pub use otto_remote::{...};`). Add `FlyConfig, FlyTarget` to that list. If the engine instead re-exports item-by-item, add `pub use otto_remote::{FlyConfig, FlyTarget};`.

- [ ] **Step 2: Add the `fly_config_from_env` helper in `main.rs`**

Immediately after `microvm_config_from_env` (ends ~line 137), add:
```rust
/// Read Fly provisioning parameters from `OTTO_FLY_*` / `FLY_API_TOKEN`. Missing `FLY_API_TOKEN`
/// yields an empty token; provisioning then fails at the first API call with a clear 401 — the CLI
/// need not special-case it here.
fn fly_config_from_env() -> otto_engine::FlyConfig {
    fn num(key: &str, default: u32) -> u32 {
        std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }
    otto_engine::FlyConfig {
        api_token: std::env::var("FLY_API_TOKEN").unwrap_or_default(),
        org_slug: std::env::var("OTTO_FLY_ORG").unwrap_or_else(|_| "personal".to_string()),
        region: std::env::var("OTTO_FLY_REGION").unwrap_or_else(|_| "iad".to_string()),
        image: std::env::var("OTTO_FLY_IMAGE").unwrap_or_default(),
        vm_cpus: num("OTTO_FLY_CPUS", 1),
        vm_mem_mib: num("OTTO_FLY_MEM_MIB", 1024),
        app_prefix: std::env::var("OTTO_FLY_APP_PREFIX").unwrap_or_else(|_| "otto-session".to_string()),
        internal_port: num("OTTO_FLY_PORT", 8787) as u16,
        boot_timeout: std::time::Duration::from_millis(num("OTTO_FLY_BOOT_TIMEOUT_MS", 30_000) as u64),
        api_base: std::env::var("OTTO_FLY_API_BASE").unwrap_or_else(|_| "https://api.machines.dev/v1".to_string()),
        graphql_base: std::env::var("OTTO_FLY_GRAPHQL_BASE").unwrap_or_else(|_| "https://api.fly.io/graphql".to_string()),
        public_base_override: std::env::var("OTTO_FLY_PUBLIC_BASE").ok(),
    }
}
```

- [ ] **Step 3: Add the flag and the mutual-exclusion arm**

In `cmd_serve`, next to `let mut promote_microvm = false;` (line 543) add:
```rust
    let mut promote_fly = false;
```
In the arg-match loop, next to `"--promote-microvm" => promote_microvm = true,` (line 578) add:
```rust
            "--promote-fly" => promote_fly = true,
```
Replace the `let promote = match (promote_loopback, promote_vps, promote_microvm) {` block (lines 614–641) so the tuple and guard include `promote_fly`:
```rust
    let promote = match (promote_loopback, promote_vps.clone(), promote_microvm, promote_fly) {
        (l, v, m, f) if (l as u8) + (v.is_some() as u8) + (m as u8) + (f as u8) > 1 => {
            eprintln!(
                "error: --promote-loopback, --promote-vps, --promote-microvm, and --promote-fly are mutually exclusive"
            );
            std::process::exit(2);
        }
        (true, _, _, _) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Loopback {
                base_dir: root.join(".otto-remotes"),
            },
        }),
        (_, Some(endpoint), _, _) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Vps { endpoint },
        }),
        (_, _, true, _) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Microvm {
                config: microvm_config_from_env(),
            },
        }),
        (_, _, _, true) => Some(otto_engine::PromoteConfig {
            token: token.clone(),
            mode: otto_engine::PromoteMode::Fly {
                config: fly_config_from_env(),
            },
        }),
        (false, None, false, false) => None,
    };
```
(`promote_vps.clone()` because the tuple now also matches the earlier bare `promote_vps`; keeping the original `promote_vps` binding intact avoids a move error.)

- [ ] **Step 4: Update the usage strings**

In the two usage strings (line 2 doc comment and line 31 `eprintln!`), extend the serve line's promote group to read:
`[--promote-loopback | --promote-vps <ws-endpoint> | --promote-microvm | --promote-fly]`

- [ ] **Step 5: Verify it compiles and the flag behaves**

Run: `cargo build -p otto-engine`
Expected: builds clean.

Run: `OTTO_TOKEN=t cargo run -p otto-engine -- serve --promote-fly --promote-vps ws://x 2>&1 | head -1`
Expected: `error: --promote-loopback, --promote-vps, --promote-microvm, and --promote-fly are mutually exclusive`

Run: `cargo clippy -p otto-engine --all-targets`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/engine/src/main.rs crates/engine/src/lib.rs
git commit -m "feat(engine): --promote-fly flag + OTTO_FLY_* config"
```

---

### Task 6: Serve handover wiring — Fly promote + demote arms

**Files:**
- Modify: `crates/engine/src/serve.rs` (disabled-message ~692; demote block ~703–837; target-build match ~851–876)

**Interfaces:**
- Consumes: `otto_remote::PromoteMode::Fly { config }`, `otto_remote::FlyTarget`, `otto_remote::export_bundle`, `otto_remote::RemoteHandle`, `otto_remote::RemoteTarget` (Task 4).
- Produces: served promote/demote support for the `Fly` mode.

- [ ] **Step 1: Add the `Fly` arm to the target-build match**

In the `match &cfg.mode` at lines 852–876, add after the `Microvm` arm (before the closing `}` at 876):
```rust
                    otto_remote::PromoteMode::Fly { config } => {
                        Box::new(otto_remote::FlyTarget::new(config.clone()))
                    }
```

- [ ] **Step 2: Add the `Fly` demote arm**

In `handle_handover`, inside the `if !to_remote {` block, after the `Microvm` demote arm (ends line 836), add:
```rust
        if let otto_remote::PromoteMode::Fly { config } = &cfg.mode {
            // Source the live app endpoint+token from the handle a prior promote stored under
            // (session, true). Clone out under the lock, release before awaiting.
            let live = state
                .remotes
                .lock()
                .unwrap()
                .get(&(session, true))
                .map(|h| (h.endpoint.clone(), h.token.clone()));
            let Some((endpoint, token)) = live else {
                let _ = send_msg(writer, &ServerMessage::Error {
                    message: "no active fly handover for this session; promote first".to_string(),
                }).await;
                return;
            };

            // Pull the current bundle off the Fly machine. On failure, leave it running and report.
            let bundle = match otto_remote::export_bundle(&endpoint, &token, session).await {
                Ok(b) => b,
                Err(e) => {
                    let _ = send_msg(writer, &ServerMessage::Error { message: e.to_string() }).await;
                    return;
                }
            };
            if let Err(e) = state.service.accept_demotion(&bundle).await {
                let msg = match e {
                    crate::service::AcceptError::Refused(m) => m,
                    crate::service::AcceptError::Failed(err) => err.to_string(),
                    crate::service::AcceptError::AlreadyExists => "demote restore conflict".to_string(),
                };
                let _ = send_msg(writer, &ServerMessage::Error { message: msg }).await;
                return;
            }

            // Success: destroy the Fly app (we own it), then drop the handle and tell the client to
            // reconnect to us. teardown deletes the app parsed from the endpoint.
            let target = otto_remote::FlyTarget::new(config.clone());
            if let Err(e) = target
                .teardown(otto_remote::RemoteHandle::new(endpoint, token))
                .await
            {
                // Restore already committed; a failed delete only risks an orphan (idle-suspended,
                // auto_destroy-reaped). Report it but the session is local again.
                let _ = send_msg(writer, &ServerMessage::Error {
                    message: format!("session demoted but fly app cleanup failed: {e}"),
                }).await;
                state.remotes.lock().unwrap().remove(&(session, true));
                return;
            }
            state.remotes.lock().unwrap().remove(&(session, true));
            match &state.public_ws_base {
                Some(base) => {
                    let _ = send_msg(writer, &ServerMessage::Demoted {
                        session, endpoint: base.clone(),
                    }).await;
                }
                None => {
                    let _ = send_msg(writer, &ServerMessage::Error {
                        message: "demote target has no public ws base configured".to_string(),
                    }).await;
                }
            }
            return;
        }
```

- [ ] **Step 3: Update the disabled-provisioning message**

At line 692, extend the message to mention `--promote-fly`:
```rust
                    "remote provisioning unavailable (start otto serve with --promote-loopback, --promote-vps, --promote-microvm, or --promote-fly)"
```

- [ ] **Step 4: Verify build, clippy, and the existing serve tests**

Run: `cargo build -p otto-engine`
Expected: builds clean.

Run: `cargo test -p otto-engine serve`
Expected: existing serve tests still pass (this task adds no new serve test — Fly behavior is covered by the Task 4 wiremock tests; a full serve+Fly path is part of the manual smoke test below, matching how microVM serve wiring has no CI-runnable VM test).

Run: `cargo clippy -p otto-engine --all-targets`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/engine/src/serve.rs
git commit -m "feat(engine): serve handover promote + demote arms for PromoteMode::Fly"
```

---

### Task 7: Container image + Fly deploy artifacts

**Files:**
- Create: `deploy/fly/Dockerfile`
- Create: `deploy/fly/fly.toml`
- Create: `deploy/fly/README.md`

**Interfaces:**
- Consumes: the env contract `create_machine_body` injects — `OTTO_TOKEN`, `OTTO_PORT`, `OTTO_ROOT` (Task 2).
- Produces: a `registry.fly.io/otto-serve` image whose `CMD` runs `otto serve --accept-promotions` reading those env vars.

- [ ] **Step 1: Write `deploy/fly/Dockerfile`**

```dockerfile
# Multi-stage build of the `otto` binary into a slim runtime image for Fly Machines.
FROM rust:1.85-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p otto-engine

FROM debian:bookworm-slim
# git + ripgrep back the served spine's git/grep tools; ca-certificates for outbound TLS to LLM APIs.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git ripgrep ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/otto /usr/local/bin/otto
# The session workspace root. create_machine injects OTTO_ROOT=/workspace.
RUN mkdir -p /workspace
ENV OTTO_PORT=8787 OTTO_ROOT=/workspace
# No OS sandbox backend (bwrap) is installed, so `bash`/sandboxed tools stay unregistered on the
# guest (fail-closed) — a deliberate posture consistent with otto's "no backend -> no bash" rule.
# OTTO_TOKEN is injected per-machine by the provisioner.
CMD ["sh", "-c", "otto serve --accept-promotions --port \"$OTTO_PORT\" --root \"$OTTO_ROOT\""]
```

- [ ] **Step 2: Write `deploy/fly/fly.toml`**

```toml
# Reference base config for the otto-serve image. FlyTarget sets per-machine services via the
# Machines API; this file documents intent and supports a manual `fly deploy` of a base app.
app = "otto-serve"
primary_region = "iad"

[build]
  dockerfile = "Dockerfile"

[[services]]
  internal_port = 8787
  protocol = "tcp"
  auto_stop_machines = "suspend"
  auto_start_machines = true
  min_machines_running = 0

  [[services.ports]]
    port = 443
    handlers = ["tls", "http"]
```

- [ ] **Step 3: Write `deploy/fly/README.md`**

```markdown
# Deploying otto on Fly.io

`FlyTarget` (mode `--promote-fly`) provisions one Fly app + machine per session from an
`otto-serve` image. Steps:

## 1. Build & push the image
```bash
fly auth docker
docker build -t registry.fly.io/otto-serve:latest -f deploy/fly/Dockerfile .
docker push registry.fly.io/otto-serve:latest
```

## 2. Configure the source engine
```bash
export FLY_API_TOKEN=$(fly auth token)      # provisioning credential
export OTTO_FLY_ORG=personal
export OTTO_FLY_REGION=iad
export OTTO_FLY_IMAGE=registry.fly.io/otto-serve:latest
export OTTO_TOKEN=<source bearer>           # required by `otto serve`
```

## 3. Serve with Fly provisioning
```bash
otto serve --promote-fly
```
Promoting a session then creates `otto-session-<id>.fly.dev`, runs `otto serve` on it, and the
client reconnects. Demote/stop destroys the app. Idle machines suspend (`autostop=suspend`) and
`auto_destroy` reaps stopped orphans.

## Env reference
`FLY_API_TOKEN`, `OTTO_FLY_ORG`, `OTTO_FLY_REGION`, `OTTO_FLY_IMAGE`, `OTTO_FLY_CPUS` (1),
`OTTO_FLY_MEM_MIB` (1024), `OTTO_FLY_APP_PREFIX` (otto-session), `OTTO_FLY_PORT` (8787),
`OTTO_FLY_BOOT_TIMEOUT_MS` (30000).

## Cleanup of orphan empty apps (follow-up)
After `auto_destroy` reaps a machine, its (free) app remains. Sweep periodically:
```bash
fly apps list | grep otto-session- | awk '{print $1}' | xargs -n1 fly apps destroy -y
```
```

- [ ] **Step 4: Verify the image builds and starts (requires Docker)**

Run: `docker build -t otto-serve:test -f deploy/fly/Dockerfile .`
Expected: build succeeds.

Run: `docker run --rm -e OTTO_TOKEN=smoketest otto-serve:test otto --help 2>&1 | head -3`
Expected: otto usage text prints (confirms the binary is in place and runs).

If Docker is unavailable in the execution environment, note that and defer this step to the manual smoke test; still verify the Dockerfile parses with `docker build --check -f deploy/fly/Dockerfile .` if available.

- [ ] **Step 5: Commit**

```bash
git add deploy/fly/Dockerfile deploy/fly/fly.toml deploy/fly/README.md
git commit -m "feat(deploy): otto-serve container image + fly.toml + deploy guide"
```

---

### Task 8: Docs update + full-workspace verification

**Files:**
- Modify: `CLAUDE.md` (the `remote` crate table row)
- Modify: `docs/ARCHITECTURE.md` (remote/deployment section)
- Modify: `README.md` (roadmap line noting the external provisioner is now shipped)

**Interfaces:**
- Consumes: everything above.
- Produces: accurate docs; a green full workspace.

- [ ] **Step 1: Update `CLAUDE.md` remote crate row**

In the `remote` row of the crate table, append a sentence:
> `FlyTarget` (mode `--promote-fly`) provisions one Fly app + machine per session over the Fly Machines REST + GraphQL APIs (shared-IP allocation is GraphQL-only), mints a fresh per-session token injected via machine `env`, applies a Fly-native idle backstop (`autostop=suspend` + `auto_destroy`), and disposes the app on demote/stop. Always-compiled (HTTP only), wiremock-tested in CI. The container image + deploy guide live in `deploy/fly/`.

- [ ] **Step 2: Update `docs/ARCHITECTURE.md`**

In the remote-axis / deployment-topologies section, add that on-demand Fly provisioning is shipped (`FlyTarget`), replacing "external-VPS provisioner" as still-ahead where that appears. Keep it to 2–3 sentences consistent with the surrounding prose.

- [ ] **Step 3: Update `README.md` roadmap**

Where the README lists "the external-VPS remote provisioner" as ahead, note that on-demand Fly.io provisioning has shipped (`otto serve --promote-fly`; see `deploy/fly/`).

- [ ] **Step 4: Full workspace verification**

Run: `cargo fmt --all`
Then run each and confirm green:
```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo test -p otto-remote
```
Expected: all pass, no warnings, the offline determinism suite unaffected (no env vars set).

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md docs/ARCHITECTURE.md README.md
git commit -m "docs: record shipped FlyTarget (on-demand Fly.io remote execution)"
```

---

## Self-Review notes (for the implementer)

- **Manual smoke test (out of CI, mirrors Firecracker):** build+push the image, `export FLY_API_TOKEN/OTTO_FLY_*/OTTO_TOKEN`, `otto serve --promote-fly`, promote a live session, confirm reconnect to `wss://otto-session-<id>.fly.dev`, then demote and confirm the app is destroyed (`fly apps list`).
- **What's deliberately deferred (v1 / YAGNI):** active reaper, multi-session-per-app, dedicated IPs, app-secret token injection (currently `env`), automated orphan-empty-app GC.
- **Determinism guard:** none of the new code runs without `--promote-fly` + `OTTO_FLY_*`; `cargo test --workspace` with no env set must stay green.
