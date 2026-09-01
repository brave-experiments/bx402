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

/// Why a request was answered with a challenge instead of served.
pub(crate) mod challenge {
    /// The request carried no payment proof at all.
    pub(crate) const NO_PAYMENT: &str = "no_payment";
    /// The proof was for a rail this deployment has turned off.
    pub(crate) const RAIL_DISABLED: &str = "rail_disabled";
    /// The request carried proof for both rails at once.
    pub(crate) const COLLISION: &str = "collision";
}

/// How a payment ended: the complete set of outcomes, whichever rail reports
/// them. Some values only ever come from one rail; they live together so the
/// whole vocabulary can be read, and checked against, in one place.
///
/// Every refusal looks the same to the client on purpose, so this is the only
/// place the reasons stay apart.
pub(crate) mod outcome {
    /// Paid, and the caller got what they paid for.
    pub(crate) const SETTLED: &str = "settled";
    /// The proof could not be read at all.
    pub(crate) const MALFORMED: &str = "malformed";
    /// The proof was readable but accepted no offer we made for this path.
    pub(crate) const NO_OFFER: &str = "no_offer";
    /// The payment was read and understood, and did not verify.
    pub(crate) const REFUSED: &str = "refused";
    /// The payer did not clear address screening.
    pub(crate) const SCREENED_OUT: &str = "screened_out";
    /// We could not reach the facilitator or the chain.
    pub(crate) const NETWORK_UNAVAILABLE: &str = "network_unavailable";
    /// Verified, then could not be settled.
    pub(crate) const SETTLE_FAILED: &str = "settle_failed";
    /// Paid for, but the upstream search failed, so nothing was charged.
    pub(crate) const UPSTREAM_FAILED: &str = "upstream_failed";
}

/// The steps of a payment worth timing separately. Which steps a rail reports
/// depends on whether it can check a payment without moving money.
pub(crate) mod step {
    /// Checking a payment without moving money.
    pub(crate) const VERIFY: &str = "verify";
    /// Moving the money.
    pub(crate) const SETTLE: &str = "settle";
}

/// One challenge the service issued.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ChallengeLabels {
    endpoint: &'static str,
    reason: &'static str,
}

/// One payment, and how it ended.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct PaymentLabels {
    rail: &'static str,
    endpoint: &'static str,
    outcome: &'static str,
}

/// One step of a payment, for its timing.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct StepLabels {
    rail: &'static str,
    step: &'static str,
}

/// One endpoint on one rail, for the money it took.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct ChargeLabels {
    rail: &'static str,
    endpoint: &'static str,
}

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
    challenges: Family<ChallengeLabels, Counter>,
    payments: Family<PaymentLabels, Counter>,
    payment_step_duration: Family<StepLabels, Histogram, fn() -> Histogram>,
    charged_base_units: Family<ChargeLabels, Counter>,
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

        let challenges = Family::default();
        registry.register(
            "challenges",
            "Payment challenges issued, by endpoint and reason",
            challenges.clone(),
        );

        let payments = Family::default();
        registry.register(
            "payments",
            "Payments attempted, by rail, endpoint and how they ended",
            payments.clone(),
        );

        let payment_step_duration =
            Family::new_with_constructor(duration_histogram as fn() -> Histogram);
        registry.register(
            "payment_step_duration_seconds",
            "Time for one step of a payment, by rail",
            payment_step_duration.clone(),
        );

        let charged_base_units = Family::default();
        registry.register(
            "charged_base_units",
            "Base units of currency charged for settled payments",
            charged_base_units.clone(),
        );

        Self {
            registry,
            requests,
            request_duration,
            upstream_requests,
            upstream_duration,
            challenges,
            payments,
            payment_step_duration,
            charged_base_units,
        }
    }

    /// Record one challenge the service issued instead of serving the request.
    pub(crate) fn record_challenge(&self, endpoint: &'static str, reason: &'static str) {
        self.challenges
            .get_or_create(&ChallengeLabels { endpoint, reason })
            .inc();
    }

    /// Record how one payment ended.
    pub(crate) fn record_payment(
        &self,
        rail: &'static str,
        endpoint: &'static str,
        outcome: &'static str,
    ) {
        self.payments
            .get_or_create(&PaymentLabels {
                rail,
                endpoint,
                outcome,
            })
            .inc();
    }

    /// Record how long one step of a payment took.
    pub(crate) fn record_payment_step(
        &self,
        rail: &'static str,
        step: &'static str,
        elapsed: Duration,
    ) {
        self.payment_step_duration
            .get_or_create(&StepLabels { rail, step })
            .observe(elapsed.as_secs_f64());
    }

    /// Record the money a settled payment brought in, in the currency's base
    /// units. Read from the catalog, so it is the price we advertised rather
    /// than anything the payer stated.
    pub(crate) fn record_charge(
        &self,
        rail: &'static str,
        endpoint: &'static str,
        base_units: u64,
    ) {
        self.charged_base_units
            .get_or_create(&ChargeLabels { rail, endpoint })
            .inc_by(base_units);
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
