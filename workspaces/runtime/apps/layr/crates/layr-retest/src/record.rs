//! Record format: one JSON file per state, `.layr-retest/<state>.json`.
//! A record is complete: it holds every test of its state, carried forward when the test did not run.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Inclusive line ranges `[first, last]`, sorted and merged.
pub type Ranges = Vec<[u32; 2]>;

/// Folder of the records, relative to the line root.
pub const RECORD_DIR: &str = ".layr-retest";

#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct FileCov {
    /// Lines with an execution count above 0.
    pub lines: Ranges,
    /// Line ranges of functions that the test executed.
    pub funcs: Ranges,
    /// Line ranges of functions in the file that the test did not execute.
    pub idle: Ranges,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct TestRec {
    /// `pass` or `fail`.
    pub status: String,
    /// Coverage per file; the key is a path relative to the line root.
    pub files: BTreeMap<String, FileCov>,
    /// Packages outside the project whose files the test executed: `<name>-<version>` (registry)
    /// or `git:<checkout folder>/<short rev>`.
    #[serde(default)]
    pub deps: Vec<String>,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Record {
    /// State of the record that this run started from. `None` for a full run.
    pub baseline: Option<String>,
    /// The nextest arguments of the run. A run with other arguments lists its tests again.
    #[serde(default)]
    pub args: Vec<String>,
    /// Key: `<binary id> <test name>`.
    pub tests: BTreeMap<String, TestRec>,
}

pub fn test_key(binary_id: &str, name: &str) -> String {
    format!("{binary_id} {name}")
}

pub fn record_path(root: &Path, state: &str) -> PathBuf {
    root.join(RECORD_DIR).join(format!("{state}.json"))
}

pub fn has_record(root: &Path, state: &str) -> bool {
    record_path(root, state).is_file()
}

/// Records of the tools that select tests themselves are in a folder per tool.
pub fn tool_record_path(root: &Path, tool: &str, state: &str) -> PathBuf {
    root.join(RECORD_DIR).join(tool).join(format!("{state}.json"))
}

pub fn load(root: &Path, state: &str) -> Result<Record> {
    let p = record_path(root, state);
    serde_json::from_slice(&std::fs::read(&p).with_context(|| p.display().to_string())?)
        .with_context(|| format!("bad record {}", p.display()))
}

pub fn save(root: &Path, state: &str, rec: &Record) -> Result<()> {
    let p = record_path(root, state);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(rec)?)?;
    std::fs::rename(&tmp, &p)?;
    Ok(())
}
