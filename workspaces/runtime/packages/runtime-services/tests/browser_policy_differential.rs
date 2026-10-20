//! The Rust URL rules (`browser_policy`) against their owner, `vm-guest/browser_policy.py`.
//! Every case must give the same accepted fields or the same refusal message.

use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::process::Command;
use studio_runtime_services::browser_policy::{self, Checked};

#[path = "support/prepared_python.rs"]
mod prepared_python;

fn corpus() -> Vec<Value> {
    let aws = "https://oidc.us-east-1.amazonaws.com/authorize?response_type=code&client_id=abc&redirect_uri=http%3A%2F%2F127.0.0.1%3A49152%2Foauth%2Fcallback&state=6d1f7f7c-1b1a-4e8e-9a43-1c2c7a3f0d11&code_challenge_method=S256&scopes=sso%3Aaccount%3Aaccess&code_challenge=xyz";
    let kube = "https://id.example.com/auth?access_type=offline&client_id=kube&code_challenge=c&code_challenge_method=S256&redirect_uri=http%3A%2F%2Flocalhost%3A8000&response_type=code&scope=openid&state=Zm9vYmFy";
    let long_state = format!(
        "https://id.example.com/a?redirect_uri=http%3A%2F%2Flocalhost%3A8000&state={}",
        "s".repeat(700)
    );
    let long = format!("https://id.example.com/{}", "a".repeat(8200));
    let long_code = format!("https://device.example.com/?user_code={}", "C".repeat(90));
    let mut urls: Vec<String> = vec![
        kube.into(),
        aws.into(),
        "https://device.sso.us-east-1.amazonaws.com/?user_code=ABCD-EFGH".into(),
        "https://login.microsoftonline.com/common/oauth2/v2.0/authorize?client_id=x&redirect_uri=http%3A%2F%2Flocalhost%3A51515&scope=openid+profile&state=a%20b".into(),
        "https://github.com/login/device".into(),
        "https://id.example.com/a?redirect_uri=https%3A%2F%2Fapp.example.com%2Fcb&state=x".into(),
        "https://id.example.com/a?redirect_uri=http%3A%2F%2F%5B%3A%3A1%5D%3A9000%2Fcb&state=x&state=y".into(),
        "https://id.example.com/a?redirect_uri=http%3A%2F%2Flocalhost%3A80%2F".into(),
        "https://id.example.com/a?redirect_uri=https%3A%2F%2Flocalhost%3A8000%2F".into(),
        "https://id.example.com/a?redirect_uri=http%3A%2F%2Fuser%40localhost%3A8000%2F".into(),
        "https://id.example.com/a?redirect_uri=http%3A%2F%2Flocalhost%3Aabc%2F".into(),
        "https://id.example.com/a?redirect_uri=http%3A%2F%2Flocalhost%2F".into(),
        "https://id.example.com/a?client_id=&client_id=second&scope=&scopes=s1".into(),
        "https://id.example.com/a?user_code=&user_code=X".into(),
        long_state,
        long,
        long_code,
        "http://localhost:8000/".into(),
        "http://127.0.0.1:18000/start?x=1".into(),
        "http://[::1]:8000/".into(),
        "http://localhost/".into(),
        "http://localhost:80/".into(),
        "http://localhost:99999/".into(),
        "http://localhost:0/".into(),
        "http://example.com/".into(),
        "https://user:pw@id.example.com/".into(),
        "https://id.example.com@evil.example/".into(),
        "https://localhost/".into(),
        "https://a.localhost/".into(),
        "https://10.0.0.1/".into(),
        "https://192.168.1.1/".into(),
        "https://172.16.5.4/".into(),
        "https://169.254.169.254/".into(),
        "https://127.0.0.1/".into(),
        "https://0.0.0.0/".into(),
        "https://224.0.0.1/".into(),
        "https://240.0.0.1/".into(),
        "https://100.64.0.1/".into(),
        "https://8.8.8.8/".into(),
        "https://010.0.0.1/".into(),
        "https://[::1]/".into(),
        "https://[fe80::1]/".into(),
        "https://[fc00::1]/".into(),
        "https://[::ffff:8.8.8.8]/".into(),
        "https://[2606:4700::1111]/".into(),
        "https://[2001:db8::1]/".into(),
        "https://[ff02::1]/".into(),
        "https://ID.Example.COM:8443/Path".into(),
        "https://id.example.com:/x".into(),
        "https://id.example.com:abc/x".into(),
        "https://id.example.com:70000/x".into(),
        "https:///nohost".into(),
        "file:///etc/passwd".into(),
        "javascript:alert(1)".into(),
        "data:text/html,hi".into(),
        "ftp://files.example.com/".into(),
        "smb://share/".into(),
        "vscode://open".into(),
        "zoommtg://join".into(),
        "https://id.example.com/a b".into(),
        "https://id.example.com/a\tb".into(),
        "https://id.example.com/\u{7f}".into(),
        "".into(),
        "https://[::1/".into(),
        "https://[zz]/".into(),
        "https://id.example.com/a?redirect_uri=http%3A%2F%2F127.0.0.1%3A43210%2Fcb%3Fx%3D1&state=%E2%9C%93".into(),
        "https://id.example.com/a?a=%ZZ&state=%4".into(),
    ];
    urls.push(
        "http://127.0.0.1:47000/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A8000&state=t"
            .into(),
    );
    let mut cases = Vec::new();
    for url in &urls {
        for rule in ["classify", "provider"] {
            cases.push(json!({"rule": rule, "url": url}));
        }
    }
    cases.push(json!({"rule": "classify", "url": "http://127.0.0.1:47000/authorize?state=t", "httpProviders": ["127.0.0.1:47000"]}));
    cases.push(json!({"rule": "provider", "url": "http://127.0.0.1:47000/authorize?state=t", "httpProviders": ["127.0.0.1:47000"]}));
    cases.push(json!({"rule": "classify", "url": "http://localhost:47000/", "httpProviders": ["localhost:47000"]}));
    for (url, port, path, state) in [
        (
            "http://localhost:8000/?code=x&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        ("localhost:8000/?code=x&state=abc", 8000, "/", Some("abc")),
        ("127.0.0.1:8000?code=x&state=abc", 8000, "/", Some("abc")),
        (
            "http://[::1]:8000/?code=x&state=abc#frag",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://127.0.0.1:49152/oauth/callback?code=c&state=s",
            49152,
            "/oauth/callback",
            Some("s"),
        ),
        (
            "http://localhost:8000/?error=access_denied&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://localhost:8001/?code=x&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://localhost:8000/other?code=x&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        ("http://localhost:8000/?state=abc", 8000, "/", Some("abc")),
        (
            "http://localhost:8000/?code=&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://localhost:8000/?code=x&state=abd",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://localhost:8000/?code=x&state=abc&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        ("http://localhost:8000/?code=x", 8000, "/", Some("abc")),
        ("http://localhost:8000/?code=x&state=abc", 8000, "/", None),
        (
            "https://localhost:8000/?code=x&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://evil.example:8000/?code=x&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://user@localhost:8000/?code=x&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        (
            "http://localhost:8000/?code=x&state=a%20b",
            8000,
            "/",
            Some("a b"),
        ),
        (
            "http://localhost:8000/?code=x&state=a+b",
            8000,
            "/",
            Some("a b"),
        ),
        (
            "http://localhost:8000/?code=x y&state=abc",
            8000,
            "/",
            Some("abc"),
        ),
        ("", 8000, "/", Some("abc")),
    ] {
        cases.push(
            json!({"rule": "pasted", "url": url, "port": port, "path": path, "state": state}),
        );
    }
    cases
}

fn rust(case: &Value) -> Value {
    let providers: BTreeSet<String> = case
        .get("httpProviders")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let url = case["url"].as_str().expect("test step");
    let provider_fields = |value: browser_policy::Provider| {
        json!({"kind": "provider", "url": value.url, "host": value.host,
            "redirect": value.redirect.map(|r| json!({"host": r.host, "port": r.port, "path": r.path})),
            "clientId": value.client_id, "scope": value.scope, "state": value.state,
            "userCode": value.user_code})
    };
    let result = match case["rule"].as_str().expect("test step") {
        "classify" => browser_policy::classify(url, &providers).map(|checked| match checked {
            Checked::StartPage { url, port } => json!({"kind": "start", "url": url, "port": port}),
            Checked::Provider(value) => provider_fields(value),
        }),
        "provider" => browser_policy::provider(url, &providers).map(provider_fields),
        _ => browser_policy::pasted(
            url,
            case["port"].as_u64().expect("test step") as u16,
            case["path"].as_str().expect("test step"),
            case["state"].as_str(),
        )
        .map(|value| json!({"kind": "callback", "target": value.target, "state": value.state, "error": value.error})),
    };
    match result {
        Ok(value) => json!({"ok": value}),
        Err(error) => json!({"error": error.0}),
    }
}

#[test]
fn rust_url_rules_match_the_python_owner_on_every_case() {
    let cases = corpus();
    let directory = prepared_python::disposable_state_dir();
    let input = directory.path().join("corpus.json");
    std::fs::write(&input, serde_json::to_vec(&cases).expect("test step")).expect("test step");
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let script = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/browser_bridge/policy_oracle.py");
    let mut command = Command::new(python);
    command.arg("-I").arg(&script).arg(&input);
    let output = prepared_python::output_with_timeout(
        &mut command,
        &script,
        prepared_python::PYTHON_ORACLE_TIMEOUT,
    )
    .expect("run the Python URL rules");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<Value> = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    assert_eq!(expected.len(), cases.len());
    let mut accepted = 0;
    for (case, expected) in cases.iter().zip(expected) {
        let actual = rust(case);
        if expected.get("type") == Some(&json!("ValueError")) {
            // Python's urlsplit raises a plain ValueError; Rust must refuse too.
            assert!(
                actual.get("error").is_some(),
                "case {case}: Rust accepted {actual}"
            );
            continue;
        }
        let mut expected = expected;
        if let Some(object) = expected.as_object_mut() {
            object.remove("type");
        }
        accepted += usize::from(actual.get("ok").is_some());
        assert_eq!(actual, expected, "case {case}");
    }
    assert!(
        accepted > 20,
        "the corpus accepts real link shapes: {accepted}"
    );
}
