//! Captured legacy decisions over immutable observations; no backend process is needed.
use serde_json::{Value, json};
use studio_runtime_services::turn_prepare::{
    ContextManifest, Event, NativeReceipt, PreparationSnapshot, PrepareError, ProviderInput,
    STUDIO_INPUT_BYTES, accept, compile,
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CapturedCase {
    case: Value,
    response: Value,
}
struct CapturedExpectations(Vec<CapturedCase>);
impl CapturedExpectations {
    fn new() -> Self {
        Self(crate::turn_fixtures::load(include_bytes!(
            "turn_prepare/expected.json"
        )))
    }
    fn call(&mut self, value: Value) -> Value {
        #[derive(serde::Deserialize)]
        #[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
        enum Request {
            Case {
                case: Value,
            },
            Normalize {
                segments: Option<Vec<Segment>>,
                value: Value,
            },
            ReceiptSequence {
                order: Vec<usize>,
            },
        }
        match serde_json::from_value(value).expect("typed fixture request") {
            Request::Case { case } => self
                .0
                .iter()
                .find(|row| {
                    row.case == case
                        || (case.get("id").is_some() && row.case.get("id") == case.get("id"))
                })
                .expect("captured preparation case")
                .response
                .clone(),
            Request::Normalize { segments, value } => {
                normalize_prepared(value, &segments.unwrap_or_default())
            }
            Request::ReceiptSequence { order } => {
                let rows: Vec<ReceiptSequence> = crate::turn_fixtures::load(include_bytes!(
                    "turn_prepare/receipt_sequences.json"
                ));
                rows.into_iter()
                    .find(|row| row.order == order)
                    .expect("captured receipt order")
                    .expected
            }
        }
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptSequence {
    order: Vec<usize>,
    expected: Value,
}
#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Segment {
    Literal(String),
    Json(Value),
    Token(String),
}
fn normalize_text(mut text: &str, segments: &[Segment]) -> Value {
    let mut result = Vec::new();
    for segment in segments {
        let (value, consumed) = match segment {
            Segment::Literal(expected) => {
                assert!(text.starts_with(expected), "literal text differs: {text:?}");
                (Segment::Literal(expected.clone()), expected.len())
            }
            Segment::Json(_) => {
                let mut stream = serde_json::Deserializer::from_str(text).into_iter::<Value>();
                let value = stream
                    .next()
                    .expect("generated JSON segment")
                    .expect("valid generated JSON");
                (Segment::Json(value), stream.byte_offset())
            }
            Segment::Token(expected) => {
                let token = text.get(..expected.len()).expect("complete context token");
                (Segment::Token(token.to_owned()), expected.len())
            }
        };
        result.push(value);
        text = &text[consumed..];
    }
    assert!(text.is_empty(), "unexpected trailing model text");
    json!(result)
}
fn normalize_prepared(value: Value, segments: &[Segment]) -> Value {
    #[derive(serde::Deserialize)]
    struct Observation {
        ok: bool,
        #[serde(default)]
        fulltext: Option<String>,
        #[serde(default)]
        transcript: Option<studio_runtime_services::turn_prepare::TranscriptItem>,
        #[serde(default)]
        provider_inputs: Vec<studio_runtime_services::turn_prepare::ProviderInput>,
        #[serde(default)]
        clock_segments: Vec<Segment>,
        #[serde(flatten)]
        remaining: std::collections::BTreeMap<String, Value>,
    }
    let observation: Observation =
        serde_json::from_value(value.clone()).expect("typed preparation observation");
    if !observation.ok {
        return value;
    }
    let transcript = observation.transcript.expect("successful transcript");
    let mut projected = json!(transcript);
    if let Some(fulltext) = observation.fulltext {
        assert!(transcript.truncated && fulltext.starts_with(&transcript.text));
        let fulltext = normalize_text(&fulltext, segments);
        projected["text"] =
            json!({"prefixOf":fulltext,"characters":transcript.text.chars().count()});
        let mut remaining = observation.remaining;
        remaining.insert("fulltext".into(), fulltext);
        finish_observation(
            remaining,
            projected,
            observation.provider_inputs,
            segments,
            &observation.clock_segments,
        )
    } else {
        assert!(!transcript.truncated);
        projected["text"] = normalize_text(&transcript.text, segments);
        let mut remaining = observation.remaining;
        remaining.insert("fulltext".into(), Value::Null);
        finish_observation(
            remaining,
            projected,
            observation.provider_inputs,
            segments,
            &observation.clock_segments,
        )
    }
}
fn finish_observation(
    mut remaining: std::collections::BTreeMap<String, Value>,
    transcript: Value,
    inputs: Vec<studio_runtime_services::turn_prepare::ProviderInput>,
    segments: &[Segment],
    clocks: &[Segment],
) -> Value {
    let mut segments = segments.to_vec();
    segments.extend_from_slice(clocks);
    let inputs: Vec<Value> = inputs
        .into_iter()
        .enumerate()
        .map(|(index, input)| {
            if index == 0 {
                let studio_runtime_services::turn_prepare::ProviderInput::Text { text, .. } = input
                else {
                    panic!("text first")
                };
                json!({"type":"text","text":normalize_text(&text,&segments)})
            } else {
                json!(input)
            }
        })
        .collect();
    remaining.insert("ok".into(), json!(true));
    remaining.insert("transcript".into(), transcript);
    remaining.insert("provider_inputs".into(), json!(inputs));
    json!(remaining)
}
fn difference(left: &Value, right: &Value, path: &str) -> Option<String> {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys()) {
                if !a.contains_key(key) || !b.contains_key(key) {
                    return Some(format!("{path}.{key}: absent versus present"));
                }
                if let Some(diff) = difference(&a[key], &b[key], &format!("{path}.{key}")) {
                    return Some(diff);
                }
            }
            None
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{path}: array lengths {} != {}", a.len(), b.len()));
            }
            a.iter()
                .zip(b)
                .enumerate()
                .find_map(|(i, (a, b))| difference(a, b, &format!("{path}[{i}]")))
        }
        _ if left == right => None,
        _ => Some(format!(
            "{path}: Rust {} != Python {}",
            preview(left),
            preview(right)
        )),
    }
}
fn preview(value: &Value) -> String {
    value.to_string().chars().take(300).collect()
}

