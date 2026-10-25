//! Relative object paths.

use anyhow::{bail, Result};
use std::path::{Component, Path};

/// Check a relative path: no absolute paths, no `..`, no empty components.
pub fn check_rel(path: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    let p = Path::new(path);
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            _ => bail!("invalid path: {path}"),
        }
    }
    Ok(())
}
