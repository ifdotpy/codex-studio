//! Draft branch rules ported from web/src/sync/{conflicts,drafts}.ts.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct DraftVersion {
    pub id: String,
    pub session: String,
    pub device: String,
    pub text: String,
    pub updated: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub seen: BTreeMap<String, u64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl DraftVersion {
    pub fn validate(&self) -> Result<(), String> {
        let legacy_id = format!("{}:{}", self.device, self.session);
        let writer_id = self
            .id
            .strip_prefix(&format!("{}:", self.device))
            .and_then(|tail| tail.strip_suffix(&format!(":{}", self.session)))
            .is_some_and(|writer| !writer.is_empty() && !writer.contains(':'));
        if self.device.is_empty()
            || self.session.is_empty()
            || (self.id != legacy_id && !writer_id)
            || self.id.len() > 300
            || self.updated > 9_007_199_254_740_991
            || self.seen.values().any(|n| *n > 9_007_199_254_740_991)
        {
            return Err("Invalid draft identity or version.".into());
        }
        Ok(())
    }
}

pub fn merge(master: &DraftVersion, fork: &DraftVersion) -> Result<DraftVersion, String> {
    master.validate()?;
    fork.validate()?;
    if master.id != fork.id {
        return Err("Only versions of the same draft branch can merge.".into());
    }
    let (winner, other) = if fork.updated > master.updated {
        (fork, master)
    } else {
        (master, fork)
    };
    let replaced = winner
        .seen
        .get(&other.id)
        .is_some_and(|time| *time >= other.updated);
    let mut alternatives: Vec<_> = master
        .alternatives
        .iter()
        .chain(&fork.alternatives)
        .chain(if replaced { None } else { Some(&other.text) })
        .filter(|text| !text.is_empty() && **text != winner.text)
        .cloned()
        .collect();
    // JS Array.sort compares UTF-16 units. Preserve that order in saved payloads.
    alternatives.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    alternatives.dedup();
    Ok(DraftVersion {
        alternatives,
        ..winner.clone()
    })
}

#[derive(Debug, Default, Serialize)]
pub struct DraftView {
    pub drafts: BTreeMap<String, String>,
    pub conflicts: Vec<DraftVersion>,
}

pub fn reconcile(
    versions: &[DraftVersion],
    pending: &BTreeMap<String, String>,
    dismissed: &BTreeSet<(String, String)>,
) -> Result<DraftView, String> {
    let mut sessions: BTreeMap<&str, Vec<&DraftVersion>> = BTreeMap::new();
    for version in versions {
        version.validate()?;
        sessions.entry(&version.session).or_default().push(version);
    }
    let mut view = DraftView {
        drafts: pending.clone(),
        conflicts: Vec::new(),
    };
    let mut unique = BTreeSet::new();
    for (session, branches) in sessions {
        if pending.contains_key(session) {
            continue;
        }
        let mut active: Vec<_> = branches
            .iter()
            .copied()
            .filter(|version| {
                !branches.iter().any(|other| {
                    other.id != version.id
                        && other
                            .seen
                            .get(&version.id)
                            .is_some_and(|time| *time >= version.updated)
                })
            })
            .collect();
        active.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
        let chosen = active
            .iter()
            .find(|version| !dismissed.contains(&(version.id.clone(), version.text.clone())));
        let text = chosen
            .map(|version| version.text.clone())
            .unwrap_or_default();
        view.drafts.insert(session.into(), text.clone());
        for branch in active {
            let mut candidates = Vec::new();
            if !branch.text.is_empty() && branch.text != text {
                candidates.push(branch.clone());
            }
            for alternate in &branch.alternatives {
                if !alternate.is_empty() && *alternate != text {
                    candidates.push(DraftVersion {
                        id: format!("{}:conflict", branch.id),
                        text: alternate.clone(),
                        ..branch.clone()
                    });
                }
            }
            for conflict in candidates {
                let key = (conflict.id.clone(), conflict.text.clone());
                if !dismissed.contains(&key) && unique.insert(key) {
                    view.conflicts.push(conflict);
                }
            }
        }
    }
    Ok(view)
}

