//! Mac folder sync rounds without a VM (docs/vm-mac-sync.md). The cases follow the
//! Mac-side tests of the last Python release (`test_codex_vm_mac_sync.py`), plus the
//! recovery after a guest answer that has no proven result.
#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use studio_runtime_services::{
    linux_vm_guest::{GuestError, GuestRpc},
    mac_sync::{
        self,
        round::{ProjectSync, Rules, scan, scan_paths, write_mac},
        schedule::{FileEvents, RoundRunner, Scheduler, Watcher, synced_projects},
    },
};
use studio_runtime_store::path_identity::CanonicalPath;

fn sha(data: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(data))
}

/// Runs once, under the guest lock, at the first call of its method.
type Hook = Box<dyn FnMut(&mut GuestState) + Send>;

#[derive(Default)]
struct GuestState {
    replies: VecDeque<Result<Value, GuestError>>,
    files: BTreeMap<(String, String), Option<Vec<u8>>>,
    hashes: BTreeMap<String, Value>,
    head: String,
    calls: Vec<(String, Value, Option<String>)>,
    hooks: Vec<(String, Hook)>,
}

/// Answers `sync.mac.apply` from a script and records every call, like the Python fake.
#[derive(Default)]
struct Guest(Mutex<GuestState>);

impl Guest {
    fn new(replies: Vec<Result<Value, GuestError>>) -> Self {
        let guest = Self::default();
        {
            let mut state = guest.0.lock().unwrap();
            state.replies = replies.into();
            state.head = "m9".into();
        }
        guest
    }
    fn file(&self, state: &str, rel: &str, data: Option<&[u8]>) {
        self.0
            .lock()
            .unwrap()
            .files
            .insert((state.into(), rel.into()), data.map(<[u8]>::to_vec));
    }
    fn hook(&self, method: &str, hook: impl FnMut(&mut GuestState) + Send + 'static) {
        self.0
            .lock()
            .unwrap()
            .hooks
            .push((method.to_owned(), Box::new(hook)));
    }
    fn push(&self, reply: Value) {
        self.0.lock().unwrap().replies.push_back(Ok(reply));
    }
    fn calls(&self, method: &str) -> Vec<(Value, Option<String>)> {
        self.0
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|(name, _, _)| name == method)
            .map(|(_, params, request)| (params.clone(), request.clone()))
            .collect()
    }
    fn applies(&self) -> Vec<Value> {
        self.calls("sync.mac.apply")
            .into_iter()
            .map(|(params, _)| params)
            .collect()
    }
    fn operations(&self) -> Vec<String> {
        self.calls("sync.mac.apply")
            .into_iter()
            .map(|(_, request)| request.unwrap())
            .collect()
    }
}

impl GuestRpc for Guest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        _timeout: Duration,
        _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        let mut state = self.0.lock().unwrap();
        state.calls.push((
            method.to_owned(),
            params.clone(),
            request_id.map(str::to_owned),
        ));
        if let Some(position) = state.hooks.iter().position(|(name, _)| name == method) {
            let (_, mut hook) = state.hooks.remove(position);
            hook(&mut state);
        }
        match method {
            "sync.mac.apply" => {
                let mut reply = state.replies.pop_front().expect("a scripted apply reply")?;
                if reply.as_object_mut().unwrap().remove("_exact").is_some() {
                    return Ok(reply);
                }
                // Like the guest: every requested base has a list of main's changes.
                let mut outbound = serde_json::Map::new();
                for base in params["since"].as_array().unwrap() {
                    outbound.insert(base.as_str().unwrap().into(), json!([]));
                }
                for (base, changes) in reply["outbound"].as_object().unwrap() {
                    outbound.insert(base.clone(), changes.clone());
                }
                reply["outbound"] = Value::Object(outbound);
                Ok(reply)
            }
            "sync.mac.read" => {
                let key = (
                    params["stateId"].as_str().unwrap().to_owned(),
                    params["path"].as_str().unwrap().to_owned(),
                );
                Ok(
                    match state.files.get(&key).cloned().unwrap_or(Some(Vec::new())) {
                        None => json!({"kind":"absent"}),
                        Some(data) => json!({
                            "kind":"file","mode":0o644,"eof":true,
                            "data":data_encoding::BASE64.encode(&data),
                        }),
                    },
                )
            }
            "sync.mac.head" => Ok(json!({"mainStateId": state.head})),
            "sync.mac.hashes" => {
                let items = state
                    .hashes
                    .get(params["stateId"].as_str().unwrap())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                Ok(json!({"stateId": params["stateId"], "items": items, "next": null}))
            }
            "sync.mac.handoff" => Ok(json!({
                "line":"mac-conflict","path":params["path"],"mainStateId":state.head,"stateId":"h1",
            })),
            "project.ensure" => Ok(json!({"stateId": state.head})),
            "upload.begin" | "upload.chunk" | "upload.commit" => Ok(json!({"ok":true})),
            other => panic!("unexpected guest method {other}"),
        }
    }
}

fn reply(main: &str, values: Value) -> Value {
    let mut reply = json!({
        "mainStateId":main,"macStateId":main,"applied":[],"conflicts":[],"merge":"none",
        "mergeConflicts":[],"mergeError":null,"outbound":{},
    });
    for (key, value) in values.as_object().unwrap() {
        reply[key] = value.clone();
    }
    reply
}

fn lost() -> GuestError {
    GuestError::Transport {
        message: "lost reply".into(),
        uncertain: true,
    }
}

fn unknown() -> GuestError {
    GuestError::Rejected {
        code: Some("outcome_unknown".into()),
        detail: Some("The request has no proven result; inspect the saved state".into()),
        uncertain: true,
    }
}

fn change(rel: &str, data: Option<&[u8]>, mode: u32) -> Value {
    match data {
        None => json!({"path":rel,"kind":"absent"}),
        Some(data) => {
            json!({"path":rel,"kind":"file","mode":mode,"size":data.len(),"sha256":sha(data)})
        }
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = CanonicalPath::existing(temp.path()).unwrap().to_path_buf();
        let (root, state) = (base.join("project"), base.join("state"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(state.join("layr-projects/p1")).unwrap();
        std::fs::write(
            state.join("layr-projects/p1/project.json"),
            json!({"projectId":"p1","source":root,"importStateId":"s1"}).to_string(),
        )
        .unwrap();
        Self {
            _temp: temp,
            root,
            state,
        }
    }
    fn directory(&self) -> PathBuf {
        self.state.join("layr-projects/p1/sync")
    }
    fn save(&self, name: &str, value: Value) {
        std::fs::create_dir_all(self.directory()).unwrap();
        std::fs::write(self.directory().join(name), value.to_string()).unwrap();
    }
    fn saved(&self, name: &str) -> Value {
        serde_json::from_slice(&std::fs::read(self.directory().join(name)).unwrap()).unwrap()
    }
    fn exists(&self, name: &str) -> bool {
        self.directory().join(name).exists()
    }
    fn project<'a>(&self, guest: &'a Guest) -> ProjectSync<'a> {
        ProjectSync::open(guest, &self.state, "p1").unwrap()
    }
    fn write(&self, rel: &str, data: &[u8]) {
        let path = self.root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.root.join(rel)).unwrap()
    }
    /// The manifest entry of the file as it is now, with the given base.
    fn entry(&self, rel: &str, base: &str) -> Value {
        let path = self.root.join(rel);
        let meta = std::fs::symlink_metadata(&path).unwrap();
        json!({
            "k":"file","s":meta.len(),"m":meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            "x":0o644,"h":sha(&std::fs::read(&path).unwrap()),"b":base,
        })
    }
    fn agreed(&self, rel: &str, data: &[u8], base: &str, mode: u32) -> Value {
        self.write(rel, data);
        let path = self.root.join(rel);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        json!({
            "k":"file","s":meta.len(),"m":meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            "x":mode,"h":sha(data),"b":base,
        })
    }
}

