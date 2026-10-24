use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};

const STAGE_LIMIT: usize = 128;
const HISTOGRAM_BOUNDS_MS: [u64; 8] = [1, 5, 10, 25, 50, 100, 500, 1_000];

#[derive(Default)]
struct StageHistogram {
    count: u64,
    buckets: [u64; 9],
    total_ms: u64,
    max_ms: u64,
}

static STAGES: OnceLock<Mutex<BTreeMap<&'static str, StageHistogram>>> = OnceLock::new();

pub fn record_runtime_stage(name: &'static str, elapsed: Duration) {
    let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
    let mut stages = STAGES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !stages.contains_key(name) && stages.len() >= STAGE_LIMIT {
        return;
    }
    let histogram = stages.entry(name).or_default();
    histogram.count = histogram.count.saturating_add(1);
    histogram.total_ms = histogram.total_ms.saturating_add(elapsed_ms);
    histogram.max_ms = histogram.max_ms.max(elapsed_ms);
    let bucket = HISTOGRAM_BOUNDS_MS
        .iter()
        .position(|bound| elapsed_ms <= *bound)
        .unwrap_or(HISTOGRAM_BOUNDS_MS.len());
    histogram.buckets[bucket] = histogram.buckets[bucket].saturating_add(1);
}

pub fn runtime_stage_metrics() -> Value {
    let stages = STAGES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let rows = stages.iter().map(|(name, histogram)| {
        json!({"stage": name, "count": histogram.count, "totalMs": histogram.total_ms, "maxMs": histogram.max_ms, "boundsMs": HISTOGRAM_BOUNDS_MS, "buckets": histogram.buckets})
    }).collect::<Vec<_>>();
    json!({"stages": rows})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregates_value_free_stage_histogram() {
        let mut histogram = StageHistogram::default();
        record_into(&mut histogram, 2);
        record_into(&mut histogram, 101);
        assert_eq!(histogram.count, 2);
        assert_eq!(histogram.buckets[1], 1);
        assert_eq!(histogram.buckets[6], 1);
    }

    fn record_into(histogram: &mut StageHistogram, elapsed_ms: u64) {
        histogram.count += 1;
        histogram.buckets[HISTOGRAM_BOUNDS_MS
            .iter()
            .position(|bound| elapsed_ms <= *bound)
            .unwrap_or(HISTOGRAM_BOUNDS_MS.len())] += 1;
    }
}
