//! Host memory pressure. No test creates real memory pressure.
#[cfg(target_os = "macos")]
#[path = "shell_pressure/macos.rs"]
mod macos;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub trait PressureSource: Send + Sync {
    fn critical(&self) -> bool;
    fn refresh(&self) -> futures_util::future::BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The native macOS dispatch source owns the kernel pressure verdict. Other
/// platforms use a conservative Linux PSI and available-memory observation.
pub struct HostPressure {
    #[cfg(target_os = "macos")]
    _source: macos::Source,
    critical: Arc<AtomicBool>,
}

impl Default for HostPressure {
    fn default() -> Self {
        let critical = Arc::new(AtomicBool::new(false));
        #[cfg(target_os = "macos")]
        {
            Self {
                _source: macos::Source::new(Arc::clone(&critical)),
                critical,
            }
        }
        #[cfg(not(target_os = "macos"))]
        Self { critical }
    }
}

impl PressureSource for HostPressure {
    fn critical(&self) -> bool {
        self.critical.load(Ordering::Acquire)
    }
    #[cfg(target_os = "linux")]
    fn refresh(&self) -> futures_util::future::BoxFuture<'_, ()> {
        Box::pin(async {
            let (memory, pressure) = tokio::join!(
                tokio::fs::read_to_string("/proc/meminfo"),
                tokio::fs::read_to_string("/proc/pressure/memory")
            );
            self.critical.store(matches!((memory, pressure), (Ok(memory), Ok(pressure)) if linux_critical(&memory, &pressure)), Ordering::Release);
        })
    }
}

/// PSI has no kernel-defined critical level. Require all tasks stalled for
/// 10% of the last ten seconds, plus Claude's Linux scarcity test (<10%, <4 GiB).
#[cfg(any(target_os = "linux", test))]
fn linux_critical(memory: &str, pressure: &str) -> bool {
    const CRITICAL_FULL_AVG10_PERCENT: f64 = 10.0;
    const SCARCE_AVAILABLE_FRACTION: f64 = 0.1;
    const SCARCE_AVAILABLE_KIB: f64 = 4.0 * 1024.0 * 1024.0;
    let field = |name| {
        memory.lines().find_map(|line| {
            let mut parts = line.strip_prefix(name)?.split_whitespace();
            let value = parts.next()?.parse::<u64>().ok()?;
            (parts.next() == Some("kB")).then_some(value as f64)
        })
    };
    let (Some(total), Some(available)) = (field("MemTotal:"), field("MemAvailable:")) else {
        return false;
    };
    let full = pressure
        .lines()
        .find(|line| line.starts_with("full "))
        .and_then(|line| {
            line.split_whitespace().find_map(|field| {
                field
                    .strip_prefix("avg10=")
                    .and_then(|value| value.parse::<f64>().ok())
            })
        });
    total > 0.0
        && available >= 0.0
        && available / total < SCARCE_AVAILABLE_FRACTION
        && available < SCARCE_AVAILABLE_KIB
        && full.is_some_and(|full| full.is_finite() && full >= CRITICAL_FULL_AVG10_PERCENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn linux_requires_both_scarcity_bounds_and_full_stalls() {
        let scarce = "MemTotal: 16000000 kB\nMemAvailable: 1000000 kB\n";
        assert!(linux_critical(
            scarce,
            "some avg10=50.0\nfull avg10=10.0 avg60=1.0\n"
        ));
        for (memory, pressure) in [
            (scarce, "full avg10=9.9"),
            (scarce, "some avg10=90.0"),
            (
                "MemTotal: 1000000 kB\nMemAvailable: 100000 kB",
                "full avg10=90",
            ),
            (
                "MemTotal: 100000000 kB\nMemAvailable: 5000000 kB",
                "full avg10=90",
            ),
            ("", "full avg10=90"),
            (scarce, "full avg10=NaN"),
        ] {
            assert!(!linux_critical(memory, pressure));
        }
    }
}