#[test]
fn python_submission_compiler_corpus() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_prepare/cases.json")).expect("corpus data");
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let filter = std::env::var("TURN_PREPARE_CASE").ok();
    let mut oracle = CapturedExpectations::new();
    let mut count = 0;
    for case in cases.iter().filter(|c| {
        (full || c["fast"] == true)
            && filter
                .as_ref()
                .is_none_or(|s| c["id"].as_str().is_some_and(|id| id.contains(s)))
    }) {
        let fixture = oracle.call(json!({"command":"case","case":case}));
        let snapshot: PreparationSnapshot =
            serde_json::from_value(fixture["snapshot"].clone()).expect("typed snapshot");
        let rows: Vec<Event> =
            serde_json::from_value(fixture["events"].clone()).expect("typed events");
        let original = json!(snapshot);
        let mut aggregate_projection = false;
        let mut actual = match compile(&snapshot, &rows) {
            Ok(prepared) => {
                let provider_text: String = prepared
                    .provider_inputs
                    .iter()
                    .filter_map(|input| match input {
                        ProviderInput::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                aggregate_projection = prepared.studio_input.is_some()
                    || rows.iter().any(|row| {
                        matches!(
                            row.kind.as_str(),
                            "work_decision" | "radio_turn" | "followup"
                        ) && !provider_text.contains(&row.text)
                    });
                if aggregate_projection {
                    // Rust intentionally supersedes Python's unbounded context and
                    // decision bypass. Keep differential coverage for the unchanged
                    // receipt, manifest, evidence, and transcript-input contracts.
                    let user_bytes: usize = rows
                        .iter()
                        .filter(|row| {
                            row.kind == "user"
                                || (row.kind == "followup"
                                    && row
                                        .metadata
                                        .extra
                                        .get("senderId")
                                        .and_then(Value::as_str)
                                        .is_some_and(|id| !id.is_empty()))
                        })
                        .map(|row| {
                            assert!(provider_text.contains(&row.text));
                            row.text.len()
                        })
                        .sum();
                    assert!(
                        provider_text.len().saturating_sub(user_bytes) <= STUDIO_INPUT_BYTES,
                        "{}",
                        case["id"]
                    );
                    assert!(
                        provider_text.contains("studio-input:") || provider_text.contains("event:")
                    );
                    if let Some(full) = &prepared.studio_input {
                        assert!(
                            provider_text
                                .contains(&studio_runtime_store::sha256_hex(full.as_bytes()))
                        );
                    }
                }
                assert!(
                    fixture["expected"]["ok"] == true,
                    "{}: Rust prepared a submission but Python rejected it: {}",
                    case["id"],
                    fixture["expected"]["error"]
                );
                // Replay compilation without artifact observations: it must defer, returning
                // the exact immutable content needed by the adapter, never a ready request.
                if !prepared.artifact_intents.is_empty() {
                    let mut unresolved = snapshot.clone();
                    unresolved.artifact_paths.clear();
                    match compile(&unresolved, &rows) {
                        Err(PrepareError::ArtifactsRequired(intents)) => assert!(
                            json!(intents) == json!(prepared.artifact_intents),
                            "{}: artifact intent mismatch",
                            case["id"]
                        ),
                        _ => panic!("{}: unresolved evidence did not defer", case["id"]),
                    }
                }
                let receipt: NativeReceipt =
                    serde_json::from_value(fixture["receipt"].clone()).expect("receipt");
                let current: Option<ContextManifest> =
                    serde_json::from_value(fixture["current"].clone()).expect("delivered manifest");
                let delivery = accept(&prepared, &receipt, current.as_ref());
                let mut value = json!(prepared);
                value["ok"] = json!(true);
                value["delivery"] = json!(delivery);
                let mut stored = fixture["before"].clone();
                if let Some(manifest) = delivery.manifest_upsert {
                    let list = stored["manifests"].as_array_mut().expect("manifest rows");
                    list.retain(|m| m["epoch"] != json!(manifest.epoch));
                    list.push(json!(manifest));
                    list.sort_by_key(|m| m["epoch"].to_string());
                }
                if let Some(reminder) = delivery.reminder_insert {
                    let list = stored["reminders"].as_array_mut().expect("reminder rows");
                    if !list.contains(&json!(reminder)) {
                        list.push(json!(reminder));
                    }
                }
                if let Some(turn) = delivery.transcript_turn_id {
                    stored["transcript_turn_id"] = json!(turn);
                }
                value["stored"] = stored;
                value["clock_segments"] = fixture["clock_segments"].clone();
                value
            }
            Err(error) => {
                let encoded = json!(error);
                let mut result = json!({"ok":false,"error":encoded["kind"]});
                if encoded["kind"] == "InvalidJsonEvidence" {
                    result["location"] = encoded["artifacts"]["location"].clone();
                }
                result
            }
        };
        assert!(
            json!(snapshot) == original,
            "compiler mutated its input snapshot"
        );
        let segments = if aggregate_projection {
            // The legacy oracle validates literal text before normalizing
            // manifests. Budget projections have independent assertions above;
            // exclude their text before that validation, not after it.
            actual["provider_inputs"] = json!([{"type":"text","text":""}]);
            actual["fulltext"] = Value::Null;
            actual["transcript"]["text"] = json!("");
            actual["transcript"]["truncated"] = json!(false);
            actual["clock_segments"] = json!([]);
            json!([])
        } else {
            fixture["segments"].clone()
        };
        actual = oracle.call(json!({"command":"normalize","segments":segments,"value":actual}));
        let mut expected = fixture["expected"].clone();
        if aggregate_projection {
            for value in [&mut actual, &mut expected] {
                for key in ["provider_inputs", "fulltext", "studio_input"] {
                    value.as_object_mut().expect("compiled object").remove(key);
                }
                for key in ["text", "truncated"] {
                    value["transcript"]
                        .as_object_mut()
                        .expect("transcript")
                        .remove(key);
                }
                for patch in value["metadata_patches"]
                    .as_object_mut()
                    .expect("metadata patches")
                    .values_mut()
                {
                    patch
                        .as_object_mut()
                        .expect("metadata patch")
                        .remove("studioInput");
                }
            }
        }
        if let Some(diff) = difference(&actual, &expected, "$") {
            panic!("{} ({}) {diff}", case["id"], case["family"]);
        }
        count += 1;
    }
    assert!(count > 0, "case selection was empty");
    eprintln!(
        "turn_prepare: {count} differential cases ({})",
        if full { "full" } else { "fast" }
    );
}

#[test]
fn malformed_snapshot_fields_are_refused_at_the_typed_boundary() {
    let mut oracle = CapturedExpectations::new();
    let fixture = oracle.call(json!({"command":"case","case":{}}));
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("turn_prepare/invalid_snapshots.json"))
            .expect("invalid fixture data");
    for case in cases {
        let mut snapshot = fixture["snapshot"].clone();
        let path = case["path"].as_str().expect("fixture pointer");
        *snapshot.pointer_mut(path).expect("existing field") = case["value"].clone();
        assert!(
            serde_json::from_value::<PreparationSnapshot>(snapshot).is_err(),
            "accepted invalid field {path}"
        );
    }
    for value in [json!("yesterday"), json!(false), json!([]), json!({})] {
        let mut row = fixture["events"][0].clone();
        row["metadata"]["accepted_at"] = value;
        assert!(
            serde_json::from_value::<Event>(row).is_err(),
            "accepted invalid receipt clock type"
        );
    }
}

