//! `FlyTarget` — provisions a fresh Fly Machine per session (one Fly app each), runs `otto serve`
//! on it, and disposes it on demote/stop. A `RemoteTarget` shaped like `VpsTarget` (explicit async
//! teardown, no drop-magic) because a billed remote machine must outlive the promote RPC. Always
//! compiled: it only does HTTP, so the whole flow is wiremock-tested in CI.

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
    /// Fly guest CPU tier: `"shared"` or `"performance"`. Required by the Machines API — omitting
    /// it is a hard 400 (`invalid config.guest.cpu_kind`). Default `"shared"` (shared-cpu-1x).
    pub vm_cpu_kind: String,
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

/// A globally-unique, DNS-safe Fly app name: `{prefix}-{12 hex}`.
pub(crate) fn gen_app_name(prefix: &str) -> String {
    let suffix: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect();
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
    ///
    /// Fly's GraphQL endpoint answers a failed mutation (bad appId, IP-quota exceeded, org
    /// mismatch) with **HTTP 200** and a body of `{"errors":[...]}` — `bail_on_error`'s
    /// status-only check would treat that as success, so this also inspects the body.
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
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("HTTP {status}: {body}");
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            if v.get("errors")
                .and_then(|e| e.as_array())
                .is_some_and(|a| !a.is_empty())
            {
                anyhow::bail!("Fly GraphQL error: {body}");
            }
        }
        Ok(())
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
                "OTTO_PROMOTION_SECRET": token,
                "OTTO_PORT": cfg.internal_port.to_string(),
                "OTTO_ROOT": "/workspace",
            },
            "guest": { "cpus": cfg.vm_cpus, "cpu_kind": cfg.vm_cpu_kind, "memory_mb": cfg.vm_mem_mib },
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

/// A `RemoteTarget` that provisions a fresh Fly Machine per session and disposes it explicitly.
/// Shaped like `VpsTarget` (returns a task-less `RemoteHandle`) because the machine must outlive
/// the promote RPC — it lives until an explicit `teardown` (demote/stop).
pub struct FlyTarget {
    api: FlyApi,
    cfg: FlyConfig,
}

impl FlyTarget {
    pub fn new(cfg: FlyConfig) -> Self {
        Self {
            api: FlyApi::from_config(&cfg),
            cfg,
        }
    }
}

#[async_trait]
impl crate::RemoteTarget for FlyTarget {
    async fn provision(
        &self,
        bundle: &crate::PromoteBundle,
    ) -> anyhow::Result<crate::RemoteHandle> {
        let token = crate::mint_session_secret();
        let app = gen_app_name(&self.cfg.app_prefix);

        // Everything after create_app must clean up on failure so a half-provisioned app is never
        // left billing. `run` collects the fallible steps; on Err we best-effort delete the app.
        let endpoint = self.api.session_endpoint(&app);
        let run = async {
            self.api.create_app(&app, &self.cfg.org_slug).await?;
            self.api.allocate_shared_ip(&app).await?;
            self.api.create_machine(&app, &self.cfg, &token).await?;
            self.api.wait_ready(&app, self.cfg.boot_timeout).await?;
            // One session per machine: the machine's own secret IS the session secret (spec A3) —
            // the minted token is injected as OTTO_PROMOTION_SECRET and rides the same value in the
            // X-Otto-Session-Secret header.
            crate::push_promote_bundle(&endpoint, &token, &token, bundle).await?;
            Ok::<(), anyhow::Error>(())
        };
        if let Err(e) = run.await {
            let _ = self.api.delete_app(&app).await; // best-effort; original error wins
            return Err(e);
        }
        Ok(crate::RemoteHandle::new(endpoint, token))
    }