fn paths(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn entry_paths(entries: &Value) -> Vec<String> {
    entries
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["path"].as_str().unwrap().to_owned())
        .collect()
}

// Rules and scan.

#[test]
#[allow(
    clippy::disallowed_methods,
    reason = "a test fixture creates a Git repository"
)]
fn ignored_folders_and_dependency_folders_stay_but_ignored_files_sync() {
    let fixture = Fixture::new();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&fixture.root)
        .status()
        .unwrap();
    assert!(init.success());
    fixture.write(".gitignore", b"build/\n.env\n");
    for rel in [
        "src/a.py",
        ".env",
        "build/out.o",
        "node_modules/x/i.js",
        "src/__pycache__/a.pyc",
    ] {
        fixture.write(rel, rel.as_bytes());
    }
    let rules = Rules::new(&fixture.root).unwrap();
    let (entries, notices) = scan(&fixture.root, &rules, &BTreeMap::new(), None).unwrap();
    assert_eq!(
        entries.keys().cloned().collect::<BTreeSet<_>>(),
        paths(&[".gitignore", "src/a.py", ".env"])
    );
    assert!(notices.is_empty());
}

#[test]
fn unchanged_size_and_time_reuse_the_recorded_hash() {
    let fixture = Fixture::new();
    fixture.write("a.txt", b"one");
    let rules = Rules::new(&fixture.root).unwrap();
    let (first, _) = scan(&fixture.root, &rules, &BTreeMap::new(), None).unwrap();
    let mut recorded = first["a.txt"].clone();
    recorded.h = "recorded".into();
    let manifest = BTreeMap::from([("a.txt".to_owned(), recorded)]);
    let (again, _) = scan(&fixture.root, &rules, &manifest, None).unwrap();
    assert_eq!(again["a.txt"].h, "recorded");
    let file = std::fs::File::options()
        .write(true)
        .open(fixture.root.join("a.txt"))
        .unwrap();
    file.set_modified(std::time::UNIX_EPOCH + Duration::from_nanos(1))
        .unwrap();
    let (changed, _) = scan(&fixture.root, &rules, &manifest, None).unwrap();
    assert_eq!(changed["a.txt"].h, sha(b"one"));
}

#[test]
fn links_sync_as_links_and_special_files_are_reported() {
    let fixture = Fixture::new();
    fixture.write("target.txt", b"t");
    std::os::unix::fs::symlink("target.txt", fixture.root.join("link")).unwrap();
    std::os::unix::fs::symlink("/etc", fixture.root.join("folder-link")).unwrap();
    nix::unistd::mkfifo(
        &fixture.root.join("pipe"),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .unwrap();
    let (entries, notices) = scan(
        &fixture.root,
        &Rules::new(&fixture.root).unwrap(),
        &BTreeMap::new(),
        None,
    )
    .unwrap();
    assert_eq!(entries["link"].k, "link");
    assert_eq!(entries["folder-link"].k, "link");
    assert_eq!(entries["link"].h, sha(b"target.txt"));
    assert_eq!(
        notices,
        vec!["pipe: a special file does not sync".to_owned()]
    );
}

#[test]
fn writes_never_cross_a_symbolic_link_or_leave_the_folder() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), fixture.root.join("escape")).unwrap();
    for rel in ["escape/x.txt", "../x.txt"] {
        assert!(
            write_mac(&fixture.root, rel, "file", b"x", 0o644).is_err(),
            "{rel}"
        );
    }
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    let entry = write_mac(
        &fixture.root,
        "new/deep/file.sh",
        "file",
        b"#!/bin/sh\n",
        0o755,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (entry.h.as_str(), entry.x),
        (sha(b"#!/bin/sh\n").as_str(), 0o755)
    );
    let names = std::fs::read_dir(fixture.root.join("new/deep"))
        .unwrap()
        .map(|item| item.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["file.sh".to_owned()]);
}

// Rounds.

#[test]
fn a_lost_reply_repeats_the_same_operation() {
    let fixture = Fixture::new();
    fixture.write("a.txt", b"agreed");
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.entry("a.txt", "s1")}),
    );
    fixture.write("a.txt", b"changed on the Mac");
    let guest = Guest::new(vec![
        Err(lost()),
        Ok(reply("m2", json!({"applied":["a.txt"],"merge":"merged"}))),
    ]);
    let project = fixture.project(&guest);
    assert!(project.round(None).is_err());
    project.round(None).unwrap();
    let operations = guest.operations();
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0], operations[1]);
    let saved = fixture.saved("manifest.json");
    assert_eq!(saved["a.txt"]["h"], sha(b"changed on the Mac"));
    assert_eq!(saved["a.txt"]["b"], "m2");
}

#[test]
fn many_mac_deletions_pause_the_round() {
    let fixture = Fixture::new();
    let mut manifest = serde_json::Map::new();
    for index in 0..10 {
        let rel = format!("f{index}.txt");
        fixture.write(&rel, index.to_string().as_bytes());
        manifest.insert(rel.clone(), fixture.entry(&rel, "s1"));
    }
    fixture.save("manifest.json", Value::Object(manifest));
    for index in 0..5 {
        std::fs::remove_file(fixture.root.join(format!("f{index}.txt"))).unwrap();
    }
    let guest = Guest::new(vec![]);
    let status = fixture.project(&guest).round(None).unwrap();
    assert_eq!(status["state"], "paused");
    assert!(
        status["reason"]
            .as_str()
            .unwrap()
            .contains("5 files were deleted")
    );
    assert!(guest.0.lock().unwrap().calls.is_empty());
}

#[test]
fn main_changes_skip_a_file_the_user_edited_again() {
    let fixture = Fixture::new();
    fixture.write("a.txt", b"agreed");
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.entry("a.txt", "s1")}),
    );
    fixture.write("a.txt", b"edited during the round");
    let outbound = json!({"s1":[change("a.txt", Some(b"vm!"), 0o644)]});
    let guest = Guest::new(vec![Ok(reply("m2", json!({"outbound":outbound})))]);
    guest.file("m2", "a.txt", Some(b"vm!"));
    fixture.project(&guest).round(None).unwrap();
    assert_eq!(fixture.read("a.txt"), b"edited during the round");
}

#[test]
fn a_refused_merge_sends_its_paths_again_in_the_next_partial_round() {
    let fixture = Fixture::new();
    fixture.write("a.txt", b"agreed");
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.entry("a.txt", "s1")}),
    );
    fixture.write("a.txt", b"mac");
    let guest = Guest::new(vec![
        Ok(reply(
            "s1",
            json!({"merge":"refused","mergeError":"main moved"}),
        )),
        Ok(reply("m2", json!({"applied":["a.txt"],"merge":"merged"}))),
    ]);
    let project = fixture.project(&guest);
    let first = project.round(None).unwrap();
    assert_eq!(
        (first["state"].clone(), first["rescan"].clone()),
        (json!("waiting"), json!(["a.txt"]))
    );
    let second = project.round(Some(&BTreeSet::new())).unwrap();
    let sent = guest
        .applies()
        .iter()
        .map(|params| entry_paths(&params["entries"]))
        .collect::<Vec<_>>();
    assert_eq!(
        sent,
        vec![vec!["a.txt".to_owned()], vec!["a.txt".to_owned()]]
    );
    assert_eq!(
        (second["state"].clone(), second["rescan"].clone()),
        (json!("synced"), json!([]))
    );
}

