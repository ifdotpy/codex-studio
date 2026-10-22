use serde_json::json;
#[test]
fn bridge_source_fingerprint_and_generation_compatibility_are_conservative() {
    use studio_runtime_services::claude_bridge::{bridge_build, compatibility_hold};
    let root = tempfile::tempdir().expect("fixture");
    for (name, body) in [
        ("z.mjs", "last"),
        ("a.mjs", "first: café"),
        ("x.test.mjs", "ignored test"),
        ("ignore.js", "ignored extension"),
    ] {
        std::fs::write(root.path().join(name), body).expect("fixture source");
    }
    let sdk = root
        .path()
        .join("node_modules/@anthropic-ai/claude-agent-sdk");
    std::fs::create_dir_all(&sdk).expect("SDK fixture");
    std::fs::write(
        sdk.join("package.json"),
        r#"{"version":"1.0.0","name":"fixture"}"#,
    )
    .expect("SDK manifest");
    let expected = bridge_build(root.path()).expect("fingerprint");
    assert_eq!(bridge_build(root.path()).expect("fingerprint"), expected);
    std::fs::write(root.path().join("x.test.mjs"), "changed ignored test").expect("test edit");
    assert_eq!(bridge_build(root.path()).expect("fingerprint"), expected);
    let init = json!({"capabilities":{"claudeVersion":23,"claudeBuild":expected}});
    assert!(compatibility_hold(&init, &expected, &json!({}), &json!({})).is_none());
    assert!(
        compatibility_hold(
            &init,
            &expected,
            &json!({}),
            &json!({"permissionMode":"plan"})
        )
        .is_some()
    );
    assert!(
        compatibility_hold(
            &json!({"capabilities":{"claudeVersion":22,"claudeBuild":expected}}),
            &expected,
            &json!({}),
            &json!({})
        )
        .is_some()
    );
    std::fs::write(root.path().join("a.mjs"), "changed production source").expect("source edit");
    let changed = bridge_build(root.path()).expect("changed fingerprint");
    assert_ne!(changed, expected);
    assert!(compatibility_hold(&init, &changed, &json!({}), &json!({})).is_some());
}

#[test]
fn bridge_identity_changes_when_only_installed_sdk_version_changes() {
    use studio_runtime_services::claude_bridge::{bridge_build, compatibility_hold};
    let root = tempfile::tempdir().expect("fixture");
    std::fs::write(root.path().join("bridge.mjs"), "unchanged source").expect("source");
    let sdk = root
        .path()
        .join("node_modules/@anthropic-ai/claude-agent-sdk");
    std::fs::create_dir_all(&sdk).expect("SDK fixture");
    let manifest = sdk.join("package.json");
    std::fs::write(&manifest, r#"{"version":"1.0.0"}"#).expect("SDK manifest");
    let before = bridge_build(root.path()).expect("original identity");
    std::fs::write(&manifest, r#"{"version":"1.0.1"}"#).expect("SDK update");
    let after = bridge_build(root.path()).expect("updated identity");
    assert_ne!(
        before, after,
        "an SDK-only update must invalidate retained bridges"
    );
    let retained = json!({"capabilities":{"claudeVersion":23,"claudeBuild":before}});
    assert!(compatibility_hold(&retained, &after, &json!({}), &json!({})).is_some());
    std::fs::write(
        &manifest,
        r#"{"version":"1.0.1","description":"unrelated metadata"}"#,
    )
    .expect("metadata edit");
    assert_eq!(bridge_build(root.path()).expect("same SDK version"), after);
}

#[tokio::test]
async fn bridge_identity_owner_matches_real_initialize_response() {
    use tokio::io::AsyncReadExt;
    const CHECK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(15);
    use studio_runtime_services::claude_bridge::bridge_build;
    let bridge = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../providers/apps/claude-bridge");
    let state = tempfile::tempdir().expect("isolated bridge state");
    // Ask the real protocol caller, without a model request or native credentials.
    let expected = bridge_build(&bridge).expect("Rust identity owner");
    let observed = tokio::time::timeout(CHECK_DEADLINE, async {
        let mut child = tokio::process::Command::new("node")
            .args([
                "--input-type=module",
                "-e",
                r#"
import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { once } from 'node:events';
const HANG_GUARD_MS = 10_000;
const child = spawn(process.execPath, [process.argv[1], process.argv[2]], {
  stdio: ['pipe', 'pipe', 'inherit'], timeout: HANG_GUARD_MS,
});
const exited = once(child, 'exit');
child.stdin.write(JSON.stringify({id: 1, method: 'initialize', params: {}}) + '\n');
for await (const line of createInterface({input: child.stdout})) {
  const response = JSON.parse(line);
  if (response.id !== 1) continue;
  if (response.error) throw new Error(JSON.stringify(response.error));
  process.stdout.write(response.result.capabilities.claudeBuild);
  child.kill();
  break;
}
await exited;
"#,
            ])
            .arg(bridge.join("bridge.mjs"))
            .arg(state.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("Node bridge initialize");
        let mut output = Vec::new();
        child
            .stdout
            .take()
            .expect("identity pipe")
            .take(expected.len() as u64 + 1)
            .read_to_end(&mut output)
            .await
            .expect("bounded identity");
        assert!(child.wait().await.expect("bridge check exit").success());
        String::from_utf8(output).expect("UTF-8 identity")
    })
    .await
    .expect("bridge check deadline");
    assert_eq!(
        observed, expected,
        "the Rust identity owner and Node initialize consumer must stay in parity",
    );
}
