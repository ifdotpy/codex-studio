//! Preference RPC and committed view witnesses through the actual native owner.
use crate::{
    client::Client,
    native::ConnectionConfig,
    protocol::{Request, Response},
    store::Store,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;

const WORKSPACE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn request(id: &str, method: &str, params: Value) -> Request {
    Request {
        id: id.into(),
        method: method.into(),
        params,
    }
}

fn duplicate(request: &Request) -> Request {
    Request {
        id: request.id.clone(),
        method: request.method.clone(),
        params: request.params.clone(),
    }
}

fn success(response: Response) -> Value {
    assert!(
        response.error.is_none(),
        "unexpected native error: {:?}",
        response.error
    );
    response
        .result
        .expect("a successful native reply has a result")
}

fn compare(id: &str, expected: Option<&str>, raw: Option<&str>) -> Request {
    request(
        id,
        "preferences.raw_compare_exchange",
        json!({"key":"choice","expectedRaw":expected,"raw":raw}),
    )
}

#[test]
fn native_preference_compare_exchange_replays_exact_committed_outcome_after_restart() {
    let root = tempfile::tempdir().expect("private preference fixture");
    let path = root.path().join("preferences.sqlite3");
    let mut store = Store::open(&path).expect("open preference store");
    let original = compare("saved-cas", None, Some("Ж🦀"));
    let receipt = success(store.dispatch(duplicate(&original)));
    assert_eq!(receipt, json!({"applied":true,"raw":"Ж🦀","revision":"1"}));
    drop(store);
    let mut store = Store::open(&path).expect("reopen preference store");
    success(store.dispatch(request(
        "later-edit",
        "preferences.raw_set",
        json!({"key":"choice","raw":"later"}),
    )));
    assert_eq!(success(store.dispatch(original)), receipt);
    assert_eq!(
        success(store.dispatch(request("read", "preferences.raw_all", json!({}))))["choice"],
        "later"
    );
    let changed = store.dispatch(compare("saved-cas", None, Some("different")));
    assert_eq!(
        changed.error.expect("changed identity must fail").code,
        "identity_conflict"
    );
    let missed = compare("missed-cas", Some("old"), Some("not-written"));
    assert_eq!(
        success(store.dispatch(duplicate(&missed))),
        json!({"applied":false,"raw":"later","revision":"2"})
    );
    success(store.dispatch(request(
        "newer-edit",
        "preferences.raw_set",
        json!({"key":"choice","raw":"newer"}),
    )));
    assert_eq!(
        success(store.dispatch(missed)),
        json!({"applied":false,"raw":"later","revision":"2"})
    );
    assert_eq!(
        success(store.dispatch(compare("fresh-cas", Some("newer"), None))),
        json!({"applied":true,"raw":null,"revision":"4"})
    );
}

#[test]
fn native_preference_compare_exchange_refuses_namespace_and_scope_selectors() {
    let root = tempfile::tempdir().expect("private preference fixture");
    let mut store =
        Store::open(&root.path().join("preferences.sqlite3")).expect("open preference store");
    for (index, params) in [
        json!({"key":"browser:\"server:other\":choice","expectedRaw":null,"raw":"x"}),
        json!({"key":"choice","expectedRaw":null,"raw":"x","scope":"other"}),
        json!({"key":"choice","expectedRaw":null,"raw":"x","serverId":"other"}),
        json!({"key":"choice","expectedRaw":null,"raw":"x","workspace":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}),
        json!({"key":"choice","raw":"x"}),
    ].into_iter().enumerate() {
        assert!(store.dispatch(request(&format!("refuse-{index}"), "preferences.raw_compare_exchange", params)).error.is_some());
    }
    assert_eq!(
        success(store.dispatch(request("read", "preferences.raw_all", json!({})))),
        json!({})
    );
    let receipts: i64 = store
        .connection
        .query_row("SELECT count(*) FROM client_receipts", [], |row| row.get(0))
        .expect("read exact receipts");
    assert_eq!(
        receipts, 0,
        "unadmitted scope claims must not write receipts or values"
    );
}

#[test]
fn native_preference_compare_exchange_is_atomic_between_store_connections() {
    let root = tempfile::tempdir().expect("private preference fixture");
    let path = root.path().join("preferences.sqlite3");
    let first = Store::open(&path).expect("first connection");
    let second = Store::open(&path).expect("second connection");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let tasks = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, mut store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let value = format!("writer-{index}");
                barrier.wait();
                success(store.dispatch(compare(&value, None, Some(&value))))
            })
        })
        .collect::<Vec<_>>();
    let replies = tasks
        .into_iter()
        .map(|task| task.join().expect("CAS worker exits"))
        .collect::<Vec<_>>();
    assert_eq!(
        replies
            .iter()
            .filter(|reply| reply["applied"] == true)
            .count(),
        1
    );
    assert_eq!(replies[0]["raw"], replies[1]["raw"]);
}

