use super::*;
use serde_json::json;

pub(super) fn record(seq: u64, op: &str) -> Record {
    let mut r = Record::new(op);
    r.id = format!("record-{seq}");
    r.project = "project".into();
    r.machine = "a".into();
    r.seq = seq;
    r.time = seq as i64;
    r.actor.uid = 10;
    r
}
pub(super) fn state(id: &str, kind: StateKind, parents: &[&str]) -> StateRec {
    StateRec {
        id: id.into(),
        kind,
        change_id: Some("change".into()),
        derived_from: None,
        context: None,
        parents: parents.iter().map(|s| s.to_string()).collect(),
        line: Some("line".into()),
        subvol: String::new(),
        author: Author::default(),
        message: id.into(),
        time: 1,
        running: false,
        conflicts: vec![],
        caches: BTreeMap::new(),
        cache_by: None,
        cache_shared: false,
        cache_platform: None,
    }
}
fn line(head: &str, machine: &str) -> LineChange {
    LineChange {
        before: None,
        after: Some(LineState { name: "main".into(), head: head.into(), owner: 10, machine: machine.into() }),
        working_before: None,
        working_after: None,
    }
}
pub(super) fn fold(records: Vec<Record>) -> Project {
    Project::from_records("project", records, "a").unwrap()
}

