//! `LAYR_TRACE=1` prints how long the expensive steps take (to standard error).

use std::time::Instant;

pub struct Span {
    name: String,
    start: Instant,
}

pub fn span(name: impl Into<String>) -> Option<Span> {
    if std::env::var_os("LAYR_TRACE").is_some() {
        Some(Span { name: name.into(), start: Instant::now() })
    } else {
        None
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        eprintln!("trace {:>8.1} ms  {}", self.start.elapsed().as_secs_f64() * 1000.0, self.name);
    }
}