#[test]
fn native_sidebar_edit_preserves_unedited_latest_siblings_and_native_clock_identity() {
    let root = tempfile::tempdir().expect("private preference fixture");
    let path = root.path().join("preferences.sqlite3");
    let mut store = Store::open(&path).expect("open preference store");
    let key = "codex-project-tree:/state";
    store
        .connection
        .execute(
            "INSERT INTO client_preferences VALUES(?1,?2)",
            params![key, "{\"a\":false,\"b\":true}"],
        )
        .expect("fixture seed is not a user edit");
    let command = request(
        "sidebar-edit",
        "preferences.edit",
        json!({"key":key,"previous":{"a":false,"b":false},"next":{"a":true,"b":false}}),
    );
    let receipt = success(store.dispatch(duplicate(&command)));
    assert_eq!(receipt["value"], json!({"a":true,"b":true}));
    let field = serde_json::to_string(&json!([key, "a"])).expect("field identity");
    let first = receipt["fields"]["server"][&field].clone();
    assert_eq!(first["value"], true);
    assert!(
        first["timestamp"]
            .as_u64()
            .is_some_and(|timestamp| timestamp > 0)
    );
    assert!(
        first["writer"]
            .as_str()
            .is_some_and(|writer| !writer.is_empty())
    );
    assert_eq!(
        receipt["fields"]["server"]
            .as_object()
            .expect("native fields")
            .len(),
        1
    );
    drop(store);
    let mut store = Store::open(&path).expect("reopen preference store");
    assert_eq!(success(store.dispatch(command)), receipt);
    let removed = success(store.dispatch(request(
        "sidebar-remove",
        "preferences.edit",
        json!({"key":key,"previous":{"a":true},"next":{}}),
    )));
    assert_eq!(removed["value"], json!({"b":true}));
    assert_eq!(removed["fields"]["server"][&field]["value"], Value::Null);
    assert_eq!(
        removed["fields"]["server"][&field]["writer"],
        first["writer"]
    );
    assert!(
        removed["fields"]["server"][&field]["timestamp"].as_u64() > first["timestamp"].as_u64()
    );
}

#[test]
fn native_managed_raw_writes_capture_only_successful_new_field_intent() {
    let root = tempfile::tempdir().expect("private managed preference fixture");
    let mut store = Store::open(&root.path().join("preferences.sqlite3")).expect("native store");
    let key = "codex-studio-preferences-v1";
    let theme = serde_json::to_string(&json!([key, "theme"])).expect("theme field");
    let first = request(
        "managed-raw",
        "preferences.raw_set",
        json!({"key":key,"raw":"{\"theme\":\"dark\"}"}),
    );
    let receipt = success(store.dispatch(duplicate(&first)));
    let initial = success(store.dispatch(request("view", "preferences.view", json!({}))));
    assert_eq!(initial["fields"]["user"][&theme]["value"], "dark");
    assert_eq!(initial["sync"]["user"]["state"], "saving");
    let noop = success(store.dispatch(request(
        "noop",
        "preferences.raw_set",
        json!({"key":key,"raw":initial["values"][key]}),
    )));
    assert_eq!(noop["revision"], initial["revision"]);
    let missed = success(store.dispatch(request(
        "miss",
        "preferences.raw_compare_exchange",
        json!({"key":key,"expectedRaw":"older","raw":"{\"theme\":\"light\"}"}),
    )));
    assert_eq!(missed["applied"], false);
    assert_eq!(missed["revision"], initial["revision"]);
    let saved = success(store.dispatch(request(
        "cas",
        "preferences.raw_compare_exchange",
        json!({"key":key,"expectedRaw":initial["values"][key],"raw":"{\"theme\":\"light\"}"}),
    )));
    assert_eq!(saved["applied"], true);
    assert_eq!(success(store.dispatch(first)), receipt);
    let final_view = success(store.dispatch(request("final", "preferences.view", json!({}))));
    assert_eq!(final_view["fields"]["user"][&theme]["value"], "light");
    assert!(
        final_view["fields"]["user"][&theme]["timestamp"].as_u64()
            > initial["fields"]["user"][&theme]["timestamp"].as_u64()
    );
    assert_eq!(final_view["revision"], saved["revision"]);
}

