#![allow(clippy::unwrap_used)]

use super::{linux_unit, macos_label, macos_plist};
use crate::operator_cli::service_layout;
use std::path::Path;

#[test]
fn templates_use_the_shared_delivery_layout_and_escape_paths() {
    let layout = service_layout().unwrap();
    let root = Path::new("/release space/%$&<");
    let state = Path::new("/state space/%$&<");
    let unit = linux_unit(&layout, root, state).unwrap();
    assert!(unit.contains(&format!(" --port {}\n", layout.port)));
    assert!(unit.contains("ExecStart=\"/release space/%%$$&</current/bin/studio-runtime-server\""));
    assert!(unit.contains("Environment=\"CODEX_AGENTS_STATE_DIR=/state space/%%$&<\""));
    assert!(!unit.contains("python"));
    let label = macos_label(&layout, state);
    assert_ne!(label, macos_label(&layout, root));
    let plist = macos_plist(&layout, root, state, &label).unwrap();
    assert!(plist.contains("/release space/%$&amp;&lt;/current/bin/studio-runtime-server"));
    assert!(plist.contains("/state space/%$&amp;&lt;"));
    assert!(plist.contains(&format!("<string>{}</string>", layout.port)));
    assert!(!plist.contains("python"));
    assert!(!plist.contains("apps/server"));
    assert!(linux_unit(&layout, Path::new("/bad\npath"), state).is_err());
    assert!(macos_plist(&layout, root, Path::new("/bad\npath"), &label).is_err());
}

#[test]
fn launchctl_boundary_passes_the_exact_domain_and_reports_failure() {
    use std::{fs, os::unix::fs::PermissionsExt, process::ExitCode};
    let directory = tempfile::tempdir().unwrap();
    let launchctl = directory.path().join("launchctl");
    fs::write(
        &launchctl,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$0.args\"\n",
    )
    .unwrap();
    fs::set_permissions(&launchctl, fs::Permissions::from_mode(0o755)).unwrap();
    let layout = service_layout().unwrap();
    let domain = format!("gui/501/{}", macos_label(&layout, directory.path()));
    assert_eq!(
        super::manager(launchctl.to_str().unwrap(), &["print", &domain]).unwrap(),
        ExitCode::SUCCESS
    );
    assert_eq!(
        fs::read_to_string(directory.path().join("launchctl.args")).unwrap(),
        format!("print {domain}\n")
    );
    fs::write(&launchctl, "#!/bin/sh\nexit 1\n").unwrap();
    assert_eq!(
        super::manager(launchctl.to_str().unwrap(), &["enable", &domain]).unwrap(),
        ExitCode::FAILURE
    );
}

#[test]
fn default_local_server_port_copies_match_service_layout_owner() {
    let owner = service_layout().unwrap();
    let diagnostics = include_str!("../../../diagnostics/src/main.rs");
    let desktop = include_str!("../../../../../client/apps/desktop/backend.cjs");
    assert!(
        diagnostics.contains(&format!("const DEFAULT_PORT: u16 = {};", owner.port)),
        "diagnostics DEFAULT_PORT must match runtime-server/service-layout.json (default local server TCP port)"
    );
    assert!(
        desktop.contains(&format!("port = {}, env = process.env", owner.port)),
        "desktop ensureBackend default port must match runtime-server/service-layout.json (default local server TCP port)"
    );
}
