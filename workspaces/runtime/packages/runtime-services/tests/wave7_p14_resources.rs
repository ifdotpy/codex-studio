use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use studio_runtime_services::change_notifications::ChangeNotificationHub;
use studio_runtime_services::resource_notify::{
    MAX_NOTIFY_RECEIPTS, MAX_PUBLICATION_BYTES, ResourceNotifications, ResourceNotifyError,
};

use studio_runtime_services::token_rate::{MAX_SNAPSHOT_BYTES, TokenRateHub};

#[path = "support/prepared_python.rs"]
mod python;

fn request(id: &str, resources: Value) -> Value {
    json!({"requestId": id, "resources": resources})
}

fn rates(index: usize) -> Value {
    let rate = json!({"turnId": "turn", "active": true, "estimated": false, "rate": index, "outputTokens": index});
    json!({"rates": {"agent": rate}, "teams": {"lead": {"agent": rate}}})
}

fn token_hub(changes: &Arc<ChangeNotificationHub>) -> TokenRateHub {
    let changes = changes.clone();
    TokenRateHub::with_change_callback(Arc::new(move || changes.notify_one()))
}

#[test]
fn python_corpus_matches_complete_notify_and_token_publication_results() {
    let mut cases = vec![
        json!({"op": "notify", "body": request("same", json!([{"kind":"accounts"}, {"kind":"state"}]))}),
        json!({"op": "notify", "body": request("same", json!([{"kind":"state"}, {"kind":"accounts"}, {"kind":"accounts"}]))}),
        json!({"op": "notify", "body": request("same", json!([{"kind":"models"}]))}),
        json!({"op": "rates", "body": rates(0)}),
        json!({"op": "rates", "body": rates(0)}),
        json!({"op": "rates", "body": rates(123)}),
        json!({"op": "rates", "body": {"rates": {}, "teams": {}}}),
        json!({"op": "notify", "body": request("", json!([{"kind":"accounts"}]))}),
        json!({"op": "notify", "body": request("empty", json!([]))}),
        json!({"op": "notify", "body": request("unknown", json!([{"kind":"unknown"}]))}),
        json!({"op": "rates", "body": {"rates": null, "teams": {}}}),
    ];
    // Full corpus adds schema field variations; the default already covers eviction.
    if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
        for field in ["turnId", "active", "estimated", "rate", "outputTokens"] {
            for invalid in [
                Value::Null,
                json!([]),
                json!({}),
                json!(""),
                json!(false),
                json!(-1),
            ] {
                let mut value = rates(1);
                value["rates"]["agent"][field] = invalid;
                cases.push(json!({"op":"rates", "body":value}));
            }
            let mut value = rates(1);
            value["rates"]["agent"]
                .as_object_mut()
                .expect("agent rate")
                .remove(field);
            cases.push(json!({"op":"rates", "body":value}));
        }
        for id in ["x".repeat(200), "x".repeat(201), "界".repeat(200)] {
            cases.push(json!({"op":"notify", "body":request(&id, json!([{"kind":"accounts"}]))}));
        }
    }
    for kind in [
        "panel",
        "queue",
        "receipts",
        "workspace",
        "voice",
        "session-cost",
        "transcript",
    ] {
        cases.push(
            json!({"op":"notify", "body": request(kind, json!([{"kind":kind,"agentId":"agent"}]))}),
        );
    }
    for index in 0..=MAX_NOTIFY_RECEIPTS {
        cases.push(json!({"op":"notify", "body":request(&format!("bound-{index}"), json!([{"kind":"accounts"}]))}));
    }
    cases.push(cases[0].clone()); // The initial receipt was evicted.
    cases.push(json!({"op":"restart"}));
    cases.push(cases[0].clone()); // Process-local identity scope resets.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let mut child = Command::new(python::prepared_python(&source))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p14_oracle.py"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("oracle starts");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&serde_json::to_vec(&cases).expect("cases serialize"))
        .expect("write cases");
    let output = child.wait_with_output().expect("oracle finishes");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<Value> = serde_json::from_slice(&output.stdout).expect("oracle JSON");
    assert_eq!(expected.len(), cases.len(), "oracle must return every case");
    let mut hub = Arc::new(ChangeNotificationHub::new());
    let mut publisher = ResourceNotifications::new(hub.clone());
    let mut tokens = token_hub(&hub);
    for (index, (case, expected)) in cases.iter().zip(expected).enumerate() {
        let result = match case["op"].as_str().expect("operation") {
            "restart" => {
                hub = Arc::new(ChangeNotificationHub::new());
                publisher = ResourceNotifications::new(hub.clone());
                tokens = token_hub(&hub);
                Ok(true)
            }
            "rates" => tokens
                .publish(case["body"].clone())
                .map_err(|_| ResourceNotifyError::Invalid),
            _ => publisher.publish_once(&case["body"]),
        };
        let result = match result {
            Ok(changed) => json!(changed),
            Err(ResourceNotifyError::Conflict) => json!(409),
            Err(ResourceNotifyError::Invalid) => json!(400),
            Err(error) => panic!("{error}"),
        };
        let actual = json!({"result":result,"revision":hub.snapshot().revision,"rates":tokens.snapshot().to_value()});
        assert_eq!(actual, expected, "case {index}: {case}");
    }
}