#[test]
fn record_order_duplicates_and_conflicts() {
    let mut first = record(1, "commit");
    first.states.push(state("s1", StateKind::Commit, &[]));
    first.lines.insert("line".into(), line("s1", "a"));
    let mut last = record(2, "commit");
    last.states.push(state("s2", StateKind::Commit, &["s1"]));
    last.lines.insert("line".into(), line("s2", "a"));
    let model = fold(vec![last.clone(), first.clone(), last.clone()]);
    assert_eq!(model.line_list()[0].head, "s2");
    assert_eq!(model.change_versions("change").iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["s1", "s2"]);
    let mut conflict = last.clone();
    conflict.op = "other".into();
    assert!(Project::from_records("project", vec![last.clone(), conflict], "a").is_err());
    let mut conflict = last.clone();
    conflict.id = "other".into();
    assert!(Project::from_records("project", vec![last, conflict], "a").is_err());
    let mut bad = first.clone();
    bad.project = "different".into();
    assert!(Project::from_records("project", vec![bad], "a").is_err());
    let mut bad = record(3, "commit");
    bad.states = first.states.clone();
    assert!(Project::from_records("project", vec![first, bad], "a").is_err());
}
#[test]
fn heads_and_policy_keep_machine_scope() {
    let mut a = record(1, "project");
    a.data = json!({"config":{"access":{"user:10":"admin"},"grants":{"user:11":{"role":"reader","until":50}}}});
    a.lines.insert("line".into(), line("one", "a"));
    let mut b = record(2, "config");
    b.machine = "b".into();
    b.lines.insert("line".into(), line("two", "b"));
    b.data = json!({"config":{"access":{"*":"admin"}}});
    let p = fold(vec![b, a]);
    assert_eq!(p.line_list()[0].head, "one");
    assert_eq!(p.lines["line"].heads["b"], "two");
    let policy = p.access_policy(49, false);
    assert_eq!(policy.role(&policy::Who { uid: 11, groups: vec![] }), policy::Role::Reader);
    assert_eq!(p.access_policy(50, false).role(&policy::Who { uid: 11, groups: vec![] }), policy::Role::None);
    assert_eq!(p.access_policy(50, true).role(&policy::Who { uid: 11, groups: vec![] }), policy::Role::Admin);
}
#[test]
fn refs_expire_and_history_keeps_deletion() {
    let mut a = record(1, "ref.set");
    a.data = json!({"ref_change":{"name":"env/prod","before":null,"after":{"name":"env/prod","target":"s1","expires_at":10}}});
    let p = fold(vec![a.clone()]);
    assert_eq!(p.ref_list("env/", 9).len(), 1);
    assert!(p.ref_list("env/", 10).is_empty());
    let mut b = record(2, "ref.delete");
    b.data = json!({"ref_change":{"name":"env/prod","before":{"name":"env/prod","target":"s1","expires_at":10},"after":null}});
    let p = fold(vec![b, a]);
    assert!(p.ref_list("", 0).is_empty());
    assert!(p.ref_log("env/prod")[0].after.is_none());
    assert_eq!(p.ref_log("env/prod").len(), 2);
}
#[test]
fn revocations_bind_subject_actor_machine_and_order() {
    let mut approval = record(1, "approve");
    approval.data = json!({"state":"s1"});
    let mut wrong_actor = record(2, "approve.withdraw");
    wrong_actor.actor.uid = 11;
    wrong_actor.data = json!({"state":"s1"});
    let mut wrong_machine = record(3, "approve.withdraw");
    wrong_machine.machine = "b".into();
    wrong_machine.data = json!({"state":"s1"});
    let mut wrong_subject = record(4, "attest");
    wrong_subject.data = json!({"attestation":{"format":1,"attestation_id":"record-4","subject":{"kind":"state","id":"s2"},"kind":"revoke","payload_digest":"00","signer":"a","time":4},"revokes":["record-1"]});
    let mut records = vec![approval, wrong_actor, wrong_machine, wrong_subject];
    let p = fold(records.clone());
    assert!(!p.is_revoked("record-1"));
    assert_eq!(p.approvals("s1"), [(10, 1)]);
    let mut withdraw = record(5, "approve.withdraw");
    withdraw.data = json!({"state":"s1"});
    records.push(withdraw);
    let p = fold(records);
    assert!(p.is_revoked("record-1"));
    assert!(p.approvals("s1").is_empty());
    assert_eq!(p.attestations("s1", None).len(), 4);
}
#[test]
fn check_arguments_and_legacy_commands_are_exact() {
    let mut a = record(1, "check");
    a.data = json!({"state":"s1","argv":["sh","-c","true"],"command":"sh -c true","exit":0});
    let mut b = record(2, "check");
    b.data = json!({"state":"s1","argv":["sh","-c","true"],"command":"sh -c true","exit":7});
    let p = fold(vec![b, a]);
    let argv = vec!["sh".into(), "-c".into(), "true".into()];
    assert_eq!(p.check_exit("s1", &argv, None), Some(7));
    assert_eq!(p.check_exit("s1", &argv, Some(0)), Some(0));
    assert_eq!(p.check_exit("s1", &["sh -c true".into()], None), None);
    let mut c = record(3, "check");
    c.data = json!({"state":"s1","command":"true","exit":0});
    let p = fold(vec![c]);
    assert_eq!(p.check_exit("s1", &["true".into()], None), Some(0));
}
#[test]
fn history_walks_merge_and_auto_states_but_excludes_auxiliary_roots() {
    let mut a = record(1, "commit");
    a.states = vec![
        state("root", StateKind::Commit, &[]),
        state("a", StateKind::Commit, &["root"]),
        state("b", StateKind::Commit, &["root"]),
        state("merge", StateKind::Commit, &["a", "b"]),
        state("auto", StateKind::Auto, &["merge"]),
        state("derived", StateKind::Derived, &[]),
    ];
    for (index, state) in a.states.iter_mut().enumerate() {
        state.time = index as i64 + 1;
    }
    let p = fold(vec![a]);
    let ids: Vec<_> = p.log(&["merge".into()], Some("line"), 99).into_iter().map(|s| s.id).collect();
    assert_eq!(ids, ["auto", "merge", "b", "a", "root"]);
    assert!(p.log(&["derived".into()], None, 99).is_empty());
    assert_eq!(p.merge_base("a", "b"), Some("root".into()));
}
#[test]
fn queue_outcomes_keep_probes_and_removal() {
    let entry = json!({"id":"q","into":"main","target_line":"line","state":"s1","added_by":10,"time":1,"status":"done","attempts":2,"result":"merged","probes":{"status":"passed","behavior":{"base":"before","head":"merged","base_source":"available","head_source":"available","probes":[]}}});
    let mut a = record(1, "queue.done");
    a.data = json!({"queue_change":{"id":"q","before":null,"after":entry}});
    let p = fold(vec![a.clone()]);
    assert_eq!(p.queue_list(None)[0].probes.as_ref().unwrap()["status"], "passed");
    let mut b = record(2, "queue.remove");
    b.data = json!({"queue_change":{"id":"q","before":entry,"after":null}});
    assert!(fold(vec![b, a]).queue_list(None).is_empty());
}
#[test]
fn overlays_stay_separate_and_unknown_records_stay_in_log() {
    let mut parent = record(1, "future.op");
    parent.data = json!({"unknown":{"field":1}});
    let mut overlay = record(2, "project");
    overlay.project = "overlay".into();
    overlay.data = json!({"name":"p%keys","config":{"access":{}}});
    let m = Model::from_records(vec![overlay, parent], "a").unwrap();
    assert_eq!(m.projects.len(), 2);
    assert_eq!(m.projects["project"].op_log(None, 10)[0].op, "future.op");
    assert_eq!(m.projects["overlay"].name.as_deref(), Some("p%keys"));
}

#[test]
fn legacy_evidence_keeps_the_change_id_at_its_first_derivation() {
    let mut approval = record(1, "approve");
    approval.data = json!({"state":"s1"});
    let mut save = record(2, "commit");
    save.states.push(state("s1", StateKind::Commit, &[]));
    let p = fold(vec![save, approval]);
    // Db::derive_attestation updates only the local flag on a duplicate attestation.
    assert_eq!(p.attestations("s1", Some("approval"))[0].change_id, None);
}

