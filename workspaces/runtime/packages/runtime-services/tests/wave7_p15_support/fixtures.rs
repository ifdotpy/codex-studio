//! Reuse monitor seeds from either test crate; the oracle lives in services.
#[path = "../monitors/fixtures.rs"]
mod base;
pub use base::{events, seed, store, tempdir};

pub fn corpus(root: &std::path::Path) -> Vec<serde_json::Value> {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    if manifest
        .file_name()
        .is_some_and(|name| name == "runtime-services")
    {
        return base::corpus(root);
    }
    let services = manifest.join("../runtime-services");
    let output = std::process::Command::new(crate::prepared_python_support::prepared_python(
        &services.join("../../apps/server/src"),
    ))
    .arg("-B")
    .arg(services.join("tests/monitors/oracle.py"))
    .arg(services.join("../../apps/server/src"))
    .arg(root)
    .output()
    .expect("Python monitor writer");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python monitor corpus")
}
