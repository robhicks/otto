//! Concrete routers for otto. `SingleProviderRouter` is a pass-through over one
//! provider; `BrainBlendRouter` selects across a local+remote pool with
//! privacy/complexity routing and cross-provider fallback; `PinnedModelRouter` pins
//! the remote model, routing every request remote unless the request is privacy-sensitive.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::TokenMeter;
use otto_engine_core::router::{RouteHints, Router, TaskKind};
use otto_engine_core::traits::Provider;
use otto_engine_core::types::{CompleteRequest, CompleteResponse};

/// A router that always delegates to a single provider, ignoring hints. Used for
/// deterministic tests and setups where only one provider is configured.
pub struct SingleProviderRouter {
    provider: Arc<dyn Provider>,
}

impl SingleProviderRouter {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl Router for SingleProviderRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        _hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        self.provider.complete(req).await
    }
}

/// Outcome of a routing decision: which provider id should handle a request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Route {
    Local,
    Remote,
}

/// Deterministic policy mapping `RouteHints` to a `Route`. Pure function, easily tested.
pub fn decide_route(hints: &RouteHints) -> Route {
    // Privacy always forces local, regardless of complexity.
    if hints.privacy_sensitive {
        return Route::Local;
    }
    // Escalate to remote after repeated local failures.
    if hints.prior_failures >= 2 {
        return Route::Remote;
    }
    // Complexity score in [0.0, 1.0]: blend task kind and context size.
    let kind_weight = match hints.task_kind {
        TaskKind::Boilerplate => 0.0_f64,
        TaskKind::Edit => 0.4,
        TaskKind::Architecture => 1.0,
    };
    // 8k tokens saturates the size contribution.
    let size_weight = (hints.token_estimate as f64 / 8000.0).min(1.0);
    let complexity = 0.6 * kind_weight + 0.4 * size_weight;
    if complexity >= 0.5 {
        Route::Remote
    } else {
        Route::Local
    }
}

/// Brain-Blend router: a local + remote provider with privacy/complexity routing and a
/// cross-provider fallback when the primary choice errors.
pub struct BrainBlendRouter {
    providers: HashMap<Route, Arc<dyn Provider>>,
}

impl BrainBlendRouter {
    pub fn new(local: Arc<dyn Provider>, remote: Arc<dyn Provider>) -> Self {
        let mut providers = HashMap::new();
        providers.insert(Route::Local, local);
        providers.insert(Route::Remote, remote);
        Self { providers }
    }

    fn provider(&self, route: &Route) -> &Arc<dyn Provider> {
        // Both keys are always present (inserted in `new`), so this cannot fail.
        self.providers.get(route).expect("route always present")
    }

    fn other(route: &Route) -> Route {
        match route {
            Route::Local => Route::Remote,
            Route::Remote => Route::Local,
        }
    }
}

#[async_trait]
impl Router for BrainBlendRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        let primary = decide_route(&hints);
        match self.provider(&primary).complete(req.clone()).await {
            Ok(resp) => Ok(resp),
            Err(primary_err) => {
                // Never fall back across the privacy boundary: a privacy-sensitive request
                // must stay on its (local) provider — re-sending it to the other (remote)
                // provider on failure would leak sensitive data. Surface the error instead.
                if hints.privacy_sensitive {
                    return Err(primary_err);
                }
                let fallback = Self::other(&primary);
                self.provider(&fallback)
                    .complete(req)
                    .await
                    .map_err(|fallback_err| {
                        anyhow::anyhow!(
                            "both providers failed: primary({primary:?})={primary_err}; \
                             fallback({fallback:?})={fallback_err}"
                        )
                    })
            }
        }
    }
}

/// A router that honors an explicitly pinned remote model: it routes every request to the
/// remote provider (built with the pinned model id) EXCEPT privacy-sensitive requests, which
/// stay local — the privacy floor is inviolable. Complexity and prior-failure-based escalation
/// are ignored — the caller named a model, so the remote is always primary. A liveness fallback
/// to local is retained if the remote errors on a non-privacy request, matching BrainBlendRouter.
pub struct PinnedModelRouter {
    local: Arc<dyn Provider>,
    remote: Arc<dyn Provider>,
}

impl PinnedModelRouter {
    pub fn new(local: Arc<dyn Provider>, remote: Arc<dyn Provider>) -> Self {
        Self { local, remote }
    }
}

#[async_trait]
impl Router for PinnedModelRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        // Privacy floor: a sensitive request stays local and never crosses to the remote model.
        if hints.privacy_sensitive {
            return self.local.complete(req).await;
        }
        match self.remote.complete(req.clone()).await {
            Ok(resp) => Ok(resp),
            // Non-privacy liveness fallback to local, matching BrainBlendRouter.
            Err(remote_err) => self.local.complete(req).await.map_err(|local_err| {
                anyhow::anyhow!(
                    "pinned remote failed and local fallback failed: \
                     remote={remote_err}; local={local_err}"
                )
            }),
        }
    }
}

