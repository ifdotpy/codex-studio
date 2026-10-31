//! Alert for a Canvas write transaction that holds the single writer too long
//! (docs/decisions/2026-10-28-target-client-server-architecture.md, "Storage").
//!
//! The owner of the duration is `SQLITE_WRITE_TRANSACTION_ALERT_MS` in
//! `studio_runtime_schema::shared_values`. A transaction at or over it logs one warning with
//! the caller label (`file:line` of the `transaction` call) and the duration. The
//! runtime diagnostics list the same transactions in `sqliteContention.slowTransactions`.

use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use studio_runtime_schema::shared_values::SQLITE_WRITE_TRANSACTION_ALERT_MS;

#[derive(Debug)]
pub(crate) struct WriteAlert {
    threshold_ms: AtomicU64,
    alerts: AtomicU64,
}

impl Default for WriteAlert {
    fn default() -> Self {
        Self {
            threshold_ms: AtomicU64::new(SQLITE_WRITE_TRANSACTION_ALERT_MS),
            alerts: AtomicU64::new(0),
        }
    }
}

impl WriteAlert {
    /// Log a warning when `held` reaches the threshold. Runs on the writer thread after the
    /// transaction ended, so it holds no lock of the database.
    pub(crate) fn observe(&self, origin: &Location<'_>, held: Duration) {
        let threshold = Duration::from_millis(self.threshold_ms.load(Ordering::Relaxed));
        if held >= threshold {
            self.alerts.fetch_add(1, Ordering::Relaxed);
            eprintln!("{}", alert_line(origin, held, threshold));
        }
    }

    pub(crate) fn count(&self) -> u64 {
        self.alerts.load(Ordering::Relaxed)
    }

    pub(crate) fn set_threshold(&self, threshold: Duration) {
        let millis = u64::try_from(threshold.as_millis()).unwrap_or(u64::MAX);
        self.threshold_ms.store(millis, Ordering::Relaxed);
    }
}

/// The warning line for one slow write transaction.
pub(crate) fn alert_line(origin: &Location<'_>, held: Duration, threshold: Duration) -> String {
    format!(
        "warning: Canvas write transaction held the writer for {} ms (alert at {} ms), caller {}:{}",
        held.as_millis(),
        threshold.as_millis(),
        origin.file(),
        origin.line()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_transaction_at_the_threshold_alerts() {
        let alert = WriteAlert::default();
        alert.set_threshold(Duration::from_millis(50));
        let origin = Location::caller();
        alert.observe(origin, Duration::from_millis(49));
        assert_eq!(alert.count(), 0);
        alert.observe(origin, Duration::from_millis(50));
        assert_eq!(alert.count(), 1);
    }

    #[test]
    fn the_warning_names_the_caller_and_the_duration() {
        let line = alert_line(
            Location::caller(),
            Duration::from_millis(1234),
            Duration::from_millis(1000),
        );
        assert!(line.contains("held the writer for 1234 ms"), "{line}");
        assert!(line.contains("alert at 1000 ms"), "{line}");
        assert!(line.contains("write_alert.rs:"), "{line}");
    }
}
