//! On-demand path overlaps between live lines. Nothing is written to permanent history.

use crate::changes;
use crate::ctx::Ctx;
use crate::model::Line;
use crate::repo::{skip_path, Repo};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Overlap {
    pub path: String,
    pub line: String,
    pub owner: u32,
    /// Unix time in milliseconds, from the other line's file or latest saved state.
    pub changed_at: i64,
}

fn changed_paths(base: &Path, child: &Path, hidden: &BTreeSet<String>) -> Result<BTreeMap<String, i64>> {
    let mut out = BTreeMap::new();
    for change in changes::candidates(base, child)? {
        if skip_path(&change.path)
            || hidden.contains(&change.path)
            || change.old_path.as_ref().is_some_and(|p| hidden.contains(p))
        {
            continue;
        }
        if changes::really_changed(base, child, &change)? {
            let time = change.new.as_ref().map(|m| m.mtime * 1000).unwrap_or(0);
            out.insert(change.path, time);
            if let Some(old) = change.old_path {
                out.insert(old, time);
            }
        }
    }
    Ok(out)
}

impl Repo<'_> {
    fn radar_hidden(&self, ctx: &Ctx, line: &Line) -> Result<BTreeSet<String>> {
        let mut hidden = BTreeSet::new();
        for layer in self.layers(line)? {
            if !self.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                // Include earlier and newer private paths, so deletions cannot expose names.
                hidden.extend(self.private_paths(line)?);
            }
        }
        for (path, overlay) in self.pending_moves(line) {
            if !self.reads_overlay(&overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                hidden.insert(path);
            }
        }
        Ok(hidden)
    }

    pub fn overlaps(&self, ctx: &Ctx, line: &Line) -> Result<Vec<Overlap>> {
        self.can_read(ctx)?;
        let ours = self.scan(line)?;
        let our_hidden = self.radar_hidden(ctx, line)?;
        let our_private = self.private_paths(line)?;
        let mut ours_by_base = BTreeMap::new();
        let mut overlaps = Vec::new();
        for other in self.lines()? {
            if other.id == line.id || other.machine != self.p.store.machine.id || !self.working(&other).exists() {
                continue;
            }
            let base = match self.merge_base(&line.head, &other.head)? {
                Some(base) => base,
                None => continue,
            };
            let root = match self.state_root(&base) {
                Ok(root) => root,
                Err(_) => continue,
            };
            if !ours_by_base.contains_key(&base) {
                ours_by_base.insert(base.clone(), changed_paths(&root, &ours.path, &our_private)?);
            }
            let our_paths = &ours_by_base[&base];
            let hidden = self.radar_hidden(ctx, &other)?;
            // show <state>:<path> and try use project read access for both head and auto states.
            let head = self.state(&other.head)?;
            let state = self.last_auto(&other)?.filter(|s| s.time >= head.time).unwrap_or(head);
            let theirs = self.state_root(&state.id)?;
            let their_private = self.private_paths(&other)?;
            for (path, time) in changed_paths(&root, &theirs, &their_private)? {
                if our_paths.contains_key(&path) && !our_hidden.contains(&path) && !hidden.contains(&path) {
                    overlaps.push(Overlap {
                        path,
                        line: other.name.clone(),
                        owner: other.owner,
                        changed_at: if time == 0 { state.time } else { time },
                    });
                }
            }
            overlaps.extend(self.overlay_overlaps(ctx, line, &other, &ours.path, &our_hidden, &hidden)?);
        }
        overlaps.sort_by(|a, b| (&a.path, &a.line).cmp(&(&b.path, &b.line)));
        overlaps.dedup_by(|a, b| a.path == b.path && a.line == b.line);
        Ok(overlaps)
    }

    fn overlay_overlaps(
        &self,
        ctx: &Ctx,
        line: &Line,
        other: &Line,
        ours: &Path,
        our_hidden: &BTreeSet<String>,
        their_hidden: &BTreeSet<String>,
    ) -> Result<Vec<Overlap>> {
        let mut overlaps = Vec::new();
        let their_layers = self.layers(other)?;
        for layer in self.layers(line)? {
            if !self.reads_overlay(&layer.overlay, ctx.caller.uid, ctx.caller.gid).unwrap_or(false) {
                continue;
            }
            let theirs = match their_layers.iter().find(|other| other.overlay == layer.overlay) {
                Some(l) => l,
                None => continue,
            };
            let repo = self.overlay_repo(&layer.overlay)?;
            // Use the same policy as show/try for this overlay's immutable states.
            if repo.can_read(ctx).is_err() {
                continue;
            }
            let base = match repo.merge_base(&layer.state, &theirs.state)? {
                Some(base) => base,
                None => continue,
            };
            let base_root = match repo.state_root(&base) {
                Ok(root) => root,
                Err(_) => continue,
            };
            let their_root = match repo.state_root(&theirs.state) {
                Ok(root) => root,
                Err(_) => continue,
            };
            let our_changes = changed_paths(&base_root, ours, our_hidden)?;
            let state = repo.state(&theirs.state)?;
            for (path, time) in changed_paths(&base_root, &their_root, their_hidden)? {
                if our_changes.contains_key(&path) {
                    overlaps.push(Overlap {
                        path,
                        line: other.name.clone(),
                        owner: other.owner,
                        changed_at: if time == 0 { state.time } else { time },
                    });
                }
            }
        }
        Ok(overlaps)
    }
}
