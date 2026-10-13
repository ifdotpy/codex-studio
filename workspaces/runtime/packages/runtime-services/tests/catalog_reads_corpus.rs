//! Differential witness against the real Python projections and Python-written task rows.

use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use studio_runtime_services::catalog_reads::{
    CapabilityAgentSnapshot, apply_capability_result, capabilities_base, managed_tool_catalog,
    skills_for_cwd,
};
use studio_runtime_services::thread_prepare::{CatalogSnapshot, CatalogSource, ToolDefinition};
use studio_runtime_store::{CapabilityReadsRepository, Connection};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.0));
    }
}

fn scratch() -> Scratch {
    let parent = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = tempfile::Builder::new()
        .prefix("catalog-reads-corpus-")
        .tempdir_in(parent)
        .expect("isolated scratch directory");
    let path = directory.keep();
    Scratch(path)
}

fn isolated_dir(root: &Path, name: &str) -> String {
    let path = root.join(name);
    std::fs::create_dir_all(&path).expect("isolated home directory");
    path.to_string_lossy().into_owned()
}

fn python_oracle(root: &Path, input: &Value) -> Value {
    let python = std::env::var_os("CODEX_AGENTS_PYTHON")
        .expect("set CODEX_AGENTS_PYTHON to the prepared isolated interpreter");
    let mut child = Command::new(python)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/catalog_reads/oracle.py"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("HOME", isolated_dir(root, "home"))
        .env("CODEX_HOME", isolated_dir(root, "codex-home"))
        .env("CLAUDE_CONFIG_DIR", isolated_dir(root, "claude-config"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Python oracle starts");
    serde_json::to_writer(child.stdin.as_mut().expect("oracle stdin"), input)
        .expect("oracle request writes");
    child
        .stdin
        .take()
        .expect("oracle stdin closes")
        .flush()
        .expect("oracle flush");
    let output = child.wait_with_output().expect("Python oracle exits");
    assert!(
        output.status.success(),
        "Python oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python oracle returns JSON")
}

#[test]
fn python_written_history_and_scripted_provider_match_catalog_projections() {
    let scratch = scratch();
    let database = scratch.0.join("canvas.sqlite3");
    let agent = json!({
        "id":"agent-a",
        "cwd":"/repo",
        "threadId":"thread-a",
        "model":"gpt-6-astra",
        "role":"orchestrator",
        "effort":"high",
    });
    let records = vec![
        json!({"agent":"agent-a","status":"completed","name":"old-tool","created":1}),
        json!({"agent":"agent-a","status":"running","name":"live-tool","created":2}),
        json!({"agent":"agent-a","status":"failed","type":"failed-type","created":3}),
        json!({"agent":"agent-a","status":null,"name":"null-status","created":4}),
        json!({"agent":"other","status":"completed","name":"foreign-tool","created":5,"payload":"x".repeat(100_000)}),
    ];
    let skill_response = json!({"data":[
        {"cwd":"/elsewhere","skills":[{"name":"ignored","path":"/ignored"}]},
        {"cwd":"/repo","errors":[{"message":"catalog warning"},"notice"],"skills":[
            {"name":"Beta","path":"/beta","description":"B"},
            {"name":"alpha","path":"/alpha"},
            {"name":"alpha","path":"/shadowed"},
            {"name":"disabled","path":"/disabled","enabled":false},
            {"name":"missing-path"}
        ]}
    ]});
    let mcp_response =
        json!({"servers":[{"name":"fixture-mcp","status":"connected"}],"nextCursor":"cursor-a"});
    let input = json!({
        "database":database.to_string_lossy(),
        "agent":agent,
        "records":records,
        "replies":{"skills/list":skill_response,"mcpServerStatus/list":mcp_response}
    });
    let expected = python_oracle(&scratch.0, &input);
    assert!(
        expected["capabilities"]["managed"].is_array(),
        "Python tool_definitions() supplies the managed response"
    );

    let skill_projection = skills_for_cwd("/repo", &expected["skillResponse"])
        .expect("Python provider response is valid");
    assert_eq!(skill_projection, expected["skills"]);

    for (name, malformed) in [
        (
            "errors",
            json!({"data":[{"cwd":"/repo","errors":null,"skills":[{"name":"hidden","path":"/hidden"}]}]}),
        ),
        ("skills", json!({"data":[{"cwd":"/repo","skills":null}]})),
    ] {
        let mut malformed_input = input.clone();
        malformed_input["database"] = json!(
            scratch
                .0
                .join(format!("malformed-{name}.sqlite3"))
                .to_string_lossy()
        );
        malformed_input["replies"]["skills/list"] = malformed.clone();
        let python_result = python_oracle(&scratch.0, &malformed_input);
        assert_eq!(python_result["skills"]["skills"], json!([]));
        assert!(
            !python_result["skills"]["errors"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let rust_result = skills_for_cwd("/repo", &malformed)
            .unwrap_or_else(|message| json!({"skills":[],"errors":[message]}));
        assert_eq!(rust_result["skills"], python_result["skills"]["skills"]);
        assert!(!rust_result["errors"].as_array().unwrap().is_empty());
    }

    let connection = Connection::open(&database).expect("Python-written DB opens read only");
    let observed = CapabilityReadsRepository::observed_names(&connection, "agent-a")
        .expect("selected-agent task names project");
    let snapshot = CapabilityAgentSnapshot::from_record(&agent).expect("valid agent snapshot");
    let catalog = CatalogSnapshot {
        source: CatalogSource::Current,
        // No production constructor for PreparationResources.catalog exists on this base.
        // Keep this witness to Rust's managed-tool serialization contract only.
        definitions: vec![ToolDefinition {
            name: "fixture-tool".into(),
            description: "fixture description".into(),
            input_schema: serde_json::from_value(json!({"type":"object"})).expect("fixture schema"),
            extra: serde_json::from_value(json!({"strict":true})).expect("fixture extension"),
        }],
        saved_thread_id: None,
        saved_digest: None,
    };
    let managed = managed_tool_catalog(&catalog).expect("full selected tool inventory");
    assert_eq!(
        managed,
        vec![json!({
            "name":"fixture-tool",
            "description":"fixture description",
            "inputSchema":{"type":"object"},
            "strict":true
        })]
    );
    let mut capabilities = capabilities_base("agent-a", &snapshot, managed, observed, 1234.5);
    apply_capability_result(&mut capabilities, "skills", &expected["skillResponse"]);
    apply_capability_result(&mut capabilities, "servers", &expected["mcpResponse"]);
    let mut python_capabilities = expected["capabilities"].clone();
    python_capabilities
        .as_object_mut()
        .expect("Python capabilities")
        .remove("managed");
    capabilities
        .as_object_mut()
        .expect("Rust capabilities")
        .remove("managed");
    assert_eq!(capabilities, python_capabilities);
    assert_eq!(expected["capabilities"], expected["cachedCapabilities"]);

    let calls = expected["calls"].as_array().expect("provider calls");
    assert_eq!(calls.len(), 3);
    assert!(
        calls.iter().any(|call| {
            call["method"] == "skills/list" && call["params"]["forceReload"] == false
        })
    );
    assert!(
        calls.iter().any(|call| {
            call["method"] == "skills/list" && call["params"]["forceReload"] == true
        })
    );
    assert!(
        calls
            .iter()
            .any(|call| call["method"] == "mcpServerStatus/list")
    );
}