#[test]
fn lru_refresh_and_lost_ack_keep_exact_identity_at_capacity() {
    let hub = Arc::new(ChangeNotificationHub::new());
    let publisher = ResourceNotifications::new(hub.clone());
    let body = |index| request(&format!("request-{index}"), json!([{"kind":"accounts"}]));
    for index in 0..MAX_NOTIFY_RECEIPTS {
        assert!(publisher.publish_once(&body(index)).expect("first"));
    }
    assert!(!publisher.publish_once(&body(0)).expect("lost ACK replay"));
    assert!(
        publisher
            .publish_once(&body(MAX_NOTIFY_RECEIPTS))
            .expect("eviction")
    );
    assert!(
        !publisher
            .publish_once(&body(0))
            .expect("refreshed retained")
    );
    assert!(publisher.publish_once(&body(1)).expect("oldest evicted"));
    assert_eq!(hub.snapshot().revision, MAX_NOTIFY_RECEIPTS as i64 + 2);
}

#[test]
fn same_identity_concurrent_requests_publish_once() {
    let hub = Arc::new(ChangeNotificationHub::new());
    let publisher = Arc::new(ResourceNotifications::new(hub.clone()));
    let handles = (0..4)
        .map(|_| {
            let publisher = publisher.clone();
            std::thread::spawn(move || {
                publisher
                    .publish_once(&request("same", json!([{"kind":"accounts"}])))
                    .expect("publish")
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        handles
            .into_iter()
            .map(|handle| usize::from(handle.join().expect("thread")))
            .sum::<usize>(),
        1
    );
    assert_eq!(hub.snapshot().revision, 1);
}

#[test]
fn closed_publisher_and_invalid_input_never_advance_cursor() {
    let hub = Arc::new(ChangeNotificationHub::new());
    let publisher = ResourceNotifications::new(hub.clone());
    let large = request(
        "large",
        json!([{"kind":"panel", "agentId":"x".repeat(MAX_PUBLICATION_BYTES)}]),
    );
    assert_eq!(
        publisher.publish_once(&large),
        Err(ResourceNotifyError::Invalid)
    );
    hub.close();
    assert_eq!(
        publisher.publish_once(&request("first", json!([{"kind":"accounts"}]))),
        Err(ResourceNotifyError::Unavailable)
    );
    assert_eq!(hub.snapshot().revision, 0);
}

#[test]
fn external_state_reset_and_receipt_share_the_publication_boundary() {
    let hub = Arc::new(ChangeNotificationHub::new());
    let publisher = ResourceNotifications::new(hub.clone());
    let body = request("reset", json!([{"kind":"state"}]));
    assert!(!publisher.entity_reset_after(0).expect("before publication"));
    assert!(
        publisher
            .publish_once(&body)
            .expect("publish committed state")
    );
    let revision = hub.snapshot().revision;
    assert!(publisher.entity_reset_after(0).expect("reset"));
    assert!(
        !publisher
            .entity_reset_after(revision)
            .expect("already observed")
    );
    assert!(!publisher.publish_once(&body).expect("lost ACK"));
    assert_eq!(hub.snapshot().revision, revision);
}

#[test]
fn token_snapshot_byte_bound_is_checked_before_replacing_the_last_value() {
    let changes = Arc::new(ChangeNotificationHub::new());
    let publisher = token_hub(&changes);
    let value = rates(1)["rates"]["agent"].clone();
    let count = 1024;
    let map = (0..count)
        .map(|index| (format!("a{index:04}"), value.clone()))
        .collect::<serde_json::Map<_, _>>();
    let remaining = MAX_SNAPSHOT_BYTES - json!({"rates":map,"teams":{}}).to_string().len();
    let map = (0..count)
        .map(|index| {
            let padding = remaining / count + usize::from(index < remaining % count);
            (format!("a{index:04}{}", "x".repeat(padding)), value.clone())
        })
        .collect::<serde_json::Map<_, _>>();
    let at_bound = json!({"rates":map,"teams":{}});
    assert_eq!(at_bound.to_string().len(), MAX_SNAPSHOT_BYTES);
    assert!(
        publisher
            .publish(at_bound.clone())
            .expect("exact byte bound")
    );
    let mut above_bound = at_bound.clone();
    let entries = above_bound["rates"].as_object_mut().expect("rates");
    let key = entries.keys().next().expect("key").clone();
    let value = entries.remove(&key).expect("entry");
    entries.insert(format!("{key}x"), value);
    assert!(publisher.publish(above_bound).is_err());
    assert_eq!(publisher.snapshot().to_value(), at_bound);
    assert_eq!(changes.snapshot().revision, 1);
}