#[test]
fn a_replayed_round_keeps_the_paths_it_did_not_read() {
    let fixture = Fixture::new();
    fixture.write("a.txt", b"agreed");
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.entry("a.txt", "s1")}),
    );
    fixture.write("a.txt", b"mac");
    let guest = Guest::new(vec![
        Err(lost()),
        Ok(reply("m2", json!({"applied":["a.txt"],"merge":"merged"}))),
    ]);
    let project = fixture.project(&guest);
    assert!(project.round(None).is_err());
    // A choice asks for b.txt to be read again while the lost round waits.
    fixture.save("status.json", json!({"rescan":["b.txt"]}));
    assert_eq!(
        project.round(Some(&BTreeSet::new())).unwrap()["rescan"],
        json!(["b.txt"])
    );
}

#[test]
fn a_base_the_guest_did_not_compare_stays_with_its_path() {
    let fixture = Fixture::new();
    for rel in ["old.txt", "new.txt"] {
        fixture.write(rel, rel.as_bytes());
    }
    fixture.save(
        "manifest.json",
        json!({"old.txt": fixture.entry("old.txt", "s0"), "new.txt": fixture.entry("new.txt", "s1")}),
    );
    let guest = Guest::new(vec![Ok(reply(
        "m2",
        json!({"merge":"none","outbound":{"s1":[]},"_exact":true}),
    ))]);
    fixture.project(&guest).round(None).unwrap();
    let saved = fixture.saved("manifest.json");
    assert_eq!(
        (saved["old.txt"]["b"].clone(), saved["new.txt"]["b"].clone()),
        (json!("s0"), json!("m2"))
    );
}

#[test]
fn confirmed_mac_deletions_go_to_the_vm() {
    let fixture = Fixture::new();
    let mut manifest = serde_json::Map::new();
    for index in 0..10 {
        let rel = format!("f{index}.txt");
        fixture.write(&rel, index.to_string().as_bytes());
        manifest.insert(rel.clone(), fixture.entry(&rel, "s1"));
    }
    fixture.save("manifest.json", Value::Object(manifest));
    for index in 0..5 {
        std::fs::remove_file(fixture.root.join(format!("f{index}.txt"))).unwrap();
    }
    let deleted = (0..5)
        .map(|index| format!("f{index}.txt"))
        .collect::<Vec<_>>();
    let guest = Guest::new(vec![Ok(reply(
        "m2",
        json!({"applied":deleted,"merge":"merged"}),
    ))]);
    let project = fixture.project(&guest);
    assert_eq!(project.round(None).unwrap()["deletes"], 5);
    assert_eq!(
        mac_sync::configure(&fixture.state, "p1", None, true).unwrap()["state"],
        "syncing"
    );
    let status = project.round(None).unwrap();
    assert_eq!(
        (status["state"].clone(), status["deletes"].clone()),
        (json!("synced"), json!(0))
    );
    let entries = guest.applies()[0]["entries"].clone();
    let absent = entries
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "absent")
        .map(|item| item["path"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(absent, deleted);
    assert!(!fixture.exists("allow-deletes.json"));
    assert!(mac_sync::configure(&fixture.state, "p1", None, true).is_err());
}

// Rounds that must neither lose a Mac edit nor a main change.

#[test]
fn a_refused_merge_keeps_the_unmerged_mac_edit() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.agreed("a.txt", b"agreed\n", "s1", 0o644)}),
    );
    fixture.write("a.txt", b"mac edit\n");
    let outbound = json!({"s1":[change("a.txt", Some(b"agent edit\n"), 0o644)]});
    let guest = Guest::new(vec![Ok(reply(
        "m2",
        json!({"merge":"refused","outbound":outbound}),
    ))]);
    guest.file("m2", "a.txt", Some(b"agent edit\n"));
    let status = fixture.project(&guest).round(None).unwrap();
    assert_eq!(fixture.read("a.txt"), b"mac edit\n");
    assert_eq!(
        fixture.saved("manifest.json")["a.txt"]["h"],
        sha(b"agreed\n")
    );
    assert_eq!(status["rescan"], json!(["a.txt"]));
}

#[test]
fn a_newer_mac_edit_during_a_handoff_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"app.txt": fixture.agreed("app.txt", b"agreed\n", "s1", 0o644)}),
    );
    fixture.write("app.txt", b"newer mac\n");
    let combined = change("app.txt", Some(b"combined\n"), 0o644);
    let guest = Guest::new(vec![Ok(reply(
        "m10",
        json!({"conflicts":["app.txt"],"outbound":{"s1":[combined.clone()],"m9":[combined]}}),
    ))]);
    guest.file("m10", "app.txt", Some(b"combined\n"));
    fixture.save(
        "held.json",
        json!({"app.txt":{"h":sha(b"handed\n"),"why":"agent","base":"m9","stateId":"h1"}}),
    );
    let status = fixture.project(&guest).round(None).unwrap();
    assert_eq!(fixture.read("app.txt"), b"newer mac\n");
    assert_eq!(status["conflicts"], json!(["app.txt"]));
    assert_eq!(
        fixture.saved("held.json")["app.txt"]["h"],
        sha(b"newer mac\n")
    );
}

#[test]
fn a_save_during_the_round_keeps_the_sent_content_as_the_next_base() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.agreed("a.txt", b"x\n", "s1", 0o644)}),
    );
    fixture.write("a.txt", b"x abc\n");
    let guest = Guest::new(vec![Ok(reply(
        "m2",
        json!({"applied":["a.txt"],"merge":"merged","outbound":{"s1":[change("a.txt", Some(b"x abc\n"), 0o644)]}}),
    ))]);
    let root = fixture.root.clone();
    guest.hook("sync.mac.apply", move |_| {
        std::fs::write(root.join("a.txt"), b"x abcd\n").unwrap();
    });
    let project = fixture.project(&guest);
    project.round(None).unwrap();
    assert_eq!(fixture.read("a.txt"), b"x abcd\n");
    let entry = fixture.saved("manifest.json")["a.txt"].clone();
    assert_eq!(
        (entry["h"].clone(), entry["b"].clone()),
        (json!(sha(b"x abc\n")), json!("m2"))
    );
    guest.push(reply("m3", json!({"applied":["a.txt"],"merge":"merged"})));
    project.round(None).unwrap();
    let sent = guest.applies().last().unwrap()["entries"].clone();
    assert_eq!(sent, json!([{"path":"a.txt","kind":"file","base":"m2"}]));
}

#[test]
fn a_mac_deletion_against_a_main_edit_is_a_conflict_that_stays() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({
            "a.txt": fixture.agreed("a.txt", b"agreed\n", "s1", 0o644),
            "b.txt": fixture.agreed("b.txt", b"b\n", "s1", 0o644),
        }),
    );
    std::fs::remove_file(fixture.root.join("a.txt")).unwrap();
    let edit = json!([change("a.txt", Some(b"agent edit\n"), 0o644)]);
    let guest = Guest::new(vec![
        Ok(reply(
            "m2",
            json!({"conflicts":["a.txt"],"outbound":{"s1":edit}}),
        )),
        Ok(reply("m2", json!({"outbound":{"s1":edit}}))),
    ]);
    guest.file("m2", "a.txt", Some(b"agent edit\n"));
    let project = fixture.project(&guest);
    let first = project.round(None).unwrap();
    assert_eq!(first["conflicts"], json!(["a.txt"]));
    assert_eq!(
        fixture.saved("held.json")["a.txt"],
        json!({"h":"absent","why":"conflict"})
    );
    let second = project.round(Some(&BTreeSet::new())).unwrap();
    assert!(!fixture.root.join("a.txt").exists());
    assert_eq!(second["conflicts"], json!(["a.txt"]));
    assert_eq!(guest.applies().last().unwrap()["entries"], json!([]));
}

