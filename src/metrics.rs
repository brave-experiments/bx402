//! Prometheus metrics: the registry, and the endpoint that exposes it.
//!
//! Metrics are served from a listener of their own rather than from the router
//! that takes public traffic. The page names every paid endpoint, how often
//! payments are refused, and how much has been charged, so keeping it off the
//! public port is a property of the process rather than of a proxy rule
//! somewhere upstream.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::Registry;

use crate::endpoints;

/// Address the metrics listener binds. Fixed rather than configurable, like the
/// main bind, and above 1024 so the unprivileged container user can bind it.
const BIND_ADDR: &str = "0.0.0.0:8090";

/// Content type of the exposition the encoder writes.
const EXPOSITION: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// Buckets for every duration we record, in seconds.
///
/// The boundaries straddle the deadlines this service enforces, so a histogram
/// answers directly how close requests are running to them: two seconds for a
/// screen, five for the upstream connect, fifteen for a whole upstream search.
/// The steps between one and five seconds are where paid requests actually
/// land, because settling on chain dominates the time.
const DURATION_BUCKETS: [f64; 11] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 8.0, 15.0, 30.0];

/// Catch-all label value, used wherever the caller supplies something outside
/// the set we serve, so a request cannot mint a series of its own.
const OTHER: &str = "other";

/// One request the service answered.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RequestLabels {
    endpoint: &'static str,
    method: &'static str,
    status: u16,
}

/// One route, for the timings that do not split by status.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RouteLabels {
    endpoint: &'static str,
    method: &'static str,
}

/// One call to the Brave Search API.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct UpstreamLabels {
    endpoint: &'static str,
    status: String,
}

/// An endpoint on its own.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct EndpointLabels {
    endpoint: &'static str,
}

/// A histogram over [`DURATION_BUCKETS`], built per label set.
fn duration_histogram() -> Histogram {
    Histogram::new(DURATION_BUCKETS)
}

/// Everything the service records, and the registry that renders it.
///
/// Built once at startup and shared, so all recording lands in the one registry
/// the endpoint reads.
pub struct Metrics {
    registry: Registry,
    requests: Family<RequestLabels, Counter>,
    request_duration: Family<RouteLabels, Histogram, fn() -> Histogram>,
    upstream_requests: Family<UpstreamLabels, Counter>,
    upstream_duration: Family<EndpointLabels, Histogram, fn() -> Histogram>,
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

        let requests = Family::default();
        registry.register(
            "http_requests",
            "Requests answered, by endpoint and status",
            requests.clone(),
        );

        let request_duration =
            Family::new_with_constructor(duration_histogram as fn() -> Histogram);
        registry.register(
            "http_request_duration_seconds",
            "Time to answer a request, payment included",
            request_duration.clone(),
        );

        let upstream_requests = Family::default();
        registry.register(
            "upstream_requests",
            "Calls to the Brave Search API, by endpoint and outcome",
            upstream_requests.clone(),
        );

        let upstream_duration =
            Family::new_with_constructor(duration_histogram as fn() -> Histogram);
        registry.register(
            "upstream_duration_seconds",
            "Time for one Brave Search API call, response body included",
            upstream_duration.clone(),
        );

        Self {
            registry,
            requests,
            request_duration,
            upstream_requests,
            upstream_duration,
        }
    }

    /// Record one request the service answered.
    fn record_request(
        &self,
        endpoint: &'static str,
        method: &'static str,
        status: StatusCode,
        elapsed: Duration,
    ) {
        self.requests
            .get_or_create(&RequestLabels {
                endpoint,
                method,
                status: status.as_u16(),
            })
            .inc();
        self.request_duration
            .get_or_create(&RouteLabels { endpoint, method })
            .observe(elapsed.as_secs_f64());
    }

    /// Record one call to the Brave Search API. `status` is the response code,
    /// or the kind of failure when no response arrived.
    pub(crate) fn record_upstream(&self, endpoint: &'static str, status: &str, elapsed: Duration) {
        self.upstream_requests
            .get_or_create(&UpstreamLabels {
                endpoint,
                status: status.to_owned(),
            })
            .inc();
        self.upstream_duration
            .get_or_create(&EndpointLabels { endpoint })
            .observe(elapsed.as_secs_f64());
    }

    /// Render the current values as an OpenMetrics text exposition.
    pub(crate) fn render(&self) -> Result<String, std::fmt::Error> {
        let mut exposition = String::new();
        encode(&mut exposition, &self.registry)?;
        Ok(exposition)
    }
}

/// The label for a request path: the paid endpoint it names, the health probe,
/// or [`OTHER`]. Drawn from the catalog rather than the request, so a
/// caller cannot mint label values by asking for paths that do not exist.
pub(crate) fn endpoint_label(path: &str) -> &'static str {
    if path == crate::HEALTH_PATH {
        return crate::HEALTH_PATH;
    }
    endpoints::find(path).map_or(OTHER, |endpoint| endpoint.path)
}

/// The label for a request method. Anything the service does not serve collapses
/// to one value, since the method is caller-supplied and otherwise unbounded.
fn method_label(method: &Method) -> &'static str {
    match *method {
        Method::GET => "GET",
        Method::POST => "POST",
        Method::HEAD => "HEAD",
        _ => OTHER,
    }
}

/// Count and time every request, including those matching no route.
pub(crate) async fn measure(
    State(metrics): State<Arc<Metrics>>,
    req: Request,
    next: Next,
) -> Response {
    let endpoint = endpoint_label(req.uri().path());
    let method = method_label(req.method());
    let started = Instant::now();
    let response = next.run(req).await;
    metrics.record_request(endpoint, method, response.status(), started.elapsed());
    response
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

/// Assert `series` appears verbatim in what `metrics` has recorded. Shared by the
/// tests in every module that records, so each assertion names only the series it
/// cares about.
#[cfg(test)]
pub(crate) fn assert_recorded(metrics: &Metrics, series: &str) {
    let exposition = metrics.render().expect("metrics render");
    assert!(
        exposition.contains(series),
        "missing `{series}` in:\n{exposition}"
    );
}

/// The inverse of [`assert_recorded`], for proving something was never recorded.
#[cfg(test)]
pub(crate) fn assert_not_recorded(metrics: &Metrics, fragment: &str) {
    let exposition = metrics.render().expect("metrics render");
    assert!(
        !exposition.contains(fragment),
        "unexpected `{fragment}` in:\n{exposition}"
    );
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
