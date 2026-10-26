//! Compiler boundary and receipt-order regressions over minimal immutable snapshots.
use serde_json::{Value, json};
use studio_runtime_services::turn_prepare::{
    Event, NativeReceipt, PreparationSnapshot, accept, compile,
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
            "turn_prepare/snapshots.json"
        )))
    }
    fn case(&self, case: &Value) -> &Value {
        &self
            .0
            .iter()
            .find(|row| {
                row.case == *case
                    || (case.get("id").is_some() && row.case.get("id") == case.get("id"))
            })
            .expect("captured preparation case")
            .response
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptSequence {
    order: Vec<usize>,
    expected: Value,
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
fn malformed_snapshot_fields_are_refused_at_the_typed_boundary() {
    let fixtures = CapturedExpectations::new();
    let fixture = fixtures.case(&json!({}));
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
    let fixtures = CapturedExpectations::new();
    let mut preparations = Vec::new();
    for (index, role) in ["First role", "Second role"].iter().enumerate() {
        let fixture = fixtures.case(&json!({
            "snapshot":{"guidance":{"role":role},"operation_id":format!("op-{index}")},
            "events":[{"id":format!("event-{index}")}]
        }));
        let snapshot = serde_json::from_value(fixture["snapshot"].clone()).expect("snapshot");
        let events: Vec<Event> = serde_json::from_value(fixture["events"].clone()).expect("events");
        preparations.push(compile(&snapshot, &events).expect("prepared"));
    }
    assert!(preparations[0].manifest.versions != preparations[1].manifest.versions);
    for order in [[0, 1, 2, 0], [1, 0, 2, 1]] {
        preparations.truncate(2);
        let prior_role = ["First role", "Second role"][order[0]];
        let fixture = fixtures.case(&json!({
            "snapshot":{"guidance":{"role":"Successive role"},"operation_id":"op-2"},
            "prior":{"guidance":{"role":prior_role}},"events":[{"id":"event-2"}]
        }));
        let snapshot =
            serde_json::from_value(fixture["snapshot"].clone()).expect("successive snapshot");
        let events: Vec<Event> = serde_json::from_value(fixture["events"].clone()).expect("events");
        preparations.push(compile(&snapshot, &events).expect("successive preparation"));
        let sequences: Vec<ReceiptSequence> =
            crate::turn_fixtures::load(include_bytes!("turn_prepare/receipt_sequences.json"));
        let expected = &sequences
            .iter()
            .find(|row| row.order == order)
            .expect("captured receipt order")
            .expected;
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
    let fixtures = CapturedExpectations::new();
    let fixture = fixtures.case(&json!({"id":"summary-result-file-soft-limit"}));
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
