//! Native repository adapters for the shared overlay authority checks.
use crate::{oplog, repo::Repo};
use anyhow::Result;
pub use layr_model::authority::{from_lines, Authority, Change};
use std::collections::{BTreeMap, BTreeSet};

pub fn of(repo: &Repo, name: &str) -> Result<Authority> {
    let lines: Vec<_> = repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    let keys = crate::store_remote::overlays::trusted_keys(repo)?;
    if let Some(authority) = from_lines(&repo.p.id, &lines, &keys)?.remove(name) {
        return Ok(authority);
    }
    let orepo = repo.overlay_repo(name)?;
    let lines: Vec<_> = orepo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    Ok(layr_model::authority::from_overlay_lines(&repo.p.id, &orepo.p.id, &lines, &keys)?.1)
}

pub fn validate_import(project: &crate::store::Project, incoming: &[String]) -> Result<()> {
    let Some((parent, name)) = project.name.split_once('%') else { return Ok(()) };
    let repo = Repo { p: crate::store::Project::open(project.store, parent)? };
    let mut lines: BTreeSet<String> = project.export_jsonl()?.lines().map(str::to_owned).collect();
    lines.extend(incoming.iter().cloned());
    let id = if project.id.is_empty() {
        incoming
            .first()
            .map(|line| oplog::parse_line(line, None).map(|signed| signed.record.project))
            .transpose()?
            .unwrap_or_default()
    } else {
        project.id.clone()
    };
    let lines: Vec<_> = lines.into_iter().collect();
    let keys = crate::store_remote::overlays::trusted_keys(&repo)?;
    let parent_lines: Vec<_> = repo.p.export_jsonl()?.lines().map(str::to_owned).collect();
    if let Some(authority) = from_lines(&repo.p.id, &parent_lines, &keys)?.remove(name) {
        authority.validate(&lines, &id, &keys)
    } else {
        let (found, _) = layr_model::authority::from_overlay_lines(&repo.p.id, &id, &lines, &keys)?;
        anyhow::ensure!(found == name, "hidden overlay name differs from its project");
        Ok(())
    }
}

pub fn retained(authority: &Authority, repo: &Repo) -> Result<BTreeMap<String, String>> {
    let lines = repo.p.export_jsonl()?.lines().map(str::to_owned).collect::<Vec<_>>();
    layr_model::authority::retained(authority, &lines)
}