pub fn edit(
    device: &str,
    session: &str,
    text: String,
    versions: &[DraftVersion],
    now: u64,
) -> Result<DraftVersion, String> {
    let mut updated = now;
    let mut seen = BTreeMap::<String, u64>::new();
    for version in versions {
        version.validate()?;
        updated = updated.max(
            version
                .updated
                .checked_add(1)
                .ok_or("Draft clock overflow.")?,
        );
        if version.session != session {
            continue;
        }
        for (id, time) in version
            .seen
            .iter()
            .chain(std::iter::once((&version.id, &version.updated)))
        {
            seen.entry(id.clone())
                .and_modify(|old| *old = (*old).max(*time))
                .or_insert(*time);
        }
    }
    let id = format!("{device}:{session}");
    seen.entry(id.clone()).or_insert(0);
    let version = DraftVersion {
        id,
        session: session.into(),
        device: device.into(),
        text,
        updated,
        seen,
        alternatives: Vec::new(),
        extra: BTreeMap::new(),
    };
    version.validate()?;
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn branch(device: &str, text: &str, time: u64) -> DraftVersion {
        edit(device, "room", text.into(), &[], time).unwrap()
    }

    #[test]
    fn browser_writer_branches_keep_their_identity_and_concurrent_text() {
        let mut first = branch("device", "first tab", 10);
        first.id = "device:tab-one:room".into();
        let mut second = branch("device", "second tab", 11);
        second.id = "device:tab-two:room".into();
        let view = reconcile(
            &[first.clone(), second.clone()],
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(view.drafts["room"], "second tab");
        assert_eq!(view.conflicts[0].id, first.id);
        let replacement = edit(
            "device",
            "room",
            "replacement".into(),
            &[first.clone(), second],
            12,
        )
        .unwrap();
        assert_eq!(replacement.seen[&first.id], 10);
        for id in [
            "other:tab-one:room",
            "device::room",
            "device:tab-one:other",
            "device:a:b:room",
        ] {
            first.id = id.into();
            assert!(first.validate().is_err(), "{id}");
        }
    }

    #[test]
    fn same_branch_concurrent_edits_keep_both_texts() {
        let master = branch("device", "first tab", 10);
        let fork = branch("device", "second tab", 11);
        let merged = merge(&master, &fork).unwrap();
        assert_eq!(merged.text, "second tab");
        assert_eq!(merged.alternatives, ["first tab"]);
        assert_eq!(merge(&merged, &master).unwrap(), merged);
    }

    #[test]
    fn an_edit_replaces_only_the_versions_it_has_seen() {
        let first = branch("one", "first", 10);
        let concurrent = branch("two", "unseen", 11);
        let edit = edit(
            "one",
            "room",
            "replacement".into(),
            std::slice::from_ref(&first),
            9,
        )
        .unwrap();
        assert_eq!(edit.updated, 11);
        assert!(merge(&first, &edit).unwrap().alternatives.is_empty());
        let view = reconcile(&[edit, concurrent], &BTreeMap::new(), &BTreeSet::new()).unwrap();
        assert_eq!(view.conflicts.len(), 1);
        assert_eq!(view.conflicts[0].text, "unseen");
    }

    #[test]
    fn pending_local_text_is_not_replaced_by_replication() {
        let versions = [branch("one", "remote", 99)];
        let pending = BTreeMap::from([("room".into(), "typing".into())]);
        let view = reconcile(&versions, &pending, &BTreeSet::new()).unwrap();
        assert_eq!(view.drafts["room"], "typing");
        assert!(view.conflicts.is_empty());
    }

    #[test]
    fn merge_retains_unknown_payload_fields_and_unicode_order() {
        let mut master = branch("one", "master", 10);
        master
            .extra
            .insert("futureField".into(), serde_json::json!({"nested":true}));
        master.alternatives = vec!["\u{ffff}".into(), "🦀".into()];
        let merged = merge(&master, &branch("one", "fork", 9)).unwrap();
        assert_eq!(merged.extra, master.extra);
        assert_eq!(merged.alternatives, ["fork", "🦀", "\u{ffff}"]);
    }
}