    async fn teardown(&self, handle: crate::RemoteHandle) -> anyhow::Result<()> {
        let app = app_name_from_endpoint(&handle.endpoint).ok_or_else(|| {
            anyhow::anyhow!("cannot parse Fly app from endpoint {}", handle.endpoint)
        })?;
        self.api.delete_app(&app).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RemoteTarget;
    use wiremock::matchers::{header, header_exists, header_regex, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_cfg() -> FlyConfig {
        FlyConfig {
            api_token: "fly-tok".into(),
            org_slug: "personal".into(),
            region: "iad".into(),
            image: "registry.fly.io/otto-serve:latest".into(),
            vm_cpus: 1,
            vm_cpu_kind: "shared".into(),
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
        assert_eq!(body["config"]["env"]["OTTO_PROMOTION_SECRET"], "sess-tok");
        assert_eq!(body["config"]["env"]["OTTO_PORT"], "8787");
        assert_eq!(body["config"]["env"]["OTTO_ROOT"], "/workspace");
        assert_eq!(body["config"]["guest"]["cpus"], 1);
        // Required by the Machines API: omitting cpu_kind is a hard 400.
        assert_eq!(body["config"]["guest"]["cpu_kind"], "shared");
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
        assert_eq!(
            api.session_endpoint("otto-session-x"),
            "wss://otto-session-x.fly.dev"
        );
        let mut cfg = sample_cfg();
        cfg.public_base_override = Some("ws://127.0.0.1:9999".into());
        let api = FlyApi::from_config(&cfg);
        assert_eq!(api.session_endpoint("ignored"), "ws://127.0.0.1:9999");
    }

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
        assert_eq!(
            app_name_from_endpoint("http://otto-session-x.fly.dev"),
            None
        );
        assert_eq!(app_name_from_endpoint("otto-session-x"), None);
    }

    #[test]
    fn gen_app_name_is_prefixed_and_dns_safe() {
        let name = gen_app_name("otto-session");
        assert!(name.starts_with("otto-session-"), "{name}");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
            "{name}"
        );
        assert_ne!(gen_app_name("otto-session"), gen_app_name("otto-session"));
    }

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
        let err = api
            .create_app("x", "personal")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("422") && err.contains("name taken"), "{err}");
    }

    #[tokio::test]
    async fn allocate_shared_ip_posts_graphql() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .and(header("authorization", "Bearer fly-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{}}"#))
            .expect(1)
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        api.allocate_shared_ip("otto-session-x").await.unwrap();
    }

    #[tokio::test]
    async fn allocate_shared_ip_surfaces_graphql_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"errors":[{"message":"quota exceeded"}]}"#),
            )
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        let err = api
            .allocate_shared_ip("otto-session-x")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("quota exceeded"), "{err}");
    }

    #[tokio::test]
    async fn create_machine_posts_to_app_machines() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/apps/otto-session-x/machines"))
            .and(header("authorization", "Bearer fly-tok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"abc"}"#))
            .expect(1)
            .mount(&server)
            .await;
        let cfg = cfg_for(&server);
        let api = FlyApi::from_config(&cfg);
        api.create_machine("otto-session-x", &cfg, "sess-tok")
            .await
            .unwrap();
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
        api.wait_ready("otto-session-x", std::time::Duration::from_secs(5))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_app_issues_delete() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/apps/otto-session-x"))
            .and(header("authorization", "Bearer fly-tok"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let api = FlyApi::from_config(&cfg_for(&server));
        api.delete_app("otto-session-x").await.unwrap();
    }

    use otto_engine_core::types::WorkspaceSnapshot;
    use otto_persistence::{SessionState, SessionStatus};
    use otto_protocol::SessionId;

    fn empty_bundle() -> crate::PromoteBundle {
        crate::PromoteBundle {
            session: SessionState {
                id: SessionId::new(),
                owner: otto_protocol::UserId::local(),
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
        Mock::given(method("POST"))
            .and(path("/apps"))
            .respond_with(ResponseTemplate::new(201))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{}}"#))
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex(r"^/apps/.+/machines$"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"id":"m"}"#))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/"))
            .respond_with(ResponseTemplate::new(401))
            .mount(server)
            .await; // readiness
        Mock::given(method("POST"))
            .and(path("/promote"))
            // The per-session secret is minted inside `provision`, so presence + 32-hex shape is
            // all a pre-mounted matcher can assert; equality is covered by the env-injection test
            // and `handle.token.len() == 32` (env and header share the same mint).
            .and(header_exists("x-otto-session-secret"))
            .and(header_regex("x-otto-session-secret", "^[0-9a-f]{32}$"))
            .respond_with(ResponseTemplate::new(200))
            .mount(server)
            .await;
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
        Mock::given(method("POST"))
            .and(path("/apps"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":{}}"#))
            .mount(&server)
            .await;
        // create_machine fails → provision must clean up.
        Mock::given(method("POST"))
            .and(path_regex(r"^/apps/.+/machines$"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;
        let delete = Mock::given(method("DELETE"))
            .and(path_regex(r"^/apps/.+$"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1);
        server.register(delete).await;

        let target = FlyTarget::new(cfg_for(&server));
        assert!(target.provision(&empty_bundle()).await.is_err());
        // On drop, MockServer verifies the DELETE .expect(1) was satisfied.
    }

    #[tokio::test]
    async fn teardown_deletes_the_app_parsed_from_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/apps/otto-session-abc"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        // Endpoint carries the app name; delete goes to the (mock) machines_base.
        let target = FlyTarget::new(cfg_for(&server));
        let handle = crate::RemoteHandle::new("wss://otto-session-abc.fly.dev", "t");
        target.teardown(handle).await.unwrap();
    }
}
