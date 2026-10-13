use std::path::PathBuf;
use std::process::Command;

use serde_json::{Value, json};
use studio_runtime_services::provider_binaries::{
    Provider, ProviderVersionInput, ProviderVersionSnapshot, SemanticVersion, VersionEvidence,
    VersionSource, bundle_digest, compare_versions, decide_provider_version, parse_native_version,
    parse_version, version_warning,
};

#[test]
fn provider_version_core_matches_the_python_oracle_corpus() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = manifest.join("../../apps/server/src");
    let corpus_path = manifest.join("tests/fixtures/provider-binary-versions.json");
    let script = manifest.join("tests/provider_binaries_oracle.py");
    let output = Command::new(python_executable())
        .arg(script)
        .arg(source)
        .arg(&corpus_path)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("run Python provider-version oracle");
    assert!(
        output.status.success(),
        "provider-version oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON output");
    let corpus: Value = serde_json::from_slice(
        &std::fs::read(corpus_path).expect("provider version corpus readable"),
    )
    .expect("provider version corpus JSON");

    let parsed = corpus["versions"]
        .as_array()
        .expect("version corpus")
        .iter()
        .map(|value| {
            value
                .as_str()
                .and_then(parse_version)
                .map(version_key_json)
                .unwrap_or(Value::Null)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        parsed,
        expected["parsed"]
            .as_array()
            .expect("oracle parsed array")
            .clone(),
        "parsed version corpus"
    );

    let comparisons = corpus["comparisons"]
        .as_array()
        .expect("comparison corpus")
        .iter()
        .map(|case| {
            let source = match case["source"].as_str() {
                Some("upstream") => VersionSource::Upstream,
                _ => VersionSource::Installed,
            };
            compare_versions(
                case["installed"].as_str(),
                case["available"].as_str(),
                source,
            )
            .map(|notice| {
                json!({
                    "availableVersion": notice.available_version,
                    "source": source_key(notice.source),
                })
            })
            .unwrap_or(Value::Null)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        comparisons,
        expected["comparisons"]
            .as_array()
            .expect("oracle comparison array")
            .clone(),
        "update notices"
    );

    let cases = corpus["diagnostics"].as_array().expect("diagnostic corpus");
    let diagnostics = cases
        .iter()
        .map(|case| {
            let provider = provider(case["provider"].as_str().expect("provider"));
            let snapshot = ProviderVersionSnapshot::new(ProviderVersionInput {
                account_key: case["accountKey"].as_str().expect("account key").to_owned(),
                provider,
                running_version: optional_string(case, "runningVersion"),
                installed_version: optional_string(case, "installedVersion"),
                configured_version: optional_string(case, "configuredVersion"),
                available_version: optional_string(case, "availableVersion"),
                upstream_version: optional_string(case, "upstreamVersion"),
                error: optional_string(case, "error"),
                running_note: optional_string(case, "runningNote"),
                at: case["at"].as_f64().expect("fixed timestamp"),
            })
            .expect("Python product record makes a valid typed snapshot");
            let decision = decide_provider_version(snapshot);
            let notices = decision
                .notices
                .iter()
                .map(|notice| {
                    json!({
                        "availableVersion": notice.available_version,
                        "source": source_key(notice.source),
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "id": decision.id,
                "accountKey": decision.account_key,
                "provider": decision.provider.key(),
                "status": status_key(decision.status),
                "runningVersion": decision.running_version,
                "installedVersion": decision.installed_version,
                "configuredVersion": decision.configured_version,
                "updateNotices": notices,
                "baseline": decision.baseline,
                "error": decision.error,
                "message": decision.message,
                "at": decision.at,
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        diagnostics,
        expected["diagnostics"]
            .as_array()
            .expect("oracle diagnostic array")
            .clone(),
        "provider diagnostics"
    );

    let warnings = cases
        .iter()
        .map(|case| {
            case["runningVersion"]
                .as_str()
                .and_then(|version| {
                    version_warning(
                        provider(case["provider"].as_str().expect("provider")),
                        version,
                        VersionEvidence::Running,
                    )
                })
                .map(|warning| {
                    json!({
                        "provider": warning.provider.key(),
                        "version": warning.version,
                        "baseline": warning.baseline,
                        "message": warning.message,
                    })
                })
                .unwrap_or(Value::Null)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        warnings,
        expected["warnings"]
            .as_array()
            .expect("oracle warning array")
            .clone(),
        "tested-version warnings"
    );

    let native_versions = corpus["nativeVersions"]
        .as_array()
        .expect("native version corpus")
        .iter()
        .map(|value| {
            value
                .as_str()
                .and_then(parse_native_version)
                .map(version_key_json)
                .unwrap_or(Value::Null)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        native_versions,
        expected["nativeVersions"]
            .as_array()
            .expect("oracle native versions")
            .clone(),
        "native executable version keys"
    );

    let hashes = corpus["hashes"]
        .as_array()
        .expect("bundle hash corpus")
        .iter()
        .map(|pair| {
            (
                pair[0].as_str().expect("binary name").to_owned(),
                pair[1].as_str().expect("binary hash").to_owned(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        bundle_digest(&hashes).expect("bundle hash serializes"),
        expected["bundleDigest"]
            .as_str()
            .expect("oracle bundle hash"),
        "canonical bundle digest"
    );
}

fn version_key_json(version: SemanticVersion) -> Value {
    let is_release = version.prerelease.is_none();
    let prerelease = version.prerelease.unwrap_or_else(|| {
        vec![studio_runtime_services::provider_binaries::PrereleasePart::Text(String::new())]
    });
    let parts = prerelease
        .into_iter()
        .map(|part| match part {
            studio_runtime_services::provider_binaries::PrereleasePart::Numeric(value) => {
                json!([0, value])
            }
            studio_runtime_services::provider_binaries::PrereleasePart::Text(value) => {
                json!([1, value])
            }
        })
        .collect::<Vec<_>>();
    json!([
        version.major,
        version.minor,
        version.patch,
        is_release,
        parts
    ])
}

fn provider(value: &str) -> Provider {
    match value {
        "codex" => Provider::Codex,
        "claude" => Provider::Claude,
        _ => unreachable!("corpus provider is a closed fixture enum"),
    }
}

fn source_key(source: VersionSource) -> &'static str {
    match source {
        VersionSource::Installed => "installed",
        VersionSource::Upstream => "upstream",
    }
}

fn status_key(status: studio_runtime_services::provider_binaries::CheckStatus) -> &'static str {
    match status {
        studio_runtime_services::provider_binaries::CheckStatus::Current => "current",
        studio_runtime_services::provider_binaries::CheckStatus::Outdated => "outdated",
        studio_runtime_services::provider_binaries::CheckStatus::Error => "error",
        studio_runtime_services::provider_binaries::CheckStatus::Unknown => "unknown",
    }
}

fn optional_string(case: &Value, field: &str) -> Option<String> {
    case[field].as_str().map(str::to_owned)
}

fn python_executable() -> PathBuf {
    std::env::var_os("CODEX_AGENTS_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("python3"))
}
