use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

use super::{LOCK_SAMPLE_LIMIT, Ring, quantiles, try_with_state};

#[derive(Debug)]
struct WriterSamples {
    count: u64,
    total_wait_ns: u128,
    total_hold_ns: u128,
    wait_ns: Ring,
    hold_ns: Ring,
    /// Transactions and total hold time per `#[track_caller]` call site. The
    /// key set is bounded by the number of call sites in the source.
    by_origin: HashMap<(&'static str, u32), (u64, u128)>,
}

impl Default for WriterSamples {
    fn default() -> Self {
        Self {
            count: 0,
            total_wait_ns: 0,
            total_hold_ns: 0,
            wait_ns: Ring::new(LOCK_SAMPLE_LIMIT),
            hold_ns: Ring::new(LOCK_SAMPLE_LIMIT),
            by_origin: HashMap::new(),
        }
    }
}

static WRITER: OnceLock<Mutex<WriterSamples>> = OnceLock::new();

fn state() -> &'static Mutex<WriterSamples> {
    WRITER.get_or_init(|| Mutex::new(WriterSamples::default()))
}

/// Record the Rust equivalent of Python's interpreter lock metric: time waiting
/// for and holding the serialized SQLite writer transaction.
pub fn record_store_writer_transaction(
    wait: Duration,
    hold: Duration,
    origin: &'static std::panic::Location<'static>,
) {
    let _ = try_with_state(state(), |samples| {
        samples.count = samples.count.saturating_add(1);
        samples.total_wait_ns = samples.total_wait_ns.saturating_add(wait.as_nanos());
        samples.total_hold_ns = samples.total_hold_ns.saturating_add(hold.as_nanos());
        let site = samples
            .by_origin
            .entry((origin.file(), origin.line()))
            .or_default();
        site.0 = site.0.saturating_add(1);
        site.1 = site.1.saturating_add(hold.as_nanos());
        samples
            .wait_ns
            .record(wait.as_nanos().min(u128::from(u64::MAX)) as u64);
        samples
            .hold_ns
            .record(hold.as_nanos().min(u128::from(u64::MAX)) as u64);
    });
    super::sqlite::record_store_transaction(wait, hold, origin);
}

/// Return the store writer series using the existing Python diagnostics keys.
/// These values do not claim to measure a Python `Runtime.lock` or bridge permit.
pub fn runtime_lock_operations() -> Vec<Value> {
    let mut result = Vec::new();
    let _ = try_with_state(state(), |samples| {
        if samples.count == 0 {
            return;
        }
        let (_, waits) = samples.wait_ns.snapshot();
        let (_, holds) = samples.hold_ns.snapshot();
        let mut origins: Vec<_> = samples.by_origin.iter().collect();
        origins.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
        let origins: Vec<Value> = origins
            .into_iter()
            .map(|((file, line), (count, hold_ns))| {
                json!({
                    "origin": format!("{file}:{line}"),
                    "count": count,
                    "totalHoldMs": *hold_ns as f64 / 1_000_000.0,
                })
            })
            .collect();
        result.push(json!({
            "callSite": "runtime_store.writer_transaction",
            "origins": origins,
            "count": samples.count,
            "totalWaitMs": samples.total_wait_ns as f64 / 1_000_000.0,
            "totalHoldMs": samples.total_hold_ns as f64 / 1_000_000.0,
            "waitMs": quantiles(&waits, true, 1_000_000.0),
            "holdMs": quantiles(&holds, true, 1_000_000.0),
        }));
    });
    result
}

/// Passive samples matching the legacy route shape. Rust has no interpreter
/// lock holder to probe; holder is therefore always null.
pub fn runtime_lock_samples() -> Vec<Value> {
    let mut result = Vec::new();
    let _ =
        try_with_state(state(), |samples| {
            let (_, waits) = samples.wait_ns.snapshot();
            result.extend(
                waits.into_iter().rev().take(5).rev().map(
                    |wait| json!({"waitMs": wait as f64 / 1_000_000.0, "holder": Value::Null}),
                ),
            );
        });
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_transactions_are_counted_per_call_site() {
        let first = std::panic::Location::caller();
        record_store_writer_transaction(Duration::ZERO, Duration::from_millis(2), first);
        record_store_writer_transaction(Duration::ZERO, Duration::from_millis(2), first);
        let operations = runtime_lock_operations();
        let writer = operations
            .iter()
            .find(|entry| entry["callSite"] == "runtime_store.writer_transaction")
            .expect("writer operation");
        let origin = format!("{}:{}", first.file(), first.line());
        let site = writer["origins"]
            .as_array()
            .expect("origins")
            .iter()
            .find(|site| site["origin"] == origin)
            .expect("call site entry");
        assert!(site["count"].as_u64().is_some_and(|count| count >= 2));
    }
}
