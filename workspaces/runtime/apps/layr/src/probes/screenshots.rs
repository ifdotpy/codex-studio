//! File access for the shared screen comparison model.
use super::{FileData, Probe};
use crate::tree::Tree;
use anyhow::{bail, Result};
use layr_model::screenshots::{self as model, Screen};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

pub(super) use model::{changed, limit_reasons};

fn read(root: &Path, path: &str) -> Result<Vec<u8>> {
    let limit = if path.ends_with(".png") { model::MAX_IMAGE_BYTES } else { model::MAX_TEXT_BYTES };
    let mut bytes = Vec::new();
    Tree::open(root)?.open_read(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        if path.ends_with(".png") {
            bail!("PNG screen '{path}' exceeds the image allocation limit");
        }
        bail!("structure file '{path}' exceeds the text size limit");
    }
    Ok(bytes)
}
fn definition(probe: &Probe) -> layr_model::probes::Probe {
    layr_model::probes::Probe {
        run: probe.run.clone(),
        outputs: probe.outputs.clone(),
        kind: layr_model::probes::Kind::Screenshots,
        noise: probe.noise,
        fail_over: probe.fail_over,
        tolerance: probe.tolerance,
        ignore_attributes: probe.ignore_attributes.clone(),
    }
}
pub(super) fn validate(root: &Path, files: &BTreeMap<String, FileData>, probe: &Probe) -> Result<()> {
    let probe = definition(probe);
    for screen in model::inventory(files.keys().map(String::as_str))?.values() {
        for path in screen.png.iter().chain(&screen.structure) {
            model::validate_file(path, &read(root, path)?, &probe)?;
        }
    }
    Ok(())
}
fn load(root: &Path, screen: Option<&Screen>) -> Result<BTreeMap<String, Vec<u8>>> {
    screen.into_iter().flat_map(|s| s.png.iter().chain(&s.structure)).map(|path| Ok((path.clone(), read(root, path)?))).collect()
}
pub(super) fn compare(
    base_root: &Path,
    before: &BTreeMap<String, FileData>,
    head_root: &Path,
    after: &BTreeMap<String, FileData>,
    probe: &Probe,
) -> Result<Vec<Value>> {
    let old = model::inventory(before.keys().map(String::as_str))?;
    let new = model::inventory(after.keys().map(String::as_str))?;
    let probe = definition(probe);
    let mut out = Vec::new();
    // Load one screen pair at a time, rather than retaining every encoded image.
    for path in old.keys().chain(new.keys()).collect::<BTreeSet<_>>() {
        out.extend(model::compare(&load(base_root, old.get(path))?, &load(head_root, new.get(path))?, &probe)?);
    }
    Ok(out)
}

pub(super) fn summary(row: &Value) -> String {
    let mut text = row["screen"].as_str().unwrap_or("").to_owned();
    if row["status"] != "compared" {
        text.push_str(&format!(" {}", row["status"].as_str().unwrap_or("")));
    }
    if let Some(share) = row["changed_share"].as_f64() {
        let count = row["regions"].as_array().map_or(0, Vec::len);
        text.push_str(&format!(
            " {:.1} % of pixels changed, {count} {}",
            share * 100.0,
            if count == 1 { "region" } else { "regions" }
        ));
        if row["size_changed"] == true {
            text.push_str(&format!(
                ", size {}x{} to {}x{}",
                row["before"]["width"], row["before"]["height"], row["after"]["width"], row["after"]["height"]
            ));
        }
    }
    if row["structure_available"] == true {
        let mut counts = Vec::new();
        for (key, singular, plural) in [
            ("elements_added", "element added", "elements added"),
            ("elements_removed", "element removed", "elements removed"),
            ("text_changes", "text change", "text changes"),
        ] {
            let n = row[key].as_u64().unwrap_or(0);
            if n > 0 {
                counts.push(format!("{n} {}", if n == 1 { singular } else { plural }));
            }
        }
        let detail = if counts.is_empty() {
            if row["structure_diff"] == "" { "unchanged" } else { "changed" }.into()
        } else {
            counts.join(", ")
        };
        text.push_str(&format!("; structure: {detail}"));
    }
    text
}