#[test]
fn many_main_deletions_wait_for_a_confirmation() {
    let fixture = Fixture::new();
    let mut manifest = serde_json::Map::new();
    for index in 0..10 {
        let rel = format!("f{index}.txt");
        manifest.insert(
            rel.clone(),
            fixture.agreed(&rel, index.to_string().as_bytes(), "s1", 0o644),
        );
    }
    fixture.save("manifest.json", Value::Object(manifest));
    let outbound = json!({"s1": (0..5).map(|index| change(&format!("f{index}.txt"), None, 0)).collect::<Vec<_>>()});
    let guest = Guest::new(vec![
        Ok(reply("m2", json!({"outbound":outbound.clone()}))),
        Ok(reply("m2", json!({"outbound":outbound}))),
    ]);
    let project = fixture.project(&guest);
    let status = project.round(None).unwrap();
    assert_eq!(
        (status["state"].clone(), status["deletes"].clone()),
        (json!("paused"), json!(5))
    );
    assert!(
        status["reason"]
            .as_str()
            .unwrap()
            .contains("deleted in the VM")
    );
    assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 10);
    mac_sync::configure(&fixture.state, "p1", None, true).unwrap();
    let done = project.round(None).unwrap();
    assert_eq!(done["state"], "synced");
    assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 5);
}

#[test]
fn executable_bits_sync_both_ways() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({
            "run.sh": fixture.agreed("run.sh", b"#!/bin/sh\n", "s1", 0o644),
            "tool.sh": fixture.agreed("tool.sh", b"#!/bin/sh\n", "s1", 0o644),
        }),
    );
    std::fs::set_permissions(
        fixture.root.join("tool.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let outbound = json!({"s1":[change("run.sh", Some(b"#!/bin/sh\n"), 0o755)]});
    let guest = Guest::new(vec![Ok(reply(
        "m2",
        json!({"applied":["tool.sh"],"merge":"merged","outbound":outbound}),
    ))]);
    fixture.project(&guest).round(None).unwrap();
    let mode = std::fs::metadata(fixture.root.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(
        guest.applies().last().unwrap()["entries"],
        json!([{"path":"tool.sh","kind":"file","base":"s1","modeChanged":true}])
    );
}

#[test]
fn a_file_too_large_to_send_is_not_a_deletion() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"big.bin": fixture.agreed("big.bin", b"small", "s1", 0o644)}),
    );
    fixture.write("big.bin", b"now much larger");
    let guest = Guest::new(vec![Ok(reply("m2", json!({})))]);
    let status = fixture
        .project(&guest)
        .with_file_limit(8)
        .round(None)
        .unwrap();
    assert_eq!(guest.applies().last().unwrap()["entries"], json!([]));
    assert_eq!(
        status["notices"],
        json!(["big.bin: files over 256 MiB do not sync"])
    );
}

#[test]
fn links_to_any_target_sync_as_links() {
    let fixture = Fixture::new();
    std::os::unix::fs::symlink("/etc", fixture.root.join("outside")).unwrap();
    std::os::unix::fs::symlink("../elsewhere", fixture.root.join("up")).unwrap();
    fixture.save("manifest.json", json!({}));
    let guest = Guest::new(vec![Ok(reply(
        "m2",
        json!({"applied":["outside","up"],"merge":"merged"}),
    ))]);
    fixture.project(&guest).round(None).unwrap();
    let entries = guest.applies().last().unwrap()["entries"].clone();
    let mut kinds = entries
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["path"].as_str().unwrap().to_owned(),
                item["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect::<Vec<_>>();
    kinds.sort();
    assert_eq!(
        kinds,
        vec![
            ("outside".to_owned(), "link".to_owned()),
            ("up".to_owned(), "link".to_owned())
        ]
    );
    let written = write_mac(&fixture.root, "up", "link", b"../other", 0)
        .unwrap()
        .unwrap();
    assert_eq!(written.k, "link");
    assert_eq!(
        std::fs::read_link(fixture.root.join("up")).unwrap(),
        Path::new("../other")
    );
}

#[test]
fn a_refused_operation_is_not_repeated() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.agreed("a.txt", b"agreed\n", "s1", 0o644)}),
    );
    fixture.write("a.txt", b"mac\n");
    let guest = Guest::new(vec![
        Err(GuestError::Rejected {
            code: Some("invalid_params".into()),
            detail: Some("invalid".into()),
            uncertain: false,
        }),
        Ok(reply("m2", json!({"applied":["a.txt"],"merge":"merged"}))),
    ]);
    let project = fixture.project(&guest);
    assert!(project.round(None).is_err());
    assert!(!fixture.exists("pending.json"));
    project.round(None).unwrap();
    let operations = guest.operations();
    assert_ne!(operations[0], operations[1]);
}

// A guest answer without a proven result (registered difference from Python, which
// replayed the same operation forever).

