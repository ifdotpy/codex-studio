use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

const HISTOGRAM_BOUNDS_MS: [u64; 8] = [1, 5, 10, 25, 50, 100, 500, 1_000];
const STAGE_NAMES: [&str; 37] = [
    "account_driver.auth_metadata",
    "journal.compact",
    "journal.persist_streams",
    "journal.publications",
    "journal.snapshot",
    "lifecycle.projection",
    "ports.committed",
    "ports.committed.workspace_cleanup",
    "projection.agent_entities",
    "projection.canvas_chat_entities",
    "projection.direct_entities",
    "projection.event_window",
    "projection.monitor_window",
    "projection.peer_team_entities",
    "projection.source_snapshot",
    "projection.task_window",
    "projection.vault_recheck",
    "test.async",
    "test.histogram",
    "test.sync",
    "turn_preparation.account_auth",
    "turn_preparation.claim",
    "turn_preparation.ready",
    "turn_preparation.ready.preconditions",
    "turn_preparation.ready.submission_snapshot",
    "turn_preparation.ready.thread_snapshot",
    "turn_preparation.total",
    "turn_submission.commit.agent_put",
    "turn_submission.commit.compare",
    "turn_submission.commit.compile",
    "turn_submission.commit.preconditions",
    "turn_submission.commit.submission_snapshot",
    "turn_submission.commit.thread_snapshot",
    "turn_submission.commit.token",
    "turn_submission.submitted.authorize",
    "turn_submission.submitted.commit",
    "turn_submission.submitted.persist",
];

struct StageHistogram {
    count: AtomicU64,
    buckets: [AtomicU64; 9],
    total_ms: AtomicU64,
    max_ms: AtomicU64,
}

impl StageHistogram {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            buckets: [const { AtomicU64::new(0) }; 9],
            total_ms: AtomicU64::new(0),
            max_ms: AtomicU64::new(0),
        }
    }

    fn record(&self, elapsed_ms: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.total_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
        self.max_ms.fetch_max(elapsed_ms, Ordering::Relaxed);
        let bucket = HISTOGRAM_BOUNDS_MS
            .iter()
            .position(|bound| elapsed_ms <= *bound)
            .unwrap_or(HISTOGRAM_BOUNDS_MS.len());
        self.buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }
}

static STAGES: [StageHistogram; STAGE_NAMES.len()] =
    [const { StageHistogram::new() }; STAGE_NAMES.len()];

pub fn record_runtime_stage(name: &'static str, elapsed: Duration) {
    let Some(index) = STAGE_NAMES.iter().position(|stage| *stage == name) else {
        return;
    };
    let elapsed_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
    STAGES[index].record(elapsed_ms);
}

pub fn runtime_stage_metrics() -> Value {
    let stages = STAGE_NAMES.iter().enumerate().map(|(index, name)| {
        let histogram = &STAGES[index];
        json!({
            "stage": name,
            "count": histogram.count.load(Ordering::Relaxed),
            "totalMs": histogram.total_ms.load(Ordering::Relaxed),
            "maxMs": histogram.max_ms.load(Ordering::Relaxed),
            "boundsMs": HISTOGRAM_BOUNDS_MS,
            "buckets": histogram.buckets.each_ref().map(|bucket| bucket.load(Ordering::Relaxed)),
        })
    }).collect::<Vec<_>>();
    json!({"stages": stages})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_stage_recorder_updates_the_fixed_histogram() {
        record_runtime_stage("test.histogram", Duration::from_millis(2));
        record_runtime_stage("test.histogram", Duration::from_millis(101));
        let histogram = &STAGES[STAGE_NAMES
            .iter()
            .position(|name| *name == "test.histogram")
            .unwrap()];
        assert_eq!(histogram.count.load(Ordering::Relaxed), 2);
        assert_eq!(histogram.buckets[1].load(Ordering::Relaxed), 1);
        assert_eq!(histogram.buckets[6].load(Ordering::Relaxed), 1);
        assert_eq!(histogram.total_ms.load(Ordering::Relaxed), 103);
        assert_eq!(histogram.max_ms.load(Ordering::Relaxed), 101);
    }
}
