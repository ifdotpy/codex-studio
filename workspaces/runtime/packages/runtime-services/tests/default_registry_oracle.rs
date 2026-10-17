//! Python `codex_accounts.AccountStore` is the oracle for the registry a fresh
//! state gets on first start. Rust must write the same bytes and modes for the
//! same CODEX_HOME and `auth.json`.

#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use base64::Engine as _;
use serde_json::{Value, json};
use studio_runtime_services::account_driver::{create_default_registry, resolve_codex_home};

const PYTHON_FIRST_START: &str = r#"
import json, os, stat, sys
sys.path.insert(0, sys.argv[1])
from codex_accounts import AccountStore
from codex_state import codex_home
AccountStore(sys.argv[2])
path = os.path.join(sys.argv[2], "accounts", "registry.json")
with open(path, "rb") as handle:
    data = handle.read()
print(json.dumps({
    "codexHome": str(codex_home()),
    "bytes": data.decode("ascii"),
    "fileMode": stat.S_IMODE(os.stat(path).st_mode),
    "directoryMode": stat.S_IMODE(os.stat(os.path.dirname(path)).st_mode),
}))
"#;

fn token(claims: &Value) -> String {
    let encode = |value: &Value| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(value).expect("claims encode"))
    };
    format!(
        "{}.{}.signature",
        encode(&json!({"alg": "none"})),
        encode(claims)
    )
}

fn auth_cases() -> Vec<(&'static str, Option<Vec<u8>>)> {
    let chatgpt_claims = json!({
        "email": "fixture-\u{e9}@example.invalid",
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct-fixture",
            "chatgpt_plan_type": "plus",
        },
    });
    let profile_claims = json!({
        "https://api.openai.com/profile": {"email": "profile@example.invalid"},
    });
    vec![
        ("missing", None),
        (
            "api-key",
            Some(br#"{"OPENAI_API_KEY":"placeholder-not-a-credential"}"#.to_vec()),
        ),
        (
            "chatgpt",
            Some(
                serde_json::to_vec(&json!({"tokens": {
                    "id_token": token(&chatgpt_claims),
                    "access_token": "placeholder-access",
                }}))
                .expect("auth encodes"),
            ),
        ),
        (
            "tokens-without-access",
            Some(
                serde_json::to_vec(&json!({"tokens": {
                    "id_token": token(&profile_claims),
                    "account_id": "acct-token",
                }}))
                .expect("auth encodes"),
            ),
        ),
        ("empty-object", Some(b"{}".to_vec())),
        ("malformed", Some(b"{not json".to_vec())),
        ("not-an-object", Some(b"[]".to_vec())),
    ]
}

#[test]
fn rust_default_registry_matches_python_first_start_bytes_and_modes() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python_support::prepared_python(&source);
    let fixture = tempfile::Builder::new()
        .prefix("default-registry-oracle-")
        .tempdir()
        .expect("isolated fixture");
    for (name, auth) in auth_cases() {
        let case = fixture.path().join(name);
        let home = case.join("home");
        let codex_home = case.join("codex home");
        for directory in [&home, &codex_home, &case.join("cache"), &case.join("tmp")] {
            fs::create_dir_all(directory).expect("isolated directory creates");
        }
        if let Some(auth) = &auth {
            fs::write(codex_home.join("auth.json"), auth).expect("auth fixture writes");
        }

        let python_state = case.join("python-state");
        fs::create_dir_all(&python_state).expect("Python state creates");
        let mut command = Command::new(&python);
        command
            .arg("-c")
            .arg(PYTHON_FIRST_START)
            .arg(&source)
            .arg(&python_state)
            .env("HOME", &home)
            .env("CODEX_HOME", &codex_home)
            .env("XDG_CACHE_HOME", case.join("cache"))
            .env("TMPDIR", case.join("tmp"))
            .env("PYTHONDONTWRITEBYTECODE", "1");
        let output = prepared_python_support::output_with_timeout(
            &mut command,
            Path::new("default-registry-oracle"),
            prepared_python_support::PYTHON_ORACLE_TIMEOUT,
        )
        .unwrap_or_else(|error| panic!("{name}: {error}"));
        assert!(
            output.status.success(),
            "{name}: Python AccountStore failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let oracle: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");

        let resolved =
            resolve_codex_home(Some(codex_home.as_os_str()), &home).expect("CODEX_HOME resolves");
        assert_eq!(
            resolved.to_str(),
            oracle["codexHome"].as_str(),
            "{name}: codex_home() differs"
        );
        let rust_registry = case.join("rust-state/accounts/registry.json");
        fs::create_dir_all(case.join("rust-state")).expect("Rust state creates");
        assert_eq!(
            create_default_registry(&rust_registry, &resolved),
            Ok(true),
            "{name}: Rust creates the registry"
        );
        let rust_bytes = fs::read(&rust_registry).expect("Rust registry reads");
        assert_eq!(
            String::from_utf8_lossy(&rust_bytes),
            oracle["bytes"].as_str().expect("oracle bytes"),
            "{name}: registry bytes differ"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| {
                u64::from(fs::metadata(path).expect("metadata").permissions().mode() & 0o7777)
            };
            assert_eq!(
                Some(mode(&rust_registry)),
                oracle["fileMode"].as_u64(),
                "{name}: file mode differs"
            );
            assert_eq!(
                Some(mode(rust_registry.parent().expect("registry directory"))),
                oracle["directoryMode"].as_u64(),
                "{name}: directory mode differs"
            );
        }
    }
}