#[test]
fn native_partial_appearance_projection_materializes_shared_defaults() {
    let mut store = Store::open(std::path::Path::new(":memory:")).expect("native store");
    success(store.dispatch(request(
        "identity",
        "identity.set",
        json!({"workspaceId":WORKSPACE}),
    )));
    let key = "codex-studio-preferences-v1";
    let theme = serde_json::to_string(&json!([key, "theme"])).expect("theme field");
    let entity = json!({"collection":"uiPreferences","id":"user","seq":1,"op":"upsert","value":{"fields":{(theme):{"value":"dark","timestamp":1,"writer":"remote"}}}});
    store
        .connection
        .execute(
            "INSERT INTO client_projections VALUES(?1,'entity:uiPreferences:user',?2,1)",
            params![WORKSPACE, entity.to_string()],
        )
        .expect("native committed source");
    let view = success(store.dispatch(request("view", "preferences.view", json!({}))));
    let actual: Value = serde_json::from_str(view["values"][key].as_str().expect("appearance raw"))
        .expect("appearance JSON");
    let mut expected =
        studio_client_domain::preferences::appearance_defaults(cfg!(target_os = "macos"));
    expected["theme"] = json!("dark");
    assert_eq!(actual, expected);
    assert_eq!(
        view["sync"]["user"]["state"], "saved",
        "source materialization cannot create local edit intent"
    );
}

#[test]
fn native_preferences_adopt_only_committed_projection_fields_not_renderer_clocks() {
    let root = tempfile::tempdir().expect("private preference fixture");
    let mut store =
        Store::open(&root.path().join("preferences.sqlite3")).expect("open preference store");
    success(store.dispatch(request(
        "identity",
        "identity.set",
        json!({"workspaceId":WORKSPACE}),
    )));
    let key = "codex-project-tree:/state";
    let edit = success(store.dispatch(request(
        "edit",
        "preferences.edit",
        json!({"key":key,"previous":{},"next":{"a":true}}),
    )));
    let a = serde_json::to_string(&json!([key, "a"])).expect("field a");
    let b = serde_json::to_string(&json!([key, "b"])).expect("field b");
    let timestamp = edit["fields"]["server"][&a]["timestamp"]
        .as_u64()
        .expect("native clock");
    let fields = json!({(a.clone()):{"value":false,"timestamp":timestamp-1,"writer":"server"},(b.clone()):{"value":true,"timestamp":timestamp+1,"writer":"server"}});
    let entity = json!({"collection":"uiPreferences","id":"server","seq":10,"op":"upsert","value":{"fields":fields}});
    store.connection.execute("INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,'entity:uiPreferences:server',?2,10)", params![WORKSPACE,entity.to_string()]).expect("committed source entity fixture");
    let forged = store.dispatch(request("forged", "preferences.adopt_fields", json!({"fields":{(a.clone()):{"value":false,"timestamp":9007199254740991_u64,"writer":"forged"}}})));
    assert!(forged.error.is_some());
    let adopted = success(store.dispatch(request("adopt", "preferences.adopt_fields", json!({}))));
    assert_eq!(adopted["fields"]["server"][&a]["value"], true);
    assert_eq!(adopted["fields"]["server"][&b]["value"], true);
    assert_eq!(
        serde_json::from_str::<Value>(
            adopted["values"][key]
                .as_str()
                .expect("materialized native raw")
        )
        .expect("native JSON"),
        json!({"a":true,"b":true})
    );
}

