#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::sync::Mutex;

use super::*;

/// A guest whose broker keeps the files that `apply` writes, like `guest_update.py`.
struct Guest {
    /// Hashes the guest holds, by path.
    held: Mutex<BTreeMap<String, String>>,
    providers: Vec<&'static str>,
    /// Files of an apply that stopped before it finished: the plan lists them again.
    unfinished: Mutex<Vec<String>>,
    /// The apply answer is lost once (the broker stopped).
    lose_apply: Mutex<bool>,
    calls: Mutex<Vec<(String, Value, Option<String>)>>,
}

impl Guest {
    fn new(held: &GuestPayload, providers: Vec<&'static str>) -> Self {
        Self {
            held: Mutex::new(
                held.files
                    .iter()
                    .map(|(name, (digest, _))| (name.clone(), digest.clone()))
                    .collect(),
            ),
            providers,
            unfinished: Mutex::new(Vec::new()),
            lose_apply: Mutex::new(false),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(method, _, _)| method.clone())
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
        self.calls.lock().unwrap().push((
            method.to_owned(),
            params.clone(),
            request_id.map(str::to_owned),
        ));
        let held = self.held.lock().unwrap().clone();
        match method {
            "update.plan" => {
                let mut changed = params["files"]
                    .as_object()
                    .unwrap()
                    .iter()
                    .filter(|(name, digest)| {
                        held.get(*name) != digest.as_str().map(str::to_owned).as_ref()
                    })
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>();
                changed.extend(self.unfinished.lock().unwrap().clone());
                changed.sort();
                changed.dedup();
                Ok(json!({"changed": changed, "reboot": []}))
            }
            "provider.list" => Ok(json!({"providers": self.providers.iter()
                .map(|state| json!({"state": state})).collect::<Vec<_>>()})),
            "update.stage" => Ok(json!({"bytes": 1})),
            "update.apply" => {
                if std::mem::take(&mut *self.lose_apply.lock().unwrap()) {
                    // The broker wrote some files and stopped: they stay unfinished.
                    let name = params["files"].as_object().unwrap().keys().next().unwrap();
                    *self.unfinished.lock().unwrap() = vec![name.clone()];
                    return Err(GuestError::Transport {
                        message: "lost reply".into(),
                        uncertain: true,
                    });
                }
                let mut held = self.held.lock().unwrap();
                for (name, digest) in params["files"].as_object().unwrap() {
                    held.insert(name.clone(), digest.as_str().unwrap().to_owned());
                }
                self.unfinished.lock().unwrap().clear();
                Ok(json!({"stage": "restart_pending"}))
            }
            "update.status" => Ok(json!({"stage": "complete"})),
            "layr.health" => Ok(json!({"state": "ready"})),
            _ => Err(GuestError::InvalidRequest),
        }
    }
}

fn payload(version: &str) -> GuestPayload {
    let mut files = BTreeMap::new();
    for (name, bytes) in [
        ("vm/guest/service.py", format!("service {version}")),
        ("vm/guest/install.sh", "install".to_owned()),
    ] {
        files.insert(
            name.to_owned(),
            (sha256(bytes.as_bytes()), bytes.into_bytes()),
        );
    }
    GuestPayload { files }
}

const KEY: &str = "00000000000000000000000000000000000000000000000000000000000000ff";

fn run(guest: &Guest, version: &str, operation: &str) -> Result<GuestUpdate, GuestError> {
    update_guest(guest, &payload(version), KEY, operation, Duration::ZERO)
}

#[test]
fn an_equal_guest_version_does_nothing() {
    let guest = Guest::new(&payload("1"), vec!["running"]);
    assert_eq!(run(&guest, "1", "a").unwrap(), GuestUpdate::Current);
    assert_eq!(guest.methods(), ["update.plan"]);
}

#[test]
fn a_differing_guest_version_updates_when_idle() {
    let guest = Guest::new(&payload("1"), vec!["stopped", "completed"]);
    let outcome = run(&guest, "2", "a").unwrap();
    assert_eq!(
        outcome,
        GuestUpdate::Updated(vec!["vm/guest/service.py".into()])
    );
    assert_eq!(
        guest.methods(),
        [
            "update.plan",
            "provider.list",
            "update.stage",
            "update.apply",
            "update.status",
            "update.plan",
            "layr.health"
        ]
    );
    assert_eq!(run(&guest, "2", "b").unwrap(), GuestUpdate::Current);
}

#[test]
fn a_running_provider_defers_the_update() {
    for state in ["running", "starting", "lost"] {
        let guest = Guest::new(&payload("1"), vec![state]);
        assert_eq!(run(&guest, "2", "a").unwrap(), GuestUpdate::Deferred);
        assert!(
            !guest
                .methods()
                .iter()
                .any(|method| method.starts_with("update.s") || method == "update.apply")
        );
    }
}

#[test]
fn an_interrupted_apply_is_planned_again_with_a_new_operation() {
    let guest = Guest::new(&payload("1"), vec![]);
    *guest.lose_apply.lock().unwrap() = true;
    assert!(run(&guest, "2", "a").unwrap_err().uncertain());
    // The apply is not sent again under a guess.
    let applies = |guest: &Guest| {
        guest
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, _, _)| method == "update.apply")
            .map(|(_, _, id)| id.clone().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(applies(&guest).len(), 1);
    // The next attempt follows the broker's own list of unfinished files.
    assert!(matches!(
        run(&guest, "2", "b").unwrap(),
        GuestUpdate::Updated(_)
    ));
    let ids = applies(&guest);
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn the_signature_covers_the_operation_and_the_manifest() {
    let manifest = json!({"a": "b"});
    assert_ne!(
        signature(KEY, "x", &manifest),
        signature(KEY, "y", &manifest)
    );
    assert_ne!(
        signature(KEY, "x", &manifest),
        signature(KEY, "x", &json!({"a": "c"}))
    );
    // The value of `guest_update.signature(KEY, "x", {"a": "b"})`.
    assert_eq!(
        signature(KEY, "x", &manifest),
        "8263028f4800fce5d71da00ef09ddb8af60e4f82899c1b993dc901a5ef07fc90"
    );
}

#[test]
fn the_payload_holds_the_guest_files_the_install_uses() {
    let root = tempfile::tempdir().unwrap();
    let write = |name: &str, data: &str| {
        let path = root.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    };
    assert!(GuestPayload::read(root.path()).unwrap().is_none());
    write("vm/guest/install.sh", "i");
    write("vm/guest/service.py", "s");
    write("vm/guest/test_service.py", "t");
    write("vm/guest/workspace.py", "w");
    write("vm/guest/sign_in_links.json", "{}");
    write("vm/layr/manifest.json", r#"{"files":{"src/main.rs":"x"}}"#);
    write("vm/layr/src/main.rs", "fn main() {}");
    write(".agents/skills/codex-workspace/SKILL.md", "skill");
    write(".agents/skills/other/SKILL.md", "other");
    let payload = GuestPayload::read(root.path()).unwrap().unwrap();
    assert_eq!(
        payload.files.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            ".agents/skills/codex-workspace/SKILL.md",
            "vm/guest/install.sh",
            "vm/guest/service.py",
            "vm/layr/manifest.json",
            "vm/layr/src/main.rs"
        ]
    );
}