/// The igor-mbp case of 2026-10-13, with files: a replayed operation that sent
/// changes, of which the guest applied only a part before its receipt lost the result.
#[test]
fn an_unknown_guest_outcome_ends_the_replay_and_loses_no_file() {
    let fixture = Fixture::new();
    let manifest = json!({
        "a.txt": fixture.agreed("a.txt", b"a0\n", "s1", 0o644),
        "b.txt": fixture.agreed("b.txt", b"b0\n", "s1", 0o644),
        "c.txt": fixture.agreed("c.txt", b"c0\n", "s1", 0o644),
        "d.txt": fixture.agreed("d.txt", b"d0\n", "s1", 0o644),
        "e.txt": fixture.agreed("e.txt", b"e0\n", "s1", 0o644),
    });
    fixture.save("manifest.json", manifest);
    fixture.write("a.txt", b"a1\n");
    fixture.write("b.txt", b"b1\n");
    std::fs::remove_file(fixture.root.join("c.txt")).unwrap();
    fixture.write("d.txt", b"d1\n");
    let guest = Guest::new(vec![Err(lost()), Err(unknown())]);
    let project = fixture.project(&guest);
    // The first send loses its reply; the replay of the same operation has no proven result.
    assert!(project.round(None).is_err());
    let dropped = guest.operations()[0].clone();
    let failed = project.round(None);
    assert!(failed.is_err());
    assert_eq!(guest.operations(), vec![dropped.clone(), dropped.clone()]);
    let status = fixture.saved("status.json");
    assert_eq!(status["state"], "error");
    assert!(
        status["reason"]
            .as_str()
            .unwrap()
            .contains("outcome_unknown")
    );
    assert!(!fixture.exists("pending.json"));
    // Before the next round: main took a1 from the dropped operation, and an agent
    // changed d.txt in main. The Mac changed d.txt as well.
    {
        let mut state = guest.0.lock().unwrap();
        state.head = "m5".into();
        state.hashes.insert(
            "m5".into(),
            json!({
                "a.txt": {"kind":"file","mode":0o644,"size":3,"sha256":sha(b"a1\n")},
                "b.txt": {"kind":"file","mode":0o644,"size":3,"sha256":sha(b"b0\n")},
                "c.txt": {"kind":"file","mode":0o644,"size":3,"sha256":sha(b"c0\n")},
                "d.txt": {"kind":"file","mode":0o644,"size":3,"sha256":sha(b"d2\n")},
                "e.txt": {"kind":"file","mode":0o644,"size":3,"sha256":sha(b"e0\n")},
            }),
        );
    }
    guest.push(reply(
        "m6",
        json!({
            "applied":["b.txt","c.txt"],"merge":"merged",
            "outbound":{"s1":[change("a.txt", Some(b"a1\n"), 0o644), change("d.txt", Some(b"d2\n"), 0o644)]},
        }),
    ));
    guest.file("m6", "d.txt", Some(b"d2\n"));
    let recovered = project.round(None).unwrap();
    let operations = guest.operations();
    assert_eq!(operations.len(), 3, "the replay loop ends");
    assert_ne!(
        operations[2], dropped,
        "the next round starts a new operation"
    );
    // Both sides were hashed again for the paths of the dropped operation.
    assert_eq!(guest.calls("sync.mac.hashes").len(), 1);
    // a.txt agrees, d.txt changed on both sides and waits; b.txt and the c.txt deletion go in.
    let mut sent = entry_paths(&guest.applies()[2]["entries"]);
    sent.sort();
    assert_eq!(sent, vec!["b.txt".to_owned(), "c.txt".to_owned()]);
    assert_eq!(recovered["state"], "conflict");
    assert_eq!(recovered["conflicts"], json!(["d.txt"]));
    // No file is lost or overwritten on the Mac.
    assert_eq!(fixture.read("a.txt"), b"a1\n");
    assert_eq!(fixture.read("b.txt"), b"b1\n");
    assert!(!fixture.root.join("c.txt").exists());
    assert_eq!(fixture.read("d.txt"), b"d1\n");
    assert_eq!(fixture.read("e.txt"), b"e0\n");
    let saved = fixture.saved("manifest.json");
    assert_eq!(saved["a.txt"]["h"], sha(b"a1\n"));
    assert_eq!(saved["d.txt"]["h"], sha(b"d0\n"));
    assert_eq!(
        fixture.saved("held.json")["d.txt"],
        json!({"h":sha(b"d1\n"),"why":"conflict"})
    );
    assert!(!fixture.exists("recover.json"));
    // The next quiet round sends nothing and keeps the conflict.
    guest.push(reply("m6", json!({})));
    let quiet = project.round(None).unwrap();
    assert_eq!(guest.applies()[3]["entries"], json!([]));
    assert_eq!(quiet["conflicts"], json!(["d.txt"]));
    assert_eq!(fixture.read("d.txt"), b"d1\n");
}

#[test]
fn an_unknown_outcome_of_a_head_only_round_needs_no_rehash() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.agreed("a.txt", b"a0\n", "s1", 0o644)}),
    );
    fixture.save(
        "pending.json",
        json!({"operation":"mac-b0b9d7d728304f14a37fb152d47cea7b","params":{"projectId":"p1","operationId":"mac-b0b9d7d728304f14a37fb152d47cea7b","since":["s1"],"entries":[]},"hashes":{}}),
    );
    let guest = Guest::new(vec![Err(unknown()), Ok(reply("m2", json!({})))]);
    let project = fixture.project(&guest);
    assert!(project.round(None).is_err());
    let status = project.round(None).unwrap();
    assert_eq!(status["state"], "synced");
    assert!(guest.calls("sync.mac.hashes").is_empty());
    let operations = guest.operations();
    assert_eq!(operations[0], "mac-b0b9d7d728304f14a37fb152d47cea7b");
    assert_ne!(operations[1], operations[0]);
}

// The first round of a project.

#[test]
fn the_first_round_agrees_on_the_import_state() {
    let fixture = Fixture::new();
    fixture.write("a.txt", b"edited after the import\n");
    let guest = Guest::new(vec![Ok(reply(
        "m2",
        json!({"applied":["a.txt"],"merge":"merged"}),
    ))]);
    guest.0.lock().unwrap().hashes.insert(
        "s1".into(),
        json!({"a.txt": {"kind":"file","mode":0o644,"size":9,"sha256":sha(b"imported\n")}}),
    );
    fixture.project(&guest).round(None).unwrap();
    assert_eq!(
        guest.applies()[0]["entries"],
        json!([{"path":"a.txt","kind":"file","base":"s1"}])
    );
}

// Choices that a later round completes.

fn conflict_fixture() -> (Fixture, Guest) {
    let fixture = Fixture::new();
    fixture.write("app.txt", b"mac\n");
    let guest = Guest::new(vec![]);
    guest.file("m9", "app.txt", Some(b"vm\n"));
    fixture.save(
        "manifest.json",
        json!({"app.txt":{"k":"file","s":7,"m":0,"x":0o644,"h":sha(b"agreed\n"),"b":"s1"}}),
    );
    fixture.save(
        "held.json",
        json!({"app.txt":{"h":sha(b"mac\n"),"why":"conflict"}}),
    );
    fixture.save(
        "status.json",
        json!({"state":"conflict","conflicts":["app.txt"],"mainStateId":"m9"}),
    );
    (fixture, guest)
}

#[test]
fn keep_mac_sends_the_mac_version_on_top_of_main() {
    let (fixture, guest) = conflict_fixture();
    guest.push(reply(
        "m10",
        json!({"applied":["app.txt"],"merge":"merged"}),
    ));
    mac_sync::resolve(&fixture.state, "p1", "app.txt", "mac", &guest).unwrap();
    assert_eq!(fixture.saved("status.json")["rescan"], json!(["app.txt"]));
    // The scheduler's next round reads the path again without a full scan.
    fixture
        .project(&guest)
        .round(Some(&BTreeSet::new()))
        .unwrap();
    assert_eq!(
        guest.applies()[0]["entries"],
        json!([{"path":"app.txt","kind":"file","base":"m9"}])
    );
    assert_eq!(fixture.read("app.txt"), b"mac\n");
    let manifest = fixture.saved("manifest.json")["app.txt"].clone();
    assert_eq!(
        (manifest["h"].clone(), manifest["b"].clone()),
        (json!(sha(b"mac\n")), json!("m10"))
    );
    assert_eq!(fixture.saved("status.json")["state"], "synced");
}

#[test]
fn ask_the_lead_and_take_main_after_the_merge() {
    let (fixture, guest) = conflict_fixture();
    let result = mac_sync::resolve(&fixture.state, "p1", "app.txt", "agent", &guest).unwrap();
    assert_eq!(result.response["stateId"], "h1");
    let status = fixture.saved("status.json");
    assert_eq!(
        (
            status["state"].clone(),
            status["withAgent"].clone(),
            status["conflicts"].clone()
        ),
        (json!("waiting"), json!(["app.txt"]), json!([]))
    );
    let project = fixture.project(&guest);
    // A quiet round leaves the Mac file alone while the lead works.
    guest.push(reply("m9", json!({"outbound":{"s1":[],"m9":[]}})));
    let quiet = project.round(Some(&BTreeSet::new())).unwrap();
    assert_eq!(
        (quiet["state"].clone(), quiet["sent"].clone()),
        (json!("waiting"), json!(0))
    );
    // The lead commits the combined file: the Mac takes main's version.
    guest.file("m10", "app.txt", Some(b"combined\n"));
    let combined = change("app.txt", Some(b"combined\n"), 0o644);
    guest.push(reply(
        "m10",
        json!({"merge":"none","outbound":{"s1":[combined.clone()],"m9":[combined]}}),
    ));
    let done = project.round(Some(&BTreeSet::new())).unwrap();
    let since = guest.applies().last().unwrap()["since"].clone();
    assert!(since.as_array().unwrap().contains(&json!("m9")));
    assert_eq!(fixture.read("app.txt"), b"combined\n");
    assert_eq!(
        (done["state"].clone(), done["withAgent"].clone()),
        (json!("synced"), json!([]))
    );
    assert_eq!(fixture.saved("held.json"), json!({}));
}

