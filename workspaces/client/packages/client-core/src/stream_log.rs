//! Appends one line per event stream change to a file that diagnostics read.
//! The file shows who closed a stream (the UI, the server or the core) and why.
use std::{io::Write, path::PathBuf, sync::OnceLock};

/// File name of the stream log inside the client data folder.
pub const STREAM_LOG_FILE: &str = "sync-streams.log";
/// Size at which the log starts again, in bytes.
const STREAM_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Sets the log folder once at startup. Without it, `line` does nothing.
pub fn start(directory: &std::path::Path) {
    let _ = PATH.set(directory.join(STREAM_LOG_FILE));
}

pub fn line(text: &str) {
    let Some(path) = PATH.get() else { return };
    let restart = std::fs::metadata(path).is_ok_and(|m| m.len() > STREAM_LOG_MAX_BYTES);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(!restart)
        .write(true)
        .truncate(restart)
        .open(path);
    if let Ok(mut file) = file {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(file, "{now} {}", text.replace('\n', " "));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lines_go_to_the_log_file() {
        let directory = tempfile::tempdir().unwrap();
        super::start(directory.path());
        super::line("open one");
        super::line("close one\nby ui");
        let text = std::fs::read_to_string(directory.path().join(super::STREAM_LOG_FILE)).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" open one"));
        assert!(lines[1].ends_with(" close one by ui"));
    }
}
