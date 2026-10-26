use crate::prepared_python_support as prepared_python;

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_services::title_generation::{normalize_title, title_model, title_prompt};

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
impl Oracle {
    fn start() -> Self {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python::prepared_python(&source);
        let mut child = Command::new(python)
            .arg("-B")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/title_generation/oracle.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start guarded title oracle");
        Self {
            input: child.stdin.take().expect("oracle input"),
            output: BufReader::new(child.stdout.take().expect("oracle output")),
            child,
        }
    }

    fn run(&mut self, value: &Value) -> Value {
        writeln!(self.input, "{value}").expect("write oracle input");
        self.input.flush().expect("flush oracle input");
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .expect("read oracle output");
        assert!(!line.is_empty(), "title oracle exited before replying");
        serde_json::from_str(&line).expect("title oracle JSON")
    }
}
impl Drop for Oracle {
    fn drop(&mut self) {
        self.input.write_all(b"null\n").unwrap_or(());
        self.input.flush().unwrap_or(());
        self.child.wait().expect("reap title oracle");
    }
}

#[test]
fn title_helpers_match_real_python_on_python_written_item_rows() {
    let mut oracle = Oracle::start();
    let items = vec![
        json!({"id":"i1","role":"user","text":"Plan a small dashboard","created":1.0}),
        json!({"id":"i2","role":"assistant","text":"Use a clear layout","created":2.0}),
        json!({"id":"i3","role":"user","text":"Include weekly trends","created":3.0}),
        json!({"id":"i4","role":"assistant","text":"I will add a chart","created":4.0}),
    ];
    let cases = [
        (
            json!({"id":"agent-codex","provider":"codex","accountKey":"default","model":"gpt-6"}),
            json!({"data":[
                {"model":"gpt-6","supportedReasoningEfforts":[{"reasoningEffort":"high"}]},
                {"model":"gpt-6-mini","supportedReasoningEfforts":[{"reasoningEffort":"low"}]},
                {"model":"hidden-mini","hidden":true}
            ]}),
        ),
        (
            json!({"id":"agent-luna","provider":"codex","accountKey":"default","model":"gpt-6"}),
            json!({"data":[
                {"model":"gpt-6"},
                {"model":"luna-fast"}
            ]}),
        ),
    ];
    for (agent, catalog) in cases {
        let case = json!({"agent":agent,"catalog":catalog,"items":items,"providerOutput":"  “Build a weekly trends dashboard.”  "});
        let expected = oracle.run(&case);
        assert_eq!(
            title_model(&case["catalog"], &case["agent"]).expect("Rust model"),
            (
                expected["model"].as_str().expect("model").to_owned(),
                expected["effort"].as_str().map(str::to_owned),
            )
        );
        let item_rows = case["items"].as_array().expect("Python-written item rows");
        let prompt = title_prompt(&case["agent"], item_rows, item_rows).expect("Rust prompt");
        let first = item_rows
            .iter()
            .find(|item| item["role"] == "user")
            .and_then(|item| item["text"].as_str());
        assert_eq!(expected["first"].as_str(), first);
        assert_eq!(expected["recent"].as_array().map(Vec::len), Some(4));
        let python_recent = expected["recent"]
            .as_array()
            .expect("Python recent messages");
        let rust_recent = item_rows
            .iter()
            .map(|item| {
                json!([
                    if item["role"] == "user" {
                        "User"
                    } else {
                        "Agent"
                    },
                    item["text"]
                ])
            })
            .collect::<Vec<_>>();
        assert_eq!(python_recent, &rust_recent);
        assert_eq!(prompt, expected["prompt"].as_str().expect("Python prompt"));
        assert_eq!(
            normalize_title(case["providerOutput"].as_str().expect("provider output"))
                .expect("Rust title"),
            expected["title"].as_str().expect("Python title")
        );
        assert_eq!(expected["first"].as_str(), Some("Plan a small dashboard"));
    }
}