#[test]
fn a_new_mac_edit_ends_the_handoff_and_syncs_as_usual() {
    let (fixture, guest) = conflict_fixture();
    mac_sync::resolve(&fixture.state, "p1", "app.txt", "agent", &guest).unwrap();
    fixture.write("app.txt", b"mac again\n");
    guest.push(reply(
        "m9",
        json!({"conflicts":["app.txt"],"outbound":{"s1":[]}}),
    ));
    let status = fixture.project(&guest).round(None).unwrap();
    assert_eq!(
        entry_paths(&guest.applies().last().unwrap()["entries"]),
        vec!["app.txt".to_owned()]
    );
    assert_eq!(
        (status["conflicts"].clone(), status["withAgent"].clone()),
        (json!(["app.txt"]), json!([]))
    );
}

// Scheduling.

#[test]
fn only_folders_with_sync_enabled_take_part() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path();
    for (project, enabled) in [("a", None), ("b", Some(false)), ("c", Some(true))] {
        let folder = state.join("layr-projects").join(project);
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("project.json"),
            json!({"projectId":project,"source":state}).to_string(),
        )
        .unwrap();
        if let Some(enabled) = enabled {
            std::fs::create_dir_all(folder.join("sync")).unwrap();
            std::fs::write(
                folder.join("sync/config.json"),
                json!({"enabled":enabled}).to_string(),
            )
            .unwrap();
        }
    }
    std::fs::create_dir_all(state.join("layr-projects/d")).unwrap();
    std::fs::write(
        state.join("layr-projects/d/project.json"),
        json!({"projectId":"d","source":state.join("missing")}).to_string(),
    )
    .unwrap();
    let ids = synced_projects(state)
        .into_iter()
        .map(|project| project.project_id)
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["a".to_owned(), "c".to_owned()]);
    mac_sync::configure(state, "a", Some(false), false).unwrap();
    assert_eq!(synced_projects(state).len(), 1);
    let paused = mac_sync::sync_status(state, "a").unwrap().unwrap();
    assert_eq!(
        (paused["state"].clone(), paused["enabled"].clone()),
        (json!("paused"), json!(false))
    );
    assert!(mac_sync::sync_status(state, "missing").unwrap().is_none());
}

#[derive(Default)]
struct Events {
    queued: Option<(bool, Option<BTreeSet<String>>)>,
    roots: Vec<PathBuf>,
}

impl FileEvents for Events {
    fn watch(&mut self, roots: &[PathBuf]) {
        self.roots = roots.to_vec();
    }
    fn available(&mut self) -> bool {
        true
    }
    fn take(&mut self, _root: &Path) -> (bool, Option<BTreeSet<String>>) {
        self.queued.take().unwrap_or((false, Some(BTreeSet::new())))
    }
}

#[derive(Default)]
struct Rounds {
    seen: Mutex<Vec<Option<BTreeSet<String>>>>,
    fail: Option<String>,
}

impl RoundRunner for Rounds {
    fn round(
        &self,
        project: &ProjectSync<'_>,
        paths: Option<&BTreeSet<String>>,
    ) -> Result<Value, mac_sync::MacSyncError> {
        self.seen.lock().unwrap().push(paths.cloned());
        if let Some(message) = &self.fail {
            return Err(mac_sync::MacSyncError::Io(std::io::Error::other(
                message.clone(),
            )));
        }
        project.record_status(json!({"state":"synced","mainStateId":"s1"}))
    }
}

#[test]
fn a_stopped_vm_makes_the_round_wait_and_a_failure_stays_visible() {
    let fixture = Fixture::new();
    let guest = Guest::new(vec![]);
    let rounds = Rounds {
        fail: Some("guest refused".into()),
        ..Rounds::default()
    };
    let mut scheduler = Scheduler::new(Events::default());
    scheduler.tick(&fixture.state, &guest, &|| false, 1000.0, &rounds);
    assert!(!fixture.exists("status.json"));
    assert!(scheduler.tick(&fixture.state, &guest, &|| true, 1001.0, &rounds));
    let status = fixture.saved("status.json");
    assert_eq!(status["state"], "error");
    assert!(status["reason"].as_str().unwrap().contains("guest refused"));
}

#[test]
fn events_main_changes_and_quiet_projects() {
    let fixture = Fixture::new();
    let guest = Guest::new(vec![]);
    guest.0.lock().unwrap().head = "s1".into();
    let rounds = Rounds::default();
    let mut scheduler = Scheduler::new(Events::default());
    let mut refreshed = 0;
    let mut tick = |scheduler: &mut Scheduler<Events>, now: f64| {
        if scheduler.tick(&fixture.state, &guest, &|| true, now, &rounds) {
            refreshed += 1;
        }
    };
    tick(&mut scheduler, 1000.0); // first: a full scan
    scheduler.events_mut().queued = Some((true, Some(paths(&["a.txt"]))));
    tick(&mut scheduler, 1020.0); // file events: only their paths
    tick(&mut scheduler, 1040.0); // main unchanged and synced: nothing
    guest.0.lock().unwrap().head = "s2".into();
    tick(&mut scheduler, 1060.0); // main moved: a round without a Mac scan
    fixture.save("allow-deletes.json", json!({"count":9}));
    tick(&mut scheduler, 1080.0); // confirmed Mac deletions: a full scan at once
    assert_eq!(
        *rounds.seen.lock().unwrap(),
        vec![None, Some(paths(&["a.txt"])), Some(BTreeSet::new()), None]
    );
    // Only the first report changes what the sidebar shows; later rounds stay synced.
    assert_eq!(refreshed, 1);
}

// File events.

#[test]
fn a_partial_scan_reads_only_the_named_paths() {
    let fixture = Fixture::new();
    for rel in ["keep.txt", "gone.txt", "edit.txt"] {
        fixture.write(rel, rel.as_bytes());
    }
    let rules = Rules::new(&fixture.root).unwrap();
    let (full, _) = scan(&fixture.root, &rules, &BTreeMap::new(), None).unwrap();
    let manifest = full
        .into_iter()
        .map(|(rel, mut entry)| {
            entry.b = Some("s1".into());
            (rel, entry)
        })
        .collect::<BTreeMap<_, _>>();
    std::fs::remove_file(fixture.root.join("gone.txt")).unwrap();
    fixture.write("edit.txt", b"edited");
    fixture.write("keep.txt", b"changed but not named");
    fixture.write("new/deep/file.txt", b"new");
    let (current, _) = scan_paths(
        &fixture.root,
        &rules,
        &manifest,
        &paths(&["gone.txt", "edit.txt", "new"]),
    )
    .unwrap();
    assert_eq!(
        current.keys().cloned().collect::<BTreeSet<_>>(),
        paths(&["keep.txt", "edit.txt", "new/deep/file.txt"])
    );
    assert_eq!(current["keep.txt"].h, manifest["keep.txt"].h);
    assert_eq!(current["edit.txt"].h, sha(b"edited"));
    assert!(current["keep.txt"].b.is_none());
    let (nothing, _) = scan_paths(&fixture.root, &rules, &manifest, &BTreeSet::new()).unwrap();
    assert_eq!(
        nothing.keys().collect::<Vec<_>>(),
        manifest.keys().collect::<Vec<_>>()
    );
}