fn open_client(root: &std::path::Path) -> (Client, mpsc::UnboundedReceiver<Value>) {
    let (events, receiver) = mpsc::unbounded_channel();
    let client = Client::open(
        ConnectionConfig {
            state_directory: root.into(),
            backend_origin: "http://127.0.0.1:9".into(),
        },
        move |event| {
            let _ = events.send(event);
        },
    )
    .expect("native socket fixture");
    (client, receiver)
}

async fn event(receiver: &mut mpsc::UnboundedReceiver<Value>, subscription: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = receiver
                .recv()
                .await
                .expect("native event channel remains open");
            if event["event"] == subscription {
                assert!(event.get("error").is_none(), "native view failed: {event}");
                return event["value"].clone();
            }
        }
    })
    .await
    .expect("the native event must reach its waiting consumer")
}

#[tokio::test]
async fn native_preference_rpc_and_push_view_preserve_connection_owner_without_network() {
    let root = tempfile::tempdir().expect("private preference fixture");
    let (first, mut events) = open_client(&root.path().join("first"));
    let (other, _) = open_client(&root.path().join("other"));
    success(
        first
            .request(request(
                "subscribe",
                "view.subscribe",
                json!({"subscription":"preferences","topic":"preferences","workspace":WORKSPACE}),
            ))
            .await
            .expect("subscribe reply"),
    );
    assert_eq!(
        event(&mut events, "preferences").await,
        json!({"values":{},"fields":{"user":{},"server":{}},"revision":"0","sync":{"user":{"state":"saved"},"server":{"state":"saved"}}})
    );
    assert_eq!(
        success(
            first
                .request(compare("cas", None, Some("selected")))
                .await
                .expect("CAS reply")
        ),
        json!({"applied":true,"raw":"selected","revision":"1"})
    );
    assert_eq!(
        event(&mut events, "preferences").await["values"]["choice"],
        "selected"
    );
    assert_eq!(
        success(
            first
                .request(compare("cas", None, Some("selected")))
                .await
                .expect("replayed CAS reply")
        ),
        json!({"applied":true,"raw":"selected","revision":"1"})
    );
    assert_eq!(
        success(
            other
                .request(request("read", "preferences.view", json!({})))
                .await
                .expect("other preference view")
        )["values"],
        json!({})
    );
    assert_eq!(
        success(
            first
                .request(request("read", "preferences.view", json!({})))
                .await
                .expect("own preference view")
        )["values"]["choice"],
        "selected"
    );
}

