use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use super::{ROUTE_SAMPLE_LIMIT, Ring, SITE_LIMIT, quantiles, try_with_state};

const ROUTE_MAX_BYTES: usize = 160;

#[derive(Debug)]
struct RouteSamples {
    count: u64,
    latency_ns: Ring,
    response_bytes: Ring,
}

impl RouteSamples {
    fn new() -> Self {
        Self {
            count: 0,
            latency_ns: Ring::new(ROUTE_SAMPLE_LIMIT),
            response_bytes: Ring::new(ROUTE_SAMPLE_LIMIT),
        }
    }
}

#[derive(Debug, Default)]
struct HttpState {
    routes: BTreeMap<(String, String), RouteSamples>,
    untracked: u64,
}

static HTTP: OnceLock<Mutex<HttpState>> = OnceLock::new();

fn state() -> &'static Mutex<HttpState> {
    HTTP.get_or_init(|| Mutex::new(HttpState::default()))
}

/// Record a completed request using an Axum route template and response size.
/// A full registry, malformed template, or contended collector increments the
/// bounded untracked counter instead of delaying the request.
pub fn record_http_request(
    method: &str,
    route_template: &str,
    elapsed: Duration,
    response_bytes: usize,
) {
    let valid_method = matches!(
        method,
        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS"
    );
    let valid_route = valid_route_template(route_template);
    if !valid_method || !valid_route {
        let _ = try_with_state(state(), |state| {
            state.untracked = state.untracked.saturating_add(1)
        });
        return;
    }
    let key = (method.to_owned(), route_template.to_owned());
    let recorded = try_with_state(state(), |state| {
        record_route(state, key, elapsed, response_bytes);
    });
    if !recorded {
        let _ = try_with_state(state(), |state| {
            state.untracked = state.untracked.saturating_add(1)
        });
    }
}

fn valid_route_template(route: &str) -> bool {
    route.starts_with('/')
        && route.len() <= ROUTE_MAX_BYTES
        && route.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'{' | b'}' | b':')
        })
}

fn record_route(
    state: &mut HttpState,
    key: (String, String),
    elapsed: Duration,
    response_bytes: usize,
) {
    if !state.routes.contains_key(&key) && state.routes.len() >= SITE_LIMIT {
        state.untracked = state.untracked.saturating_add(1);
        return;
    }
    let samples = state.routes.entry(key).or_insert_with(RouteSamples::new);
    samples.count = samples.count.saturating_add(1);
    samples
        .latency_ns
        .record(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
    samples
        .response_bytes
        .record(response_bytes.min(u64::MAX as usize) as u64);
}

/// Return summaries from fixed-capacity rings. The returned snapshot itself is
/// bounded by the route limit and a try-lock avoids stalling an HTTP request.
pub fn http_request_metrics() -> Value {
    let mut routes = Vec::new();
    let mut untracked = 0;
    let _ = try_with_state(state(), |state| {
        untracked = state.untracked;
        routes = state
            .routes
            .iter()
            .map(|((method, route), samples)| {
                let (_, latency) = samples.latency_ns.snapshot();
                let (_, sizes) = samples.response_bytes.snapshot();
                json!({
                    "method": method,
                    "route": route,
                    "count": samples.count,
                    "latencyMs": quantiles(&latency, true, 1_000_000.0),
                    "responseBytes": quantiles(&sizes, true, 1.0),
                })
            })
            .collect();
    });
    json!({"routes": routes, "untrackedRoutes": untracked})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_metrics_keep_template_and_bound_sample_history() {
        let mut metrics = RouteSamples::new();
        for index in 0..=ROUTE_SAMPLE_LIMIT {
            metrics.count += 1;
            metrics.latency_ns.record(index as u64);
            metrics.response_bytes.record(5);
        }
        assert_eq!(metrics.count, (ROUTE_SAMPLE_LIMIT + 1) as u64);
        let samples = metrics.latency_ns.snapshot().1;
        assert_eq!(samples.len(), ROUTE_SAMPLE_LIMIT);
        assert_eq!(samples.first(), Some(&1));
        assert_eq!(samples.last(), Some(&(ROUTE_SAMPLE_LIMIT as u64)));
    }

    #[test]
    fn caller_supplied_url_values_are_rejected() {
        assert!(!valid_route_template("/api/agents/private-id?token=secret"));
        assert!(valid_route_template("/api/agents/{id}"));
    }

    #[test]
    fn route_registry_is_bounded_at_capacity() {
        let mut state = HttpState::default();
        for index in 0..=SITE_LIMIT {
            record_route(
                &mut state,
                ("GET".to_owned(), format!("/route-{index}")),
                Duration::from_millis(1),
                8,
            );
        }
        assert_eq!(state.routes.len(), SITE_LIMIT);
        assert_eq!(state.untracked, 1);
    }
}
