//! The soft open-file limit of the server and the process supervisor (Python
//! `codex_open_file_limit`). launchd starts both with a soft limit of 256. Each loaded
//! Codex thread keeps pipes to its MCP servers, so a busy app-server reached that limit
//! and could not start commands (OS error 24). The providers are children of the
//! supervisor or the server and inherit its limit. The hard limit never changes.
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The soft open-file limit that the server and the supervisor ask for, in open files.
pub const OPEN_FILE_SOFT_LIMIT: u64 = 65_536;

/// Lower soft limits to try when the kernel refuses a higher one, in open files. macOS
/// caps the soft limit at `kern.maxfilesperproc` (61440 or 245760, depending on the
/// Mac); 10240 is the macOS `OPEN_MAX`.
const SOFT_LIMIT_STEPS: [u64; 4] = [61_440, 32_768, 16_384, 10_240];

/// Raise the soft limit toward [`OPEN_FILE_SOFT_LIMIT`], never above the hard limit and
/// never lower than it is. Returns the soft limit after the change, or `None` when it is
/// unlimited.
pub fn raise_open_file_limit() -> Option<u64> {
    let Rlimit { current, maximum } = getrlimit(Resource::Nofile);
    let soft = current?;
    let cap = |value: u64| maximum.map_or(value, |hard| hard.min(value));
    for wanted in std::iter::once(OPEN_FILE_SOFT_LIMIT)
        .chain(SOFT_LIMIT_STEPS)
        .map(cap)
    {
        if wanted <= soft {
            break;
        }
        let limit = Rlimit {
            current: Some(wanted),
            maximum,
        };
        if setrlimit(Resource::Nofile, limit).is_ok() {
            break;
        }
    }
    getrlimit(Resource::Nofile).current
}

#[cfg(test)]
#[path = "open_files_tests.rs"]
mod tests;
