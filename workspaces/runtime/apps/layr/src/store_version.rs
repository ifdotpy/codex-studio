//! Local store compatibility. This version is independent of encrypted remote formats.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub const FORMAT: u32 = 1;
pub const MARKER: &str = "store-version.json";
const LOCK: &str = ".layr-version.lock";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Version {
    pub format: u32,
    pub version: String,
    pub build: String,
}

impl Version {
    pub fn binary() -> Self {
        Self {
            format: FORMAT,
            version: env!("CARGO_PKG_VERSION").into(),
            build: option_env!("LAYR_BUILD_ID").unwrap_or("unknown").into(),
        }
    }

    pub(crate) fn description(&self) -> String {
        format!("format {} (layr {}, build {})", self.format, self.version, self.build)
    }

    pub fn require_compatible(&self, other: &Self, source: &str) -> Result<()> {
        if self.format != other.format {
            bail!("incompatible {source}: {}; this binary uses {}", other.description(), self.description());
        }
        Ok(())
    }
}

/// The shared lease prevents a format upgrade while a command or daemon holds a store.
pub struct Guard {
    _file: File,
}

fn marker(root: &Path) -> Result<Option<Version>> {
    let path = root.join(MARKER);
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {} for {}", path.display(), Version::binary().description())),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > 4096 {
        bail!("invalid store version file {} for {}", path.display(), Version::binary().description());
    }
    Ok(Some(
        serde_json::from_reader(file)
            .with_context(|| format!("invalid store version file {} for {}", path.display(), Version::binary().description()))?,
    ))
}

fn lock_file(root: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root.join(LOCK))
        .context("open the store version lock")?;
    if !file.metadata()?.is_file() {
        bail!("the store version lock is not a regular file");
    }
    Ok(file)
}

fn empty(root: &Path) -> Result<bool> {
    for entry in fs::read_dir(root)? {
        if entry?.file_name() != LOCK {
            return Ok(false);
        }
    }
    Ok(true)
}

fn legacy_error() -> anyhow::Error {
    anyhow::anyhow!(
        "incompatible store: format 0 (unversioned layr, unknown build); this binary uses {}. Use 'layr store-version --upgrade' explicitly",
        Version::binary().description()
    )
}

impl Guard {
    pub fn open(root: &Path) -> Result<Self> {
        let binary = Version::binary();
        if let Some(stored) = marker(root)? {
            // Refuse before opening a lock file or changing machine/project data.
            binary.require_compatible(&stored, "store")?;
            let file = lock_file(root)?;
            rustix::fs::flock(&file, rustix::fs::FlockOperation::LockShared)?;
            let stored = marker(root)?.context("the store version disappeared; refuse access")?;
            binary.require_compatible(&stored, "store")?;
            return Ok(Self { _file: file });
        }
        if !empty(root)? && marker(root)?.is_none() {
            return Err(legacy_error());
        }
        let file = lock_file(root)?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
        match marker(root)? {
            Some(stored) => binary.require_compatible(&stored, "store")?,
            None => {
                if !empty(root)? {
                    return Err(legacy_error());
                }
                crate::fsutil::atomic_write(&root.join(MARKER), &serde_json::to_vec(&binary)?, 0o644)?;
            }
        }
        Ok(Self { _file: file })
    }

    pub fn ready(&self, root: &Path) -> Result<()> {
        rustix::fs::flock(&self._file, rustix::fs::FlockOperation::LockShared)?;
        let stored = marker(root)?.context("the store version disappeared; refuse access")?;
        Version::binary().require_compatible(&stored, "store")
    }
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub binary: Version,
    pub store: Option<Version>,
    pub compatible: bool,
    pub state: &'static str,
}

/// Inspect without Store::open, locks, directory creation, or database migrations.
pub fn inspect(root: &Path) -> Result<Report> {
    let binary = Version::binary();
    let store = marker(root)?;
    let (state, compatible) = match &store {
        Some(stored) if stored.format == binary.format => ("compatible", true),
        Some(_) => ("incompatible", false),
        None => match fs::metadata(root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ("absent", true),
            Err(e) => return Err(e.into()),
            Ok(_) if empty(root)? => ("empty", true),
            Ok(_) => ("legacy", false),
        },
    };
    Ok(Report { binary, store, compatible, state })
}

fn upgrade(root: &Path) -> Result<()> {
    let report = inspect(root)?;
    if report.state == "absent" {
        bail!("the store does not exist; run a layr command to create it");
    }
    if let Some(stored) = &report.store {
        if stored.format != 0 && stored.format != FORMAT {
            Version::binary().require_compatible(stored, "store")?;
        }
    }
    let file = lock_file(root)?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .context("the store is in use; stop all layr commands and daemons before the format upgrade")?;
    // Recheck after the lease. Never downgrade a store that changed while we waited.
    if let Some(stored) = marker(root)? {
        if stored.format != 0 {
            return Version::binary().require_compatible(&stored, "store");
        }
    }
    crate::fsutil::atomic_write(&root.join(MARKER), &serde_json::to_vec(&Version::binary())?, 0o644)
}

pub fn run(root: &Path, args: &[String]) -> Result<i32> {
    let options = crate::args::parse(&crate::args::Spec::new().flag("--json").flag("--upgrade"), args)?;
    if !options.pos.is_empty() || !options.paths.is_empty() {
        bail!("usage: layr store-version [--json] [--upgrade]");
    }
    if options.has("--upgrade") {
        upgrade(root)?;
    }
    let report = inspect(root)?;
    if options.has("--json") {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("binary: {}", report.binary.description());
        match report.store {
            Some(stored) => println!("store: {} ({})", stored.description(), report.state),
            None => println!("store: {}", report.state),
        }
    }
    Ok(0)
}