#[test]
fn the_watcher_collects_paths_per_folder_and_asks_for_rescans() {
    let fixture = Fixture::new();
    let other = tempfile::tempdir().unwrap();
    let other = CanonicalPath::existing(other.path()).unwrap().to_path_buf();
    let root = fixture.root.clone();
    let mut ready = vec![root.display().to_string(), other.display().to_string()];
    ready.sort();
    let helper = fixture.state.join("helper");
    let release = fixture.state.join("release");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\necho '{}'\nwhile [ ! -e '{}' ]; do sleep 0.05; done\necho '{}'\necho '{}'\necho '{}'\ncat > /dev/null\n",
            json!({"ready": ready}),
            release.display(),
            json!({"path": root.join("a.txt")}),
            json!({"path": root.join("sub/b.txt")}),
            json!({"rescan": other}),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut watcher = Watcher::new(helper, Duration::ZERO);
    watcher.watch(&[root.clone(), other.clone()]);
    assert!(watcher.available());
    assert!(watcher.wait_ready(Duration::from_secs(10)));
    // The first take after a start scans in full: nothing was watched before.
    assert_eq!(watcher.take(&root), (true, None));
    assert_eq!(watcher.take(&other), (true, None));
    std::fs::write(&release, b"").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while watcher.pending(&root) != Some(Some(paths(&["a.txt", "sub/b.txt"])))
        || watcher.pending(&other).is_none()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "file watcher fixture events"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        watcher.take(&root),
        (true, Some(paths(&["a.txt", "sub/b.txt"])))
    );
    assert_eq!(watcher.take(&other), (true, None));
    assert_eq!(watcher.take(&root), (false, Some(BTreeSet::new())));
    watcher.stop();
    assert!(!watcher.available());
}

// Review of 2026-10-24: names that a case- or form-insensitive Mac volume treats as one
// file, scan errors, saves during a write, and the late apply of a dropped operation.

/// Whether the fixture volume finds a file under another case and Unicode form, like the
/// default APFS volume of a Mac. Elsewhere the rename cases cannot happen.
fn insensitive(root: &Path) -> bool {
    let probe = root.join("Caf\u{e9}-Probe");
    std::fs::write(&probe, b"").unwrap();
    let found = root.join("cafe\u{301}-probe").exists();
    std::fs::remove_file(probe).unwrap();
    found
}

fn names(root: &Path) -> Vec<String> {
    let mut names = std::fs::read_dir(root)
        .unwrap()
        .map(|item| item.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    names
}

/// One main rename of `from` to `to` with unchanged content, reported in byte order as
/// layr lists it, and a quiet round after it that must send nothing.
fn main_rename_keeps_the_file(from: &str, to: &str) {
    let fixture = Fixture::new();
    if !insensitive(&fixture.root) {
        return;
    }
    fixture.save(
        "manifest.json",
        json!({from: fixture.agreed(from, b"read me\n", "s1", 0o644)}),
    );
    let mut changes = vec![
        change(to, Some(b"read me\n"), 0o644),
        change(from, None, 0),
    ];
    changes.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    let guest = Guest::new(vec![
        Ok(reply("m2", json!({"merge":"none","outbound":{"s1":changes}}))),
        Ok(reply("m2", json!({}))),
    ]);
    guest.file("m2", to, Some(b"read me\n"));
    let project = fixture.project(&guest);
    project.round(None).unwrap();
    assert_eq!(names(&fixture.root), vec![to.to_owned()], "{from} -> {to}");
    assert_eq!(fixture.read(to), b"read me\n");
    let manifest = fixture.saved("manifest.json");
    assert!(manifest.get(from).is_none() && manifest.get(to).is_some());
    project.round(None).unwrap();
    assert_eq!(guest.applies()[1]["entries"], json!([]), "no deletion goes to main");
}

#[test]
fn a_case_only_rename_in_main_moves_the_mac_file_and_loses_nothing() {
    main_rename_keeps_the_file("readme.md", "README.md");
    main_rename_keeps_the_file("README.md", "readme.md");
}

#[test]
fn a_unicode_form_rename_in_main_moves_the_mac_file_and_loses_nothing() {
    main_rename_keeps_the_file("cafe\u{301}.txt", "caf\u{e9}.txt");
}

#[test]
fn a_case_only_rename_on_the_mac_keeps_the_file() {
    let fixture = Fixture::new();
    if !insensitive(&fixture.root) {
        return;
    }
    fixture.save(
        "manifest.json",
        json!({"readme.md": fixture.agreed("readme.md", b"read me\n", "s1", 0o644)}),
    );
    std::fs::rename(fixture.root.join("readme.md"), fixture.root.join("README.md")).unwrap();
    let outbound = json!({"s1":[change("README.md", Some(b"read me\n"), 0o644), change("readme.md", None, 0)]});
    let guest = Guest::new(vec![
        Ok(reply(
            "m2",
            json!({"applied":["README.md","readme.md"],"merge":"merged","outbound":outbound}),
        )),
        Ok(reply("m2", json!({}))),
    ]);
    let project = fixture.project(&guest);
    project.round(None).unwrap();
    let mut sent = entry_paths(&guest.applies()[0]["entries"]);
    sent.sort();
    assert_eq!(sent, vec!["README.md".to_owned(), "readme.md".to_owned()]);
    assert_eq!(names(&fixture.root), vec!["README.md".to_owned()]);
    assert_eq!(fixture.read("README.md"), b"read me\n");
    project.round(None).unwrap();
    assert_eq!(guest.applies()[1]["entries"], json!([]));
}

#[test]
fn a_name_that_another_mac_file_holds_in_another_case_is_never_written() {
    let fixture = Fixture::new();
    if !insensitive(&fixture.root) {
        return;
    }
    fixture.save(
        "manifest.json",
        json!({"readme.md": fixture.agreed("readme.md", b"mac\n", "s1", 0o644)}),
    );
    // Main has both spellings, as a Linux line can; the Mac keeps its own file.
    let outbound = json!({"s1":[change("README.md", Some(b"other\n"), 0o644)]});
    let guest = Guest::new(vec![Ok(reply("m2", json!({"outbound":outbound})))]);
    guest.file("m2", "README.md", Some(b"other\n"));
    let status = fixture.project(&guest).round(None).unwrap();
    assert_eq!(names(&fixture.root), vec!["readme.md".to_owned()]);
    assert_eq!(fixture.read("readme.md"), b"mac\n");
    assert!(
        status["notices"]
            .as_array()
            .unwrap()
            .iter()
            .any(|notice| notice.as_str().unwrap().starts_with("README.md:"))
    );
}

#[test]
fn a_lost_broker_channel_keeps_the_operation_and_repeats_it() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.agreed("a.txt", b"agreed\n", "s1", 0o644)}),
    );
    fixture.write("a.txt", b"mac\n");
    // The guest service answers outcome_unknown when its broker channel closes; the
    // broker may still run the apply, and a repeat joins it.
    let guest = Guest::new(vec![
        Err(GuestError::Rejected {
            code: Some("outcome_unknown".into()),
            detail: Some("The admin connection closed before its reply".into()),
            uncertain: true,
        }),
        Ok(reply("m2", json!({"applied":["a.txt"],"merge":"merged"}))),
    ]);
    let project = fixture.project(&guest);
    assert!(project.round(None).is_err());
    assert!(fixture.exists("pending.json"));
    assert!(!fixture.exists("recover.json"));
    project.round(None).unwrap();
    let operations = guest.operations();
    assert_eq!(operations[0], operations[1]);
    assert_eq!(fixture.saved("manifest.json")["a.txt"]["h"], sha(b"mac\n"));
}

