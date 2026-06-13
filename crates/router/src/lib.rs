//! Concrete routers for otto. `SingleProviderRouter` is a pass-through over one
//! provider; `BrainBlendRouter` (added later) selects across a pool.

use std::sync::Arc;

use async_trait::async_trait;
use otto_engine_core::router::{RouteHints, Router};
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
            })
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
}
