//! Monotonic time shared by local runtime event enqueue and provider stdin writes.

/// System `CLOCK_MONOTONIC` nanoseconds, matching Python's `time.monotonic_ns` domain.
#[must_use]
pub fn monotonic_ns() -> u128 {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    time.tv_sec as u128 * 1_000_000_000 + time.tv_nsec as u128
}
