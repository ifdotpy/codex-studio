//! Read completed transfers, bisect results, and explicit change version provenance.

use crate::{Project, Record};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Remove URL user information, query parameters, and fragments before recording an address.
pub fn public_url(url: &str) -> String {
    let url = url.split(['?', '#']).next().unwrap_or("");
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.rsplit_once('@').map(|(_, address)| address).unwrap_or(url).into();
    };
    let (authority, path) = rest.split_once('/').map(|(a, p)| (a, format!("/{p}"))).unwrap_or((rest, String::new()));
    let host = authority.rsplit_once('@').map(|(_, host)| host).unwrap_or(authority);
    format!("{scheme}://{host}{path}")
}

/// Counts cover completed state transfers. Bytes count state payloads, excluding record metadata.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Transfer {
    pub direction: String,
    pub records_sent: usize,
    pub records_received: usize,
    pub states_sent: usize,
    pub states_received: usize,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub state_ids_sent: Vec<String>,
    pub state_ids_received: Vec<String>,
    pub overlays: Vec<String>,
}

/// Explicit provenance of a new version. The record envelope identifies its actor.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionOperation {
    pub state: String,
    pub kind: String,
    pub source: String,
    pub target_line: Option<String>,
    pub target_head: String,
}

fn evidence(record: &Record, mut data: Value) -> Value {
    data["id"] = json!(record.id);
    data["machine"] = json!(record.machine);
    data["time"] = json!(record.time);
    data["actor"] = json!(record.actor);
    data
}

impl Project {
    /// Completed push and sync operations, newest first.
    pub fn transfers(&self) -> Vec<Value> {
        self.op_log(None, usize::MAX)
            .iter()
            .filter(|r| matches!(r.op.as_str(), "push" | "sync") && r.data["result"] == "success")
            .map(|r| evidence(r, r.data.clone()))
            .collect()
    }

    /// Completed bisect runs, newest first. Each run keeps its command and reused check results.
    pub fn bisect_results(&self) -> Vec<Value> {
        self.op_log(None, usize::MAX).iter().filter(|r| r.op == "bisect.run").map(|r| evidence(r, r.data.clone())).collect()
    }

    /// Return explicit metadata only. Older state records do not imply a version operation.
    pub fn version_info(&self, state: &str) -> Option<Value> {
        self.records.iter().find_map(|r| {
            let version = r.data.get("version")?;
            (version["state"].as_str() == Some(state)).then(|| evidence(r, version.clone()))
        })
    }

