use serde::Deserialize;
use serde_json::json;
use studio_runtime_services::title_generation::{normalize_title, title_model, title_prompt};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TitleExpected {
    model: String,
    effort: Option<String>,
    first: Option<String>,
    recent: Vec<(String, String)>,
    prompt: String,
    title: String,
}

#[test]
fn title_helpers_match_real_python_on_python_written_item_rows() {
    let expected: Vec<TitleExpected> =
        serde_json::from_str(include_str!("title_generation/expected.json"))
            .expect("frozen title corpus");
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
    assert_eq!(
        cases.len(),
        expected.len(),
        "every title case has an expectation"
    );
    for ((agent, catalog), expected) in cases.into_iter().zip(expected) {
        let case = json!({"agent":agent,"catalog":catalog,"items":items,"providerOutput":"  “Build a weekly trends dashboard.”  "});
        assert_eq!(
            title_model(&case["catalog"], &case["agent"]).expect("Rust model"),
            (expected.model, expected.effort)
        );
        let item_rows = case["items"].as_array().expect("Python-written item rows");
        let prompt = title_prompt(&case["agent"], item_rows, item_rows).expect("Rust prompt");
        let first = item_rows
            .iter()
            .find(|item| item["role"] == "user")
            .and_then(|item| item["text"].as_str());
        assert_eq!(expected.first.as_deref(), first);
        assert_eq!(expected.recent.len(), 4);
        let rust_recent = item_rows
            .iter()
            .map(|item| {
                (
                    if item["role"] == "user" {
                        "User"
                    } else {
                        "Agent"
                    }
                    .to_owned(),
                    item["text"].as_str().expect("item text").to_owned(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(expected.recent, rust_recent);
        assert_eq!(prompt, expected.prompt);
        assert_eq!(
            normalize_title(case["providerOutput"].as_str().expect("provider output"))
                .expect("Rust title"),
            expected.title
        );
        assert_eq!(expected.first.as_deref(), Some("Plan a small dashboard"));
    }
}