#[test]
fn competing_preparations_cannot_roll_back_either_receipt_order() {
    let mut oracle = CapturedExpectations::new();
    let mut preparations = Vec::new();
    for (index, role) in ["First role", "Second role"].iter().enumerate() {
        let fixture = oracle.call(json!({"command":"case","case":{
            "snapshot":{"guidance":{"role":role},"operation_id":format!("op-{index}")},
            "events":[{"id":format!("event-{index}")}]
        }}));
        let snapshot = serde_json::from_value(fixture["snapshot"].clone()).expect("snapshot");
        let events: Vec<Event> = serde_json::from_value(fixture["events"].clone()).expect("events");
        preparations.push(compile(&snapshot, &events).expect("prepared"));
    }
    assert!(preparations[0].manifest.versions != preparations[1].manifest.versions);
    for order in [[0, 1, 2, 0], [1, 0, 2, 1]] {
        preparations.truncate(2);
        let prior_role = ["First role", "Second role"][order[0]];
        let fixture = oracle.call(json!({"command":"case","case":{
            "snapshot":{"guidance":{"role":"Successive role"},"operation_id":"op-2"},
            "prior":{"guidance":{"role":prior_role}},"events":[{"id":"event-2"}]
        }}));
        let snapshot =
            serde_json::from_value(fixture["snapshot"].clone()).expect("successive snapshot");
        let events: Vec<Event> = serde_json::from_value(fixture["events"].clone()).expect("events");
        preparations.push(compile(&snapshot, &events).expect("successive preparation"));
        let expected = oracle.call(json!({"command":"receipt_sequence","order":order}));
        let mut current = None;
        let mut turns = [None, None, None];
        for (step, index) in order.iter().copied().enumerate() {
            let prepared = &preparations[index];
            let receipt: NativeReceipt = serde_json::from_value(json!({
                "operation_id":prepared.operation_id,"actor_id":prepared.actor_id,
                "epoch":prepared.epoch,"event_ids":prepared.event_ids,
                "outcome":{"kind":"accepted","turn_id":format!("turn-{index}")}
            }))
            .expect("receipt");
            let delivery = accept(prepared, &receipt, current.as_ref());
            if let Some(m) = &delivery.manifest_upsert {
                current = Some(m.clone());
            }
            if let Some(turn) = &delivery.transcript_turn_id {
                turns[index] = Some(turn.clone());
            }
            let actual = json!({"delivery":delivery,"stored":{"manifests":[current],"reminders":[],"transcript_turn_id":turns[index]}});
            assert!(
                difference(&actual, &expected[step], "$").is_none(),
                "receipt order {order:?}, step {step}: {}",
                difference(&actual, &expected[step], "$").unwrap_or_default()
            );
        }
    }
}

#[test]
fn reference_floor_is_soft_but_transcript_ceiling_is_hard() {
    let mut oracle = CapturedExpectations::new();
    let fixture =
        oracle.call(json!({"command":"case","case":{"id":"summary-result-file-soft-limit"}}));
    let snapshot = serde_json::from_value(fixture["snapshot"].clone()).expect("snapshot");
    let rows: Vec<Event> = serde_json::from_value(fixture["events"].clone()).expect("events");
    let prepared = compile(&snapshot, &rows).expect("prepared");
    let studio_runtime_services::turn_prepare::ProviderInput::Text { text, .. } =
        &prepared.provider_inputs[0]
    else {
        panic!("text first")
    };
    let payload = text
        .split_once('\n')
        .expect("event header")
        .1
        .split("\n\n")
        .next()
        .expect("payload");
    assert!(
        payload.len() > 22_000,
        "reference must survive the soft budget"
    );
    assert_eq!(
        serde_json::from_str::<Value>(payload).expect("summary")["resultFile"]
            .as_str()
            .expect("reference")
            .len(),
        23_000
    );
    assert_eq!(prepared.transcript.text.chars().count(), 20_000);
    assert!(prepared.transcript.truncated && prepared.fulltext.is_some());
}