/// A `Router` decorator that tallies each completion's reported token usage into a shared
/// `TokenMeter`, passing the response through unchanged. Agents are unaffected — they still
/// receive a `CompleteResponse`. Per-turn, the engine wraps the real router in this and reads
/// the meter to emit `TokenCostMeter` events.
pub struct MeteringRouter {
    inner: Arc<dyn Router>,
    meter: Arc<TokenMeter>,
}

impl MeteringRouter {
    pub fn new(inner: Arc<dyn Router>, meter: Arc<TokenMeter>) -> Self {
        Self { inner, meter }
    }
}

#[async_trait]
impl Router for MeteringRouter {
    async fn complete(
        &self,
        req: CompleteRequest,
        hints: RouteHints,
    ) -> anyhow::Result<CompleteResponse> {
        let resp = self.inner.complete(req, hints).await?;
        if let Some(u) = &resp.usage {
            self.meter.add(u);
        }
        Ok(resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoProvider;
    #[async_trait]
    impl Provider for EchoProvider {
        fn id(&self) -> &str {
            "echo"
        }
        async fn complete(&self, req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse {
                text: format!("echo:{}", req.prompt),
                usage: None,
            })
        }
    }

    struct TagProvider(&'static str);
    #[async_trait]
    impl Provider for TagProvider {
        fn id(&self) -> &str {
            self.0
        }
        async fn complete(&self, _req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse {
                text: self.0.to_string(),
                usage: None,
            })
        }
    }

    struct FailProvider;
    #[async_trait]
    impl Provider for FailProvider {
        fn id(&self) -> &str {
            "fail"
        }
        async fn complete(&self, _req: CompleteRequest) -> anyhow::Result<CompleteResponse> {
            anyhow::bail!("boom")
        }
    }

    #[tokio::test]
    async fn single_provider_router_delegates() {
        let router = SingleProviderRouter::new(Arc::new(EchoProvider));
        let out = router
            .complete(
                CompleteRequest {
                    prompt: "hi".into(),
                },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(out.text, "echo:hi");
    }

    #[test]
    fn privacy_forces_local_even_when_complex() {
        let hints = RouteHints {
            task_kind: TaskKind::Architecture,
            token_estimate: 100_000,
            privacy_sensitive: true,
            prior_failures: 0,
        };
        assert_eq!(decide_route(&hints), Route::Local);
    }

    #[test]
    fn boilerplate_routes_local_and_architecture_routes_remote() {
        assert_eq!(decide_route(&RouteHints::default()), Route::Local);
        assert_eq!(
            decide_route(&RouteHints {
                task_kind: TaskKind::Architecture,
                ..Default::default()
            }),
            Route::Remote
        );
    }

    #[test]
    fn repeated_failures_escalate_to_remote() {
        let hints = RouteHints {
            prior_failures: 2,
            ..Default::default()
        };
        assert_eq!(decide_route(&hints), Route::Remote);
    }

    #[tokio::test]
    async fn brain_blend_routes_local_for_boilerplate() {
        let router = BrainBlendRouter::new(
            Arc::new(TagProvider("local")),
            Arc::new(TagProvider("remote")),
        );
        let out = router
            .complete(
                CompleteRequest { prompt: "x".into() },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(out.text, "local");
    }

    #[tokio::test]
    async fn brain_blend_falls_back_when_primary_fails() {
        // Boilerplate → primary is local; make local fail, expect remote fallback.
        let router = BrainBlendRouter::new(Arc::new(FailProvider), Arc::new(TagProvider("remote")));
        let out = router
            .complete(
                CompleteRequest { prompt: "x".into() },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(out.text, "remote");
    }

    #[tokio::test]
    async fn privacy_sensitive_never_falls_back_to_remote() {
        // Privacy-sensitive → primary is local. If local fails, the request MUST NOT be
        // re-sent to the remote provider; it must surface the local error.
        let router = BrainBlendRouter::new(Arc::new(FailProvider), Arc::new(TagProvider("remote")));
        let hints = RouteHints {
            privacy_sensitive: true,
            ..RouteHints::default()
        };
        let err = router
            .complete(
                CompleteRequest {
                    prompt: "secret".into(),
                },
                hints,
            )
            .await
            .unwrap_err();
        // It returned the local failure, NOT the remote provider's "remote" text.
        assert!(
            err.to_string().contains("boom"),
            "expected local error, got: {err}"
        );
        assert!(
            !err.to_string().contains("remote"),
            "must not have reached remote: {err}"
        );
    }

    #[tokio::test]
    async fn both_providers_fail_surfaces_both_errors() {
        let router = BrainBlendRouter::new(Arc::new(FailProvider), Arc::new(FailProvider));
        let err = router
            .complete(
                CompleteRequest { prompt: "x".into() },
                RouteHints::default(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("primary"), "primary label missing: {msg}");
        assert!(msg.contains("fallback"), "fallback label missing: {msg}");
        assert!(msg.contains("boom"), "original error text missing: {msg}");
    }

    #[tokio::test]
    async fn remote_primary_falls_back_to_local() {
        // Architecture → primary is remote. If remote fails (non-privacy), fall back to local.
        let router = BrainBlendRouter::new(Arc::new(TagProvider("local")), Arc::new(FailProvider));
        let hints = RouteHints {
            task_kind: TaskKind::Architecture,
            ..RouteHints::default()
        };
        let out = router
            .complete(CompleteRequest { prompt: "x".into() }, hints)
            .await
            .unwrap();
        assert_eq!(out.text, "local");
    }

    use otto_engine_core::TokenMeter;
    use otto_engine_core::types::Usage;

    struct UsageRouter(Option<Usage>);
    #[async_trait]
    impl Router for UsageRouter {
        async fn complete(
            &self,
            _req: CompleteRequest,
            _hints: RouteHints,
        ) -> anyhow::Result<CompleteResponse> {
            Ok(CompleteResponse {
                text: "x".to_string(),
                usage: self.0,
            })
        }
    }

    #[tokio::test]
    async fn metering_router_tallies_usage_and_passes_through() {
        let meter = Arc::new(TokenMeter::default());
        let inner: Arc<dyn Router> = Arc::new(UsageRouter(Some(Usage {
            input_tokens: 2,
            output_tokens: 3,
        })));
        let r = MeteringRouter::new(inner, Arc::clone(&meter));

        let out = r
            .complete(
                CompleteRequest { prompt: "p".into() },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(out.text, "x"); // passed through unchanged
        assert_eq!(meter.snapshot(), (2, 3));

        r.complete(
            CompleteRequest { prompt: "p".into() },
            RouteHints::default(),
        )
        .await
        .unwrap();
        assert_eq!(meter.snapshot(), (4, 6)); // cumulative
    }

    #[tokio::test]
    async fn metering_router_ignores_none_usage() {
        let meter = Arc::new(TokenMeter::default());
        let inner: Arc<dyn Router> = Arc::new(UsageRouter(None));
        let r = MeteringRouter::new(inner, Arc::clone(&meter));
        r.complete(
            CompleteRequest { prompt: "p".into() },
            RouteHints::default(),
        )
        .await
        .unwrap();
        assert_eq!(meter.snapshot(), (0, 0));
    }

    #[tokio::test]
    async fn pinned_routes_remote_for_non_privacy() {
        // A pinned model must reach the remote slot regardless of task kind / complexity.
        let router = PinnedModelRouter::new(
            Arc::new(TagProvider("local")),
            Arc::new(TagProvider("remote")),
        );
        let out = router
            .complete(
                CompleteRequest { prompt: "x".into() },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(out.text, "remote");
    }

    #[tokio::test]
    async fn pinned_routes_local_for_privacy() {
        // The privacy floor is inviolable: a privacy-sensitive request stays local even when
        // a remote model is pinned.
        let router = PinnedModelRouter::new(
            Arc::new(TagProvider("local")),
            Arc::new(TagProvider("remote")),
        );
        let hints = RouteHints {
            privacy_sensitive: true,
            ..RouteHints::default()
        };
        let out = router
            .complete(
                CompleteRequest {
                    prompt: "secret".into(),
                },
                hints,
            )
            .await
            .unwrap();
        assert_eq!(out.text, "local");
    }

    #[tokio::test]
    async fn pinned_non_privacy_remote_error_falls_back_to_local() {
        // Liveness: a non-privacy remote failure falls back to local (matching BrainBlend).
        let router = PinnedModelRouter::new(Arc::new(TagProvider("local")), Arc::new(FailProvider));
        let out = router
            .complete(
                CompleteRequest { prompt: "x".into() },
                RouteHints::default(),
            )
            .await
            .unwrap();
        assert_eq!(out.text, "local");
    }

    #[tokio::test]
    async fn pinned_both_fail_surfaces_both_errors() {
        // Non-privacy: remote fails, local fallback also fails → combined error mentions both.
        let router = PinnedModelRouter::new(Arc::new(FailProvider), Arc::new(FailProvider));
        let err = router
            .complete(
                CompleteRequest { prompt: "x".into() },
                RouteHints::default(),
            )
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("remote="), "remote label missing: {msg}");
        assert!(msg.contains("local="), "local label missing: {msg}");
        assert!(msg.contains("boom"), "original error text missing: {msg}");
    }

    #[tokio::test]
    async fn pinned_privacy_error_never_crosses_to_remote() {
        // A privacy-sensitive request routes local; if local fails it MUST surface the error,
        // never re-send to the pinned remote model.
        let router =
            PinnedModelRouter::new(Arc::new(FailProvider), Arc::new(TagProvider("remote")));
        let hints = RouteHints {
            privacy_sensitive: true,
            ..RouteHints::default()
        };
        let err = router
            .complete(
                CompleteRequest {
                    prompt: "secret".into(),
                },
                hints,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("boom"),
            "expected local error, got: {err}"
        );
        assert!(
            !err.to_string().contains("remote"),
            "must not have reached remote: {err}"
        );
    }
}
