#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::sync::Mutex;

fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A stand-in for `mount_smbfs`: it asks for the password on its controlling
/// terminal, as `mount_smbfs` does, and saves its arguments and the password.
const PROMPTING_MOUNT: &str = r#"#!/bin/sh
out="$(dirname "$0")"
printf '%s\n' "$@" > "$out/argv"
printf 'Password for 127.0.0.1: ' > /dev/tty
stty -echo < /dev/tty
IFS= read -r pw < /dev/tty
stty echo < /dev/tty
printf '%s' "$pw" > "$out/password"
if [ "$pw" = "right" ]; then exit 0; fi
echo "mount_smbfs: server rejected the connection: Authentication error ($pw)"
exit 77
"#;

/// The password reaches `mount_smbfs` only on its controlling terminal: never
/// in an argument, and never in the failure text.
#[test]
fn the_password_is_typed_on_the_terminal_of_mount_smbfs() {
    let folder = tempfile::tempdir().unwrap();
    let program = folder.path().join("mount_smbfs");
    executable(&program, PROMPTING_MOUNT);
    let view = folder.path().join("view");
    typed_mount(
        &program,
        "//studio-view@127.0.0.1:50123/app",
        &view,
        "right",
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(folder.path().join("password")).unwrap(),
        "right"
    );
    let argv = fs::read_to_string(folder.path().join("argv")).unwrap();
    assert_eq!(
        argv.lines().collect::<Vec<_>>(),
        [
            "-o",
            MOUNT_OPTIONS,
            "//studio-view@127.0.0.1:50123/app",
            view.to_str().unwrap()
        ]
    );
    let error = typed_mount(
        &program,
        "//studio-view@127.0.0.1:50123/app",
        &view,
        "wrong",
    )
    .unwrap_err();
    assert_eq!(
        error,
        "The authenticated SMB mount failed: mount_smbfs: server rejected the connection: \
         Authentication error (***)"
    );
}

#[test]
fn the_mount_table_lists_only_smbfs_rows() {
    let output = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
                  //studio-view@127.0.0.1:50123/app on /Users/x/Studio/app (smbfs, nodev, nosuid, read-only, mounted by x)\n\
                  //user@nas/share on /Volumes/share (smbfs, nodev, nosuid, mounted by x)\n";
    let entries = parse_entries(output);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].source, "//studio-view@127.0.0.1:50123/app");
    assert_eq!(entries[0].path, "/Users/x/Studio/app");
    assert!(entries[0].read_only());
    assert!(!entries[1].read_only());
    let names = ["p1".to_owned(), "app".to_owned()];
    assert!(ours(&entries[0], &names));
    assert!(!ours(&entries[1], &names));
}

#[test]
fn a_share_name_is_the_project_folder_in_plain_ascii() {
    for (source, name) in [
        (Some("/Users/x/My App"), "My-App"),
        (Some("/Users/x/.hidden_"), "hidden"),
        (Some("/Users/x/global"), "project"),
        (Some("/Users/x/Проект"), "project"),
        (Some("/"), "project"),
        (None, "project"),
    ] {
        assert_eq!(share_hint(source), name, "{source:?}");
    }
    assert_eq!(
        share_hint(Some(&format!("/x/{}", "a".repeat(50)))).len(),
        40
    );
}

/// A guest that answers each method with its listed results in order.
struct ScriptedGuest {
    answers: Mutex<Vec<(&'static str, Result<Value, GuestError>)>>,
    calls: Mutex<Vec<String>>,
}

impl GuestRpc for ScriptedGuest {
    fn stream(
        &self,
        method: &str,
        _params: &Value,
        _request_id: Option<&str>,
        _timeout: Duration,
        _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        self.calls.lock().unwrap().push(method.to_owned());
        let mut answers = self.answers.lock().unwrap();
        let index = answers
            .iter()
            .position(|answer| answer.0 == method)
            .unwrap();
        answers.remove(index).1
    }
}

fn unavailable() -> GuestError {
    GuestError::Rejected {
        code: Some("unavailable".into()),
        detail: Some("The layr admin channel is unavailable".into()),
        uncertain: false,
    }
}

/// The layr admin channel restarts once after a boot that built new layr
/// tools; `project.ensure` waits for it and gives up on other errors.
#[test]
fn a_remount_waits_while_the_admin_channel_restarts() {
    let guest = ScriptedGuest {
        answers: Mutex::new(vec![
            ("project.ensure", Err(unavailable())),
            ("project.ensure", Ok(json!({"projectId": "p1"}))),
        ]),
        calls: Mutex::default(),
    };
    assert_eq!(
        ensure_project(&guest, "p1").unwrap()["projectId"],
        json!("p1")
    );
    assert_eq!(guest.calls.lock().unwrap().len(), 2);
    let guest = ScriptedGuest {
        answers: Mutex::new(vec![(
            "project.ensure",
            Err(GuestError::Rejected {
                code: Some("outcome_unknown".into()),
                detail: None,
                uncertain: true,
            }),
        )]),
        calls: Mutex::default(),
    };
    assert!(ensure_project(&guest, "p1").is_err());
    assert_eq!(guest.calls.lock().unwrap().len(), 1);
}

#[derive(Default)]
struct Table(Mutex<Vec<MountEntry>>);

impl ShareMountTool for Table {
    fn entries(&self) -> Result<Vec<MountEntry>, String> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn mount(&self, _source: &str, _destination: &Path, _password: &str) -> Result<(), String> {
        Err("no mount in this test".into())
    }

    fn unmount(&self, _path: &Path) -> Result<(), String> {
        Err("no unmount in this test".into())
    }
}

/// Another filesystem at the view folder stays, and a credential file that
/// others can read is refused.
#[test]
fn a_view_never_replaces_another_mount_or_uses_a_shared_credential() {
    let folder = tempfile::tempdir().unwrap();
    let studio = folder.path().join("Studio");
    let guest = ScriptedGuest {
        answers: Mutex::new(vec![("share.name", Ok(json!({"name": "app"})))]),
        calls: Mutex::default(),
    };
    let table = Table::default();
    table.0.lock().unwrap().push(MountEntry {
        source: "//user@nas/app".into(),
        path: studio.join("app").to_string_lossy().into_owned(),
        options: vec!["smbfs".into(), "read-only".into()],
    });
    let bridge = json!({"state": "listening", "port": 50123});
    let parts = Remount {
        state_dir: folder.path(),
        studio_root: &studio,
        guest: &guest,
        bridge: Some(&bridge),
        tool: &table,
    };
    let project = json!({"share": {"state": "ready"}});
    let error = mount_project(
        &parts,
        "p1",
        project.as_object().unwrap(),
        &"0".repeat(64),
        "app",
    )
    .unwrap_err();
    assert_eq!(
        error,
        "A different filesystem already uses the project share path."
    );

    use std::os::unix::fs::PermissionsExt;
    let saved = folder.path().join(CREDENTIAL_FILE);
    fs::write(&saved, json!({"password": "0".repeat(64)}).to_string()).unwrap();
    fs::set_permissions(&saved, fs::Permissions::from_mode(0o644)).unwrap();
    let error = credential(folder.path(), &guest).unwrap_err();
    assert!(error.contains("must be private"), "{error}");
}
