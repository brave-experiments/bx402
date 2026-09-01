//! Prometheus metrics: the registry, and the endpoint that exposes it.
//!
//! Metrics are served from a listener of their own rather than from the router
//! that takes public traffic. The page names every paid endpoint, how often
//! payments are refused, and how much has been charged, so keeping it off the
//! public port is a property of the process rather than of a proxy rule
//! somewhere upstream.

use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

/// Address the metrics listener binds. Fixed rather than configurable, like the
/// main bind, and above 1024 so the unprivileged container user can bind it.
const BIND_ADDR: &str = "0.0.0.0:8090";

/// Content type of the exposition the encoder writes.
const EXPOSITION: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// Everything the service records, and the registry that renders it.
///
/// Built once at startup and shared, so all recording lands in the one registry
/// the endpoint reads.
pub struct Metrics {
    registry: Registry,
}

impl Metrics {
    /// Build the registry and register every metric.
    ///
    /// The `bx402` prefix is applied by the registry, so metrics are registered
    /// under bare names and render prefixed.
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("bx402");
        registry.register(
            "build",
            "Version of the running service",
            Info::new(vec![("version", env!("CARGO_PKG_VERSION"))]),
        );
        Self { registry }
    }

    /// Render the current values as an OpenMetrics text exposition.
    fn render(&self) -> Result<String, std::fmt::Error> {
        let mut exposition = String::new();
        encode(&mut exposition, &self.registry)?;
        Ok(exposition)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Serve the metrics endpoint on its own listener until shutdown.
///
/// A port already in use aborts startup rather than leaving the service running
/// unobserved.
pub async fn serve(metrics: Arc<Metrics>) -> std::io::Result<()> {
    let router = Router::new()
        .route("/metrics", get(expose))
        .with_state(metrics);
    let listener = tokio::net::TcpListener::bind(BIND_ADDR).await?;
    tracing::info!("metrics listening on {}", listener.local_addr()?);
    axum::serve(listener, router).await
}

/// Answer a scrape with the current exposition.
async fn expose(State(metrics): State<Arc<Metrics>>) -> Response {
    match metrics.render() {
        Ok(exposition) => ([(header::CONTENT_TYPE, EXPOSITION)], exposition).into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "rendering metrics failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exposition_carries_the_running_version() {
        let rendered = Metrics::new().render().expect("metrics render");
        assert!(
            rendered.contains(&format!(
                "bx402_build_info{{version=\"{}\"}} 1",
                env!("CARGO_PKG_VERSION")
            )),
            "build info missing from:\n{rendered}"
        );
    }

    #[test]
    fn the_exposition_ends_the_way_openmetrics_requires() {
        let rendered = Metrics::new().render().expect("metrics render");
        assert!(rendered.ends_with("# EOF\n"), "unterminated:\n{rendered}");
    }
}
