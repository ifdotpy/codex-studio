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
fn change_show_includes_active_check_results_for_each_exact_version() {
    let mut first = record(1, "commit");
    first.states.push(state("s1", StateKind::Commit, &[]));
    let mut failed = record(2, "check");
    failed.data = json!({"state":"s1","argv":["test","-f","fixed"],"command":"test -f fixed","exit":1});
    let mut second = record(3, "commit");
    second.states.push(state("s2", StateKind::Commit, &[]));
    let mut passed = record(4, "check");
    passed.data = json!({"state":"s2","argv":["true"],"command":"true","exit":0});
    let mut revoked = record(5, "attest");
    revoked.data = json!({"attestation":{"format":1,"attestation_id":"record-5","subject":{"kind":"state","id":"s2"},"kind":"revoke","payload_digest":"00","signer":"a","time":5},"revokes":["record-4"]});
    let p = fold(vec![first, failed.clone(), second, passed, revoked]);
    let show = p.change_show("change", &probes::Context::default()).unwrap();
    assert_eq!(show["versions"][0]["checks"], json!([failed.data]));
    assert_eq!(show["versions"][1]["checks"], json!([]));
    assert_eq!(show["versions"][0]["checks"], json!(p.checks("s1")));
    assert_eq!(show["versions"][1]["attestations"].as_array().unwrap().len(), 2);
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

fn person_evidence(seq: u64, login: &str, key: &ed25519_dalek::SigningKey, withdraw: bool) -> Record {
    use layr_store::request::{Action, Request};
    let mut record = Record::new("attest");
    record.id = format!("01a12290-0000-7000-8000-{:012x}", 100 + seq);
    record.project = "01a12290-0000-7000-8000-000000000002".into();
    record.machine = "01a12290-0000-7000-8000-000000000005".into();
    record.seq = seq;
    record.time = seq as i64;
    record.actor = Actor { uid: u32::MAX, name: format!("person:{login}"), session: None };
    let subject = "01a12290-0000-7000-8000-000000000003";
    let action = if withdraw {
        Action::Withdraw { state: subject.into(), into: "main".into() }
    } else {
        Action::Approve { state: subject.into(), into: "main".into() }
    };
    let request =
        Request::new(format!("01a12290-0000-7000-8000-{:012x}", 200 + seq), record.project.clone(), login.into(), 0, action)
            .unwrap();
    let signed = request.sign(key).unwrap();
    let text = String::from_utf8(signed.to_bytes().unwrap()).unwrap();
    record.data = json!({"request":text,"request_id":request.id,"request_digest":hex::encode(Sha256::digest(text.as_bytes())),"person_key":hex::encode(key.verifying_key().to_bytes()),"attestation":Attestation {format:1,attestation_id:record.id.clone(),subject:Subject{kind:"state".into(),id:subject.into()},change_id:None,kind:if withdraw {"revoke".into()}else{"approval".into()},payload_digest:hex::encode(Sha256::digest(signed.r.get().as_bytes())),payload_ref:None,signer:record.actor.name.clone(),time:record.time}});
    validation::validate(&record).unwrap();
    record
}

#[test]
fn person_approvals_survive_executor_scope_and_keep_revocations_separate() {
    let key = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
    let bob_key = ed25519_dalek::SigningKey::from_bytes(&[13; 32]);
    let attacker_key = ed25519_dalek::SigningKey::from_bytes(&[12; 32]);
    let ann = person_evidence(3, "ann@example.com", &key, false);
    let bob = person_evidence(4, "bob@example.com", &bob_key, false);
    let mut source = Record::new("save");
    source.id = "01a12290-0000-7000-8000-000000000001".into();
    source.project = ann.project.clone();
    source.machine = ann.machine.clone();
    source.seq = 2;
    source.time = 2;
    source.states.push(state(&ann.data["attestation"]["subject"]["id"].as_str().unwrap(), StateKind::Commit, &[]));
    source.states[0].author.email = "author@example.com".into();
    let mut pin = source.clone();
    pin.id = "01a12290-0000-7000-8000-000000000009".into();
    pin.seq = 1;
    pin.time = 1;
    pin.states.clear();
    pin.op = "config".into();
    pin.data = json!({"config":{"person.keys":{"ann@example.com":hex::encode(key.verifying_key().to_bytes()),"bob@example.com":hex::encode(bob_key.verifying_key().to_bytes())}}});
    let mut revoke = person_evidence(5, "bob@example.com", &bob_key, true);
    revoke.machine = "01a12290-0000-7000-8000-000000000006".into();
    revoke.data["revokes"] = json!([ann.id, bob.id]);
    let mut fake = person_evidence(6, "ann@example.com", &attacker_key, false);
    fake.machine = revoke.machine.clone();
    let mut fake_revoke = person_evidence(7, "ann@example.com", &attacker_key, true);
    fake_revoke.machine = revoke.machine.clone();
    fake_revoke.data["revokes"] = json!([ann.id]);
    let mut foreign_pin = pin.clone();
    foreign_pin.id = "01a12290-0000-7000-8000-000000000099".into();
    foreign_pin.machine = revoke.machine.clone();
    foreign_pin.seq = 8;
    foreign_pin.time = 8;
    foreign_pin.data = json!({"config":{"person.keys":{"ann@example.com":hex::encode(attacker_key.verifying_key().to_bytes())}}});
    let project = Project::from_records(
        &ann.project,
        vec![pin, source, ann.clone(), bob.clone(), revoke, fake.clone(), fake_revoke, foreign_pin],
        &ann.machine,
    )
    .unwrap();
    let subject = ann.data["attestation"]["subject"]["id"].as_str().unwrap();
    assert!(!project.is_revoked(&ann.id));
    assert!(project.is_revoked(&bob.id));
    assert!(project.approvals(subject).is_empty());
    assert_eq!(project.person_approvals(subject), vec![("ann@example.com".into(), 3)]);
    assert_eq!(project.person_attestations(subject, "ann@example.com"), vec![ann.id.clone()]);
    let mut policy = policy::Policy::default();
    policy.access.insert("person:ann@example.com".into(), policy::Role::Maintainer);
    let rule = policy::RuleSpec::default().rule();
    assert_eq!(project.allowed_person_approvals(subject, None, protection::ApprovalMode::Ref, &policy, &rule).len(), 1);
    policy.deny.insert("person:ann@example.com".into(), vec!["*".into()]);
    assert!(project.allowed_person_approvals(subject, None, protection::ApprovalMode::Ref, &policy, &rule).is_empty());
    let no_pin = Project::from_records(&ann.project, vec![fake.clone()], &ann.machine).unwrap();
    assert!(no_pin.person_approvals(subject).is_empty());
}

#[test]
fn encrypted_person_proof_binds_ciphertext_digest_and_inner_routing() {
    use sha2::{Digest, Sha256};
    let key = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
    let identity = layr_store::keys::generate_identity();
    let recipient = layr_store::keys::recipient_of(&identity).unwrap();
    let mut record = person_evidence(3, "ann@example.com", &key, false);
    let inner = record.data["request"].as_str().unwrap();
    let bytes = layr_store::request::seal(inner.as_bytes(), &recipient, &key).unwrap();
    record.data["encrypted_request"] = json!(String::from_utf8(bytes.clone()).unwrap());
    record.data["request_digest"] = json!(hex::encode(Sha256::digest(&bytes)));
    validation::validate(&record).unwrap();
    let mut changed = record.clone();
    changed.data["request_digest"] = json!("0".repeat(64));
    assert!(validation::validate(&changed).is_err());
    let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    envelope["person"] = json!("bob@example.com");
    changed = record.clone();
    let text = envelope.to_string();
    changed.data["request_digest"] = json!(hex::encode(Sha256::digest(text.as_bytes())));
    changed.data["encrypted_request"] = json!(text);
    assert!(validation::validate(&changed).is_err());
    envelope["person"] = json!("ann@example.com");
    envelope["sig"] = json!("0".repeat(128));
    let text = envelope.to_string();
    changed.data["request_digest"] = json!(hex::encode(Sha256::digest(text.as_bytes())));
    changed.data["encrypted_request"] = json!(text);
    assert!(validation::validate(&changed).is_err());
}

#[test]
fn a_person_proof_cannot_be_reused_for_another_effect() {
    use layr_store::request::{Action, Request};
    let key = ed25519_dalek::SigningKey::from_bytes(&[11; 32]);
    let id = "01a12290-0000-7000-8000-000000000001";
    let project = "01a12290-0000-7000-8000-000000000002";
    let subject = "01a12290-0000-7000-8000-000000000003";
    let request = Request::new(
        id.into(),
        project.into(),
        "ann@example.com".into(),
        1,
        Action::Approve { state: subject.into(), into: "main".into() },
    )
    .unwrap();
    let signed = request.sign(&key).unwrap();
    let text = String::from_utf8(signed.to_bytes().unwrap()).unwrap();
    let mut r = record(2, "attest");
    r.id = "01a12290-0000-7000-8000-000000000004".into();
    r.project = project.into();
    r.machine = "01a12290-0000-7000-8000-000000000005".into();
    r.actor.uid = u32::MAX;
    r.actor.name = "person:ann@example.com".into();
    r.data = json!({"request":text,"request_id":id,"request_digest":hex::encode(Sha256::digest(text.as_bytes())),"person_key":hex::encode(key.verifying_key().to_bytes()),"attestation":Attestation { format:1,attestation_id:r.id.clone(),subject:Subject{kind:"state".into(),id:subject.into()},change_id:None,kind:"approval".into(),payload_digest:hex::encode(Sha256::digest(signed.r.get().as_bytes())),payload_ref:None,signer:r.actor.name.clone(),time:r.time }});
    validation::validate(&r).unwrap();
    let mut changed = r.clone();
    changed.data["attestation"]["subject"]["id"] = json!(r.id);
    assert!(validation::validate(&changed).is_err());
    changed = r.clone();
    changed.actor.name = "person:bob@example.com".into();
    assert!(validation::validate(&changed).is_err());
    changed = r.clone();
    changed.data["person_key"] = json!(hex::encode(ed25519_dalek::SigningKey::from_bytes(&[12; 32]).verifying_key().to_bytes()));
    assert!(validation::validate(&changed).is_err());
    changed = r.clone();
    changed.time = request.expires;
    changed.data["attestation"]["time"] = json!(changed.time);
    assert!(validation::validate(&changed).is_err());
}

#[test]
fn project_policy_perspective_requires_one_creator_and_keeps_foreign_config_namespaced() {
    let mut creator = record(1,"project");
    creator.data = json!({"id":"project","config":{"person.keys":{"owner@example.com":"owner-key"},"protect":{"main":{}}}});
    let mut executor = record(1,"config");
    executor.id = "executor-config".into(); executor.machine = "b".into(); executor.time = -10;
    executor.data = json!({"config":{"person.keys":{"other@example.com":"foreign-key"},"protect":{}}});
    let records = vec![executor.clone(),creator.clone()];
    let perspective = project_perspective("project", &records).unwrap();
    assert_eq!(perspective,"a");
    let model = Project::from_records("project",records,&perspective).unwrap();
    assert_eq!(model.config["person.keys"],creator.data["config"]["person.keys"]);
    assert_eq!(model.config["protect"],json!({"main":{}}));
    assert_eq!(model.config["person.keys@b"],executor.data["config"]["person.keys"]);
    assert!(project_perspective("project",[&executor]).is_err());
    assert!(project_perspective("wrong",[&creator]).is_err());
    assert_eq!(project_perspective("project",[&creator,&creator]).unwrap(),"a");
    let mut other_creator = creator.clone(); other_creator.id = "another-project-record".into();
    assert!(project_perspective("project",[&creator,&other_creator]).is_err());
    other_creator.machine = "b".into();
    assert!(project_perspective("project",[&creator,&other_creator]).is_err());
    other_creator.data["id"] = json!("wrong");
    assert!(project_perspective("project",[&other_creator]).is_err());
}

#[test]
fn project_access_mode_and_epoch_metadata_follow_signed_order_without_keys() {
    use layr_store::epochs::Mode;
    let mut first = record(1, "project.key");
    first.data = json!({"epoch":"first","mode":"private","readers_hash":"a".repeat(64),"recipient":"never-emit-recipient","readers":{"person":"never-emit-reader"},"identity":"never-emit-identity"});
    let mut opened = record(2, "project.mode");
    opened.data = json!({"mode":"public"});
    let mut second = record(3, "project.key");
    second.data = json!({"epoch":"second","mode":"private","readers_hash":"b".repeat(64)});
    let mut closed = record(4, "project.mode");
    closed.data = json!({"mode":"private","epoch":"second","reason":"audience closed"});
    let project = fold(vec![closed, opened, second, first]);
    assert_eq!(project.access_mode(), Mode::Private);
    let epochs = project.key_epochs();
    assert_eq!(epochs[0]["id"], "first");
    assert_eq!(epochs[1]["number"], 2);
    assert_eq!(epochs[1]["reason"], "audience closed");
    assert!(!serde_json::to_string(&epochs).unwrap().contains("never-emit"));
}