    /// Configured remotes with the latest successful transfer from the machine of that configuration.
    pub fn copies(&self) -> Vec<Value> {
        let mut result = Vec::new();
        for (key, url) in &self.config {
            let Some(url) = url.as_str() else { continue };
            let (key, scope) = key.rsplit_once('@').unwrap_or((key, &self.machine));
            let remote = key
                .strip_prefix("cloud.remote.")
                .and_then(|v| v.strip_suffix(".url"))
                .map(|name| {
                    (
                        name,
                        if url.starts_with("ssh://") {
                            "peer"
                        } else if url.starts_with("oci://") {
                            "registry"
                        } else {
                            "store"
                        },
                    )
                })
                .or_else(|| key.strip_prefix("remote.").and_then(|v| v.strip_suffix(".url")).map(|name| (name, "git")));
            let Some((name, kind)) = remote else { continue };
            let address = public_url(url);
            let mut copy = json!({"id":format!("{scope}:{name}"),"name":name,"kind":kind,"address":address});
            if let Some(last) =
                self.transfers().into_iter().find(|r| r["machine"] == scope && r["remote"] == name && r["url"] == address)
            {
                copy["time"] = last["time"].clone();
                copy["machine"] = last["machine"].clone();
                copy["last"] = last;
            }
            result.push(copy);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{fold, record, state};
    use crate::StateKind;

    #[test]
    fn public_addresses_exclude_credentials_and_parameters() {
        for (input, expected) in [
            ("layr+https://alice:token@example.com:443/store?token=secret#secret", "layr+https://example.com:443/store"),
            ("ssh://alice@example.com/project", "ssh://example.com/project"),
            ("s3://bucket/project?X-Amz-Signature=secret", "s3://bucket/project"),
            ("file:///tmp/store", "file:///tmp/store"),
            ("https://alice%40mail:password@example.com/store", "https://example.com/store"),
            ("git@example.com:repo.git", "example.com:repo.git"),
            ("/tmp/repo.git", "/tmp/repo.git"),
        ] {
            assert_eq!(public_url(input), expected);
        }
    }

    #[test]
    fn copies_keep_completed_transfers_scoped_to_machine_and_address() {
        let mut config = record(1, "config");
        config.data = json!({"config":{"cloud.remote.store.url":"file:///old"}});
        let mut push = record(2, "push");
        push.data = json!({"remote":"store","url":"file:///old","result":"success","direction":"push","states_sent":1});
        let mut other = push.clone();
        other.id = "other-push".into();
        other.machine = "b".into();
        other.time = 3;
        let project = fold(vec![config.clone(), push.clone(), other]);
        let copies = project.copies();
        assert_eq!(copies[0]["time"], 2);
        assert_eq!(copies[0]["machine"], "a");
        assert_eq!(copies[0]["last"]["actor"]["uid"], 10);
        assert_eq!(project.op_log(Some("line"), 20).len(), 2);
        let mut changed = record(4, "config");
        changed.data = json!({"config":{"cloud.remote.store.url":"file:///new"}});
        assert!(fold(vec![config, push, changed]).copies()[0].get("last").is_none());
    }

    #[test]
    fn versions_keep_explicit_actor_and_target_and_old_states_omit_them() {
        let mut first = record(1, "commit");
        first.states.push(state("s1", StateKind::Commit, &[]));
        let mut amend = record(2, "commit");
        amend.actor.name = "Bob".into();
        amend.states.push(state("s2", StateKind::Commit, &[]));
        amend.data = json!({"version":{"state":"s2","kind":"amend","source":"s1","target_line":"line","target_head":"s1"}});
        let mut replay = record(3, "rebase.step");
        replay.states.push(state("s3", StateKind::Commit, &["main-head"]));
        replay.data =
            json!({"version":{"state":"s3","kind":"replay","source":"s2","target_line":"main","target_head":"main-head"}});
        let model = fold(vec![replay, first, amend]);
        assert!(model.version_info("s1").is_none());
        let info = model.version_info("s2").unwrap();
        assert_eq!(info["actor"]["name"], "Bob");
        assert_eq!(info["kind"], "amend");
        assert_eq!(model.version_info("s3").unwrap()["target_head"], "main-head");
    }

    #[test]
    fn bisect_results_keep_bounds_steps_and_reuse_in_record_order() {
        let mut first = record(1, "bisect.run");
        first.data = json!({"good":"s1","bad":"s3","first_bad":"s2","steps":[{"state":"s2","exit":1,"reused":false}]});
        let mut second = record(2, "bisect.run");
        second.data = first.data.clone();
        second.data["steps"][0]["reused"] = json!(true);
        let model = fold(vec![second.clone(), first, second]);
        let results = model.bisect_results();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["steps"][0]["reused"], true);
        assert_eq!(results[1]["steps"][0]["exit"], 1);
        assert_eq!(results[0]["first_bad"], "s2");
    }

    #[test]
    fn validation_refuses_private_urls_wrong_counts_and_unknown_version_kinds() {
        let id = "00000000-0000-4000-8000-000000000001";
        let mut r = Record::new("push");
        r.id = id.into();
        r.machine = id.into();
        r.project = id.into();
        r.data = serde_json::to_value(Transfer {
            direction: "push".into(),
            states_sent: 1,
            state_ids_sent: vec![id.into()],
            ..Default::default()
        })
        .unwrap();
        r.data["remote"] = json!("store");
        r.data["url"] = json!("https://host/project");
        r.data["result"] = json!("success");
        crate::validation::validate(&r).unwrap();
        for url in ["https://user:secret@host/project", "https://host/project?token=secret", "https://host/project#secret"] {
            let mut bad = r.clone();
            bad.data["url"] = json!(url);
            assert!(crate::validation::validate(&bad).is_err());
        }
        let mut bad = r.clone();
        bad.data["states_sent"] = json!(2);
        assert!(crate::validation::validate(&bad).is_err());
        bad = r;
        bad.data["version"] = json!({"state":id,"kind":"invented","source":id,"target_head":id,"target_line":id});
        assert!(crate::validation::validate(&bad).is_err());
    }
}