#[test]
fn protection_preserves_distinct_ref_and_merge_author_rules() {
    let mut save = record(1, "commit");
    let mut s = state("s", StateKind::Commit, &[]);
    s.author.uid = 10;
    save.states.push(s.clone());
    let lookup = |id: &str| -> anyhow::Result<Option<StateRec>> { Ok((id == "s").then(|| s.clone())) };
    assert!(protection::approvals("s", Some("s"), protection::ApprovalMode::Ref, &[(10, 1)], lookup, |_| true)
        .unwrap()
        .is_empty());
    assert_eq!(
        protection::approvals("s", Some("s"), protection::ApprovalMode::Merge, &[(10, 1)], lookup, |_| true).unwrap().len(),
        1
    );
    assert!(protection::approvals("s", None, protection::ApprovalMode::Merge, &[(11, 1)], lookup, |_| false).unwrap().is_empty());
}
#[test]
fn protection_checks_and_paths_use_exact_requirements() {
    assert_eq!(protection::check_argv("true"), ["sh", "-c", "true"]);
    assert_eq!(protection::required_check("gate", Some("true"), true, Some(9)).result, protection::CheckResult::Passed);
    assert_eq!(protection::required_check("gate", Some("true"), false, Some(9)).result, protection::CheckResult::Failed);
    assert_eq!(protection::required_check("gate", None, false, None).result, protection::CheckResult::Undefined);
    let requirements = protection::path_requirements(["a", "b"], policy::Role::Writer, |path| {
        if path == "a" {
            policy::Role::Maintainer
        } else {
            policy::Role::Writer
        }
    });
    assert_eq!(requirements[0].met, Some(false));
    assert_eq!(requirements[1].met, Some(true));
}

#[test]
fn merge_status_selects_target_machine_and_keeps_unknown_actor_and_groups_undecided() {
    let mut init = record(1, "project");
    init.states = vec![state("root", StateKind::Commit, &[]), state("source", StateKind::Commit, &["root"])];
    for s in &mut init.states {
        s.author.uid = 10;
    }
    init.lines.insert("line".into(), line("root", "a"));
    init.data = json!({"config":{"access":{"user:0":"admin","user:11":"reader"},"protect":{"main":{"update":"writer","check":["gate"],"approvals":1,"approvers":["group:20"],"paths":{"secret":"admin"}}},"check.gate":"true"}});
    let mut approve = record(2, "approve");
    approve.actor.uid = 11;
    approve.data = json!({"state":"source"});
    let mut check = record(3, "check");
    check.data = json!({"state":"source","argv":["sh","-c","true"],"exit":0});
    let project = Project::from_records("project", vec![init, approve, check], "").unwrap();
    let mut context = protection::Context {
        now: 10,
        actor: None,
        approver_groups: BTreeMap::new(),
        changed_paths: vec!["secret".into()],
        candidate: None,
    };
    let summary = project.merge_status("source", "main", &context).unwrap();
    assert_eq!(summary.target_machine.as_deref(), Some("a"));
    assert_eq!(summary.allowed, None);
    assert_eq!(summary.status, "not_decided");
    assert_eq!(summary.path_requirements[0].met, None);
    assert_eq!(summary.approvals[0].met, None);
    assert_eq!(summary.checks[0].result, protection::CheckResult::Passed);
    assert!(summary.undecided_reasons.iter().any(|s| s.contains("membership not known here")));
    context.actor = Some(policy::Who { uid: 0, groups: vec![] });
    context.approver_groups.insert(11, vec![20]);
    assert_eq!(project.merge_status("source", "main", &context).unwrap().allowed, Some(true));
    let unknown = project.merge_status("source", "absent", &context).unwrap();
    assert_eq!(unknown.status, "unavailable");
    assert_eq!(unknown.allowed, None);
}

#[test]
fn hidden_layers_come_only_from_their_own_records() {
    let mut parent = record(1, "project");
    parent.lines.insert("line".into(), line("shared", "a"));
    parent.data = json!({"config":{}});
    let mut own = record(1, "project");
    own.project = "overlay-project".into();
    own.data = json!({"config":{"overlay":{"project":"project","name":"hidden","hidden":true,"mode":"follow"}}});
    let mut head = record(2, "line.create");
    head.project = "overlay-project".into();
    let mut change = line("hidden-state", "a");
    change.after.as_mut().unwrap().name = "line-line".into();
    head.lines.insert("overlay-line".into(), change);
    let reader = Model::from_records([parent.clone(), own, head], "a").unwrap();
    let other = Model::from_records([parent], "a").unwrap();
    let line = other.projects["project"].line_list().remove(0);
    assert_eq!(reader.layers("project", &line).unwrap()[0].overlay, "hidden");
    assert!(other.layers("project", &line).unwrap().is_empty());
    assert!(other.overlay_projects("project").is_empty());
    assert_eq!(
        serde_json::to_value(&reader.projects["project"]).unwrap(),
        serde_json::to_value(&other.projects["project"]).unwrap()
    );
    assert!(!serde_json::to_string(&other).unwrap().contains("hidden"));
}