#[tokio::test]
async fn native_upload_snapshot_and_push_tombstones_hide_bytes_and_other_workspaces() {
    let root = tempfile::tempdir().expect("private upload view fixture");
    let mut store =
        Store::open(&root.path().join("studio-client.sqlite3")).expect("seed upload store");
    success(store.dispatch(request(
        "identity",
        "identity.set",
        json!({"workspaceId":WORKSPACE}),
    )));
    for (id, workspace) in [
        ("owned", WORKSPACE),
        ("other", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    ] {
        let document = json!({"id":id,"workspace":workspace,"stateDir":"/state","agent":"chat","name":"file.txt","mime":"text/plain","size":18,"created":1});
        store
            .connection
            .execute(
                "INSERT INTO client_uploads VALUES(?1,?2,'/state','chat',?3,?4)",
                params![
                    id,
                    workspace,
                    document.to_string(),
                    b"private-file-bytes".as_slice()
                ],
            )
            .expect("seed exact upload row");
    }
    drop(store);
    let (client, mut events) = open_client(root.path());
    let snapshot = success(
        client
            .request(request("snapshot", "uploads.snapshot", json!({})))
            .await
            .expect("upload snapshot reply"),
    );
    assert_eq!(snapshot["workspaceId"], WORKSPACE);
    assert_eq!(
        snapshot["entries"]
            .as_array()
            .expect("upload entries")
            .len(),
        1
    );
    assert_eq!(snapshot["entries"][0]["id"], "owned");
    assert_eq!(snapshot["tombstones"], json!({}));
    assert_eq!(snapshot["revision"], "0");
    assert!(!snapshot.to_string().contains("private-file-bytes"));
    success(
        client
            .request(request(
                "subscribe",
                "view.subscribe",
                json!({"subscription":"uploads","topic":"uploads","workspace":WORKSPACE}),
            ))
            .await
            .expect("subscribe upload view"),
    );
    assert_eq!(event(&mut events, "uploads").await, snapshot);
    success(
        client
            .request(request("cancel", "uploads.cancel", json!({"id":"owned"})))
            .await
            .expect("cancel exact upload"),
    );
    let cancelled = event(&mut events, "uploads").await;
    assert_eq!(cancelled["entries"], json!([]));
    assert_eq!(
        cancelled["tombstones"]["owned"],
        json!({"cancelled":true,"settled":true})
    );
    assert_eq!(cancelled["revision"], "1");
}

#[test]
fn native_preference_revision_keeps_noop_and_replay_stable_beyond_javascript_integer_precision() {
    let root = tempfile::tempdir().expect("private preference revision fixture");
    let path = root.path().join("preferences.sqlite3");
    let mut store = Store::open(&path).expect("open preference store");
    let first = request(
        "raw",
        "preferences.raw_set",
        json!({"key":"choice","raw":"a"}),
    );
    assert_eq!(
        success(store.dispatch(duplicate(&first))),
        json!({"saved":true,"revision":"1"})
    );
    assert_eq!(
        success(store.dispatch(request(
            "noop",
            "preferences.raw_set",
            json!({"key":"choice","raw":"a"})
        ))),
        json!({"saved":true,"revision":"1"})
    );
    // Exercise the exact wire representation, not a rounded JavaScript number.
    store.connection.execute("UPDATE client_local_view_revisions SET revision=9007199254740992 WHERE topic='preferences'", []).expect("seed a durable large revision");
    let changed = success(store.dispatch(compare("large", Some("a"), Some("b"))));
    assert_eq!(changed["revision"], "9007199254740993");
    assert_eq!(
        success(store.dispatch(first))["revision"],
        "1",
        "an old saved result cannot claim the current snapshot version"
    );
    drop(store);
    let mut store = Store::open(&path).expect("reopen preference store");
    assert_eq!(
        success(store.dispatch(request("read", "preferences.view", json!({}))))["revision"],
        "9007199254740993"
    );
}

#[tokio::test]
async fn native_upload_subscription_install_covers_the_snapshot_to_subscribe_gap() {
    let root = tempfile::tempdir().expect("private upload subscription fixture");
    let mut store = Store::open(&root.path().join("studio-client.sqlite3"))
        .expect("seed owned upload identity");
    success(store.dispatch(request(
        "identity",
        "identity.set",
        json!({"workspaceId":WORKSPACE}),
    )));
    drop(store);
    let (client, mut events) = open_client(root.path());
    let staged = success(
        client
            .request(request(
                "stage",
                "files.text",
                json!({"name":"note.txt","text":"owned bytes"}),
            ))
            .await
            .expect("stage fixture bytes"),
    );
    let queued = success(
        client
            .request(request(
                "queue",
                "uploads.queue",
                json!({"workspace":WORKSPACE,"stateDir":"/state","agent":"chat","files":[staged]}),
            ))
            .await
            .expect("queue exact upload"),
    );
    let id = queued[0]["id"].as_str().expect("native upload identity");
    let initial = success(
        client
            .request(request("initial", "uploads.snapshot", json!({})))
            .await
            .expect("initial upload snapshot"),
    );
    assert_eq!(
        initial["entries"].as_array().expect("initial rows").len(),
        1
    );
    success(
        client
            .request(request(
                "cancel-before-subscribe",
                "uploads.cancel",
                json!({"id":id}),
            ))
            .await
            .expect("cancel exact upload"),
    );
    success(
        client
            .request(request(
                "subscribe",
                "view.subscribe",
                json!({"subscription":"uploads","topic":"uploads","workspace":WORKSPACE}),
            ))
            .await
            .expect("install native upload observer"),
    );
    let installed = event(&mut events, "uploads").await;
    assert_eq!(installed["entries"], json!([]));
    assert_eq!(
        installed["tombstones"][id],
        json!({"cancelled":true,"settled":true})
    );
    assert_ne!(installed["revision"], initial["revision"]);
    assert_eq!(installed["revision"], "2");
}
