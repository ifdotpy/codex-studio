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
}

impl Default for WriterSamples {
    fn default() -> Self {
        Self {
            count: 0,
            total_wait_ns: 0,
            total_hold_ns: 0,
            wait_ns: Ring::new(LOCK_SAMPLE_LIMIT),
            hold_ns: Ring::new(LOCK_SAMPLE_LIMIT),
        }
    }
}

static WRITER: OnceLock<Mutex<WriterSamples>> = OnceLock::new();

fn state() -> &'static Mutex<WriterSamples> {
    WRITER.get_or_init(|| Mutex::new(WriterSamples::default()))
}

/// Record the Rust equivalent of Python's interpreter lock metric: time waiting
/// for and holding the serialized SQLite writer transaction.
pub fn record_store_writer_transaction(wait: Duration, hold: Duration) {
    let _ = try_with_state(state(), |samples| {
        samples.count = samples.count.saturating_add(1);
        samples.total_wait_ns = samples.total_wait_ns.saturating_add(wait.as_nanos());
        samples.total_hold_ns = samples.total_hold_ns.saturating_add(hold.as_nanos());
        samples
            .wait_ns
            .record(wait.as_nanos().min(u128::from(u64::MAX)) as u64);
        samples
            .hold_ns
            .record(hold.as_nanos().min(u128::from(u64::MAX)) as u64);
    });
    super::sqlite::record_store_transaction(wait, hold);
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
        result.push(json!({
            "callSite": "runtime_store.writer_transaction",
            "count": samples.count,
            "totalWaitMs": samples.total_wait_ns as f64 / 1_000_000.0,
            "totalHoldMs": samples.total_hold_ns as f64 / 1_000_000.0,
            "waitMs": quantiles(&waits, false, 1_000_000.0),
            "holdMs": quantiles(&holds, false, 1_000_000.0),
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
