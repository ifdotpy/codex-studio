use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub enum SyncRecoveryKind {
    Reconnect,
    Gap,
    Overflow,
    Oversize,
    Reset,
    SubscriptionRejected,
}

static COUNTERS: [AtomicU64; 6] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

pub fn record_sync_recovery(kind: SyncRecoveryKind) {
    let index = match kind {
        SyncRecoveryKind::Reconnect => 0,
        SyncRecoveryKind::Gap => 1,
        SyncRecoveryKind::Overflow => 2,
        SyncRecoveryKind::Oversize => 3,
        SyncRecoveryKind::Reset => 4,
        SyncRecoveryKind::SubscriptionRejected => 5,
    };
    let counter = &COUNTERS[index];
    let mut current = counter.load(Ordering::Relaxed);
    while current < u64::MAX {
        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

pub fn sync_recovery_counters() -> Value {
    let counts = COUNTERS
        .each_ref()
        .map(|counter| counter.load(Ordering::Relaxed));
    json!({"reconnect":counts[0],"gap":counts[1],"overflow":counts[2],"oversize":counts[3],"reset":counts[4],"subscriptionRejected":counts[5]})
}