#[test]
fn a_late_apply_of_the_dropped_operation_is_read_before_the_next_operation() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.agreed("a.txt", b"base\n", "s1", 0o644)}),
    );
    fixture.write("a.txt", b"first\n");
    let guest = Guest::new(vec![Err(unknown())]);
    let project = fixture.project(&guest);
    assert!(project.round(None).is_err());
    // The user edits again; the guest still runs the apply of the dropped operation, and
    // main takes "first" only after recovery has read the head.
    fixture.write("a.txt", b"second\n");
    {
        let mut state = guest.0.lock().unwrap();
        state.head = "m5".into();
        state.hashes.insert(
            "m5".into(),
            json!({"a.txt": {"kind":"file","mode":0o644,"size":5,"sha256":sha(b"base\n")}}),
        );
    }
    guest.hook("sync.mac.hashes", |state| {
        state.head = "m6".into();
        state.hashes.insert(
            "m6".into(),
            json!({"a.txt": {"kind":"file","mode":0o644,"size":6,"sha256":sha(b"first\n")}}),
        );
    });
    guest.push(reply("m6", json!({})));
    let status = project.round(None).unwrap();
    // Changed on both sides since the agreed content: a conflict, and "second" is not
    // folded onto a stale base.
    assert_eq!(status["conflicts"], json!(["a.txt"]));
    assert_eq!(guest.applies().last().unwrap()["entries"], json!([]));
    assert_eq!(fixture.read("a.txt"), b"second\n");
}

#[test]
fn an_unreadable_folder_fails_the_round_and_deletes_nothing() {
    let fixture = Fixture::new();
    let mut manifest = serde_json::Map::new();
    for index in 0..8 {
        let rel = format!("sub/f{index}.txt");
        manifest.insert(rel.clone(), fixture.agreed(&rel, b"x", "s1", 0o644));
    }
    fixture.save("manifest.json", Value::Object(manifest));
    let sub = fixture.root.join("sub");
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o644)).unwrap();
    let guest = Guest::new(vec![Ok(reply("m2", json!({})))]);
    let result = fixture.project(&guest).round(None);
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err(), "{result:?}");
    assert!(guest.applies().is_empty());
    assert_eq!(fixture.saved("manifest.json").as_object().unwrap().len(), 8);
}

#[test]
fn a_mac_save_during_the_fetch_is_not_overwritten() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({"a.txt": fixture.agreed("a.txt", b"agreed\n", "s1", 0o644)}),
    );
    let outbound = json!({"s1":[change("a.txt", Some(b"agent\n"), 0o644)]});
    let guest = Guest::new(vec![Ok(reply("m2", json!({"outbound":outbound})))]);
    guest.file("m2", "a.txt", Some(b"agent\n"));
    let root = fixture.root.clone();
    guest.hook("sync.mac.read", move |_| {
        std::fs::write(root.join("a.txt"), b"saved during the fetch\n").unwrap();
    });
    fixture.project(&guest).round(None).unwrap();
    assert_eq!(fixture.read("a.txt"), b"saved during the fetch\n");
    assert_eq!(fixture.saved("manifest.json")["a.txt"]["h"], sha(b"agreed\n"));
}

#[test]
fn a_leftover_temporary_file_is_cleaned_and_never_sent() {
    let fixture = Fixture::new();
    fixture.save("manifest.json", json!({}));
    fixture.write(".a.txt.studio-sync-0123abcd", b"partial");
    let guest = Guest::new(vec![Ok(reply("m2", json!({})))]);
    fixture.project(&guest).round(None).unwrap();
    assert_eq!(guest.applies()[0]["entries"], json!([]));
    assert!(!fixture.root.join(".a.txt.studio-sync-0123abcd").exists());
}

#[test]
fn configuration_waits_for_a_running_round() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.directory()).unwrap();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(fixture.directory().join("round.lock"))
        .unwrap();
    let held = nix::fcntl::Flock::lock(lock, nix::fcntl::FlockArg::LockExclusive).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::park_timeout(Duration::from_millis(300));
        drop(held);
    });
    let started = std::time::Instant::now();
    let status = mac_sync::configure(&fixture.state, "p1", Some(false), false).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(250), "configure waited");
    assert_eq!(status["state"], "paused");
    release.join().unwrap();
}

#[test]
fn the_plan_reads_both_sides_and_writes_nothing() {
    let fixture = Fixture::new();
    fixture.save(
        "manifest.json",
        json!({
            "up.txt": fixture.agreed("up.txt", b"a\n", "s1", 0o644),
            "down.txt": fixture.agreed("down.txt", b"b\n", "s1", 0o644),
            "both.txt": fixture.agreed("both.txt", b"c\n", "s1", 0o644),
            "gone-mac.txt": fixture.agreed("gone-mac.txt", b"d\n", "s1", 0o644),
            "gone-main.txt": fixture.agreed("gone-main.txt", b"e\n", "s1", 0o644),
        }),
    );
    fixture.save(
        "pending.json",
        json!({"operation":"mac-b0b9d7d728304f14a37fb152d47cea7b","params":{"entries":[]},"hashes":{}}),
    );
    fixture.write("up.txt", b"a2\n");
    fixture.write("both.txt", b"c-mac\n");
    std::fs::remove_file(fixture.root.join("gone-mac.txt")).unwrap();
    fixture.write("new-mac.txt", b"n\n");
    let guest = Guest::new(vec![]);
    guest.0.lock().unwrap().hashes.insert(
        "m9".into(),
        json!({
            "up.txt": {"kind":"file","mode":0o644,"size":2,"sha256":sha(b"a\n")},
            "down.txt": {"kind":"file","mode":0o644,"size":3,"sha256":sha(b"b2\n")},
            "both.txt": {"kind":"file","mode":0o644,"size":6,"sha256":sha(b"c-vm\n")},
            "gone-mac.txt": {"kind":"file","mode":0o644,"size":2,"sha256":sha(b"d\n")},
            "new-vm.txt": {"kind":"file","mode":0o644,"size":2,"sha256":sha(b"v\n")},
        }),
    );
    let records = |fixture: &Fixture| {
        let mut files = std::fs::read_dir(fixture.directory())
            .unwrap()
            .map(|item| {
                let item = item.unwrap();
                (item.file_name(), std::fs::read(item.path()).unwrap())
            })
            .collect::<Vec<_>>();
        files.sort();
        (files, names(&fixture.root))
    };
    let before = records(&fixture);
    let plan = fixture.project(&guest).plan().unwrap();
    assert_eq!(records(&fixture), before, "the plan writes nothing");
    let methods = guest
        .0
        .lock()
        .unwrap()
        .calls
        .iter()
        .map(|(method, _, _)| method.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        methods,
        BTreeSet::from(["sync.mac.head".to_owned(), "sync.mac.hashes".to_owned()])
    );
    assert_eq!(plan["pending"]["operation"], "mac-b0b9d7d728304f14a37fb152d47cea7b");
    assert_eq!(plan["uploads"]["paths"], json!(["new-mac.txt", "up.txt"]));
    assert_eq!(plan["deletionsToMain"]["paths"], json!(["gone-mac.txt"]));
    assert_eq!(plan["macWrites"]["paths"], json!(["down.txt", "new-vm.txt"]));
    assert_eq!(plan["macDeletions"]["paths"], json!(["gone-main.txt"]));
    assert_eq!(plan["conflicts"]["paths"], json!(["both.txt"]));
    assert_eq!(plan["uploads"]["count"], 2);
}
