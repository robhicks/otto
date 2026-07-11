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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn mint_token_is_unique_hex() {
        let t = mint_token();
        assert_eq!(t.len(), 32, "{t}");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()), "{t}");
        assert_ne!(mint_token(), mint_token());
    }
}
