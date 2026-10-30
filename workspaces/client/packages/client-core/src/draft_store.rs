//! Draft edits commit before acknowledgment. Replication cannot erase a newer edit.
use crate::{
    drafts::{self, DraftSnapshot, DraftVersion},
    projections::Document,
    protocol::{Request, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};
use studio_client_domain::json_object::optional_field;

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS client_draft_device (singleton INTEGER PRIMARY KEY CHECK(singleton=1), device TEXT NOT NULL);
        INSERT OR IGNORE INTO client_draft_device VALUES(1,lower(hex(randomblob(16))));
        CREATE TABLE IF NOT EXISTS client_draft_branches (workspace TEXT NOT NULL,id TEXT NOT NULL,document TEXT NOT NULL,master TEXT,dirty INTEGER NOT NULL,PRIMARY KEY(workspace,id));
        CREATE TABLE IF NOT EXISTS client_draft_scopes (workspace TEXT PRIMARY KEY,revision INTEGER NOT NULL DEFAULT 0,checkpoint INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE IF NOT EXISTS client_draft_sync (workspace TEXT PRIMARY KEY,error TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_draft_dismissed (workspace TEXT NOT NULL,id TEXT NOT NULL,text TEXT NOT NULL,PRIMARY KEY(workspace,id,text));")
}

fn sql<T>(value: rusqlite::Result<T>) -> Result<T, String> {
    value.map_err(|e| e.to_string())
}
fn decode(document: &Document) -> Result<DraftVersion, String> {
    let value = serde_json::from_str(&document.payload).map_err(|e| e.to_string())?;
    DraftVersion::from_stored_value(&document.id, value)
}
fn document(version: &DraftVersion) -> Document {
    // DraftVersion contains only JSON-serializable fields with string map keys.
    // Value's ordered map matches the server's canonical key order.
    Document {
        id: version.id.clone(),
        seq: 0,
        deleted: false,
        payload: serde_json::to_value(version)
            .expect("draft version is JSON-serializable")
            .to_string(),
    }
}
#[cfg(test)]
thread_local! { static STORED_DRAFT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

fn rows(
    db: &Connection,
    workspace: &str,
) -> Result<Vec<(Document, Option<Document>, bool)>, String> {
    let mut query = sql(db.prepare(
        "SELECT document,master,dirty FROM client_draft_branches WHERE workspace=?1 ORDER BY id",
    ))?;
    let rows = sql(query.query_map([workspace], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, bool>(2)?,
        ))
    }))?;
    rows.map(|row| decode_row(sql(row)?)).collect()
}
fn decode_row(
    (local, master, dirty): (String, Option<String>, bool),
) -> Result<(Document, Option<Document>, bool), String> {
    #[cfg(test)]
    STORED_DRAFT_DECODES.update(|count| count + 1);
    Ok((
        serde_json::from_str(&local).map_err(|e| e.to_string())?,
        master
            .map(|v| serde_json::from_str(&v).map_err(|e| e.to_string()))
            .transpose()?,
        dirty,
    ))
}
fn keyed_row(
    db: &Connection,
    workspace: &str,
    id: &str,
) -> Result<Option<(Document, Option<Document>, bool)>, String> {
    sql(db
        .query_row(
            "SELECT document,master,dirty FROM client_draft_branches WHERE workspace=?1 AND id=?2",
            params![workspace, id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional())?
    .map(decode_row)
    .transpose()
}

fn save(
    db: &Connection,
    workspace: &str,
    local: &Document,
    master: Option<&Document>,
    dirty: bool,
) -> Result<(), String> {
    // Document fields are strings, integers and booleans, so JSON serialization is infallible.
    sql(db.execute("INSERT INTO client_draft_branches VALUES(?1,?2,?3,?4,?5) ON CONFLICT(workspace,id) DO UPDATE SET document=excluded.document,master=excluded.master,dirty=excluded.dirty",
        params![workspace,local.id,serde_json::to_string(local).expect("document is JSON-serializable"),master.map(|v|serde_json::to_string(v).expect("document is JSON-serializable")),dirty]))?;
    Ok(())
}
fn revision(db: &Connection, workspace: &str) -> Result<(), String> {
    sql(db.execute("INSERT INTO client_draft_scopes(workspace,revision) VALUES(?1,1) ON CONFLICT(workspace) DO UPDATE SET revision=revision+1",[workspace]))?;
    Ok(())
}
fn same(a: &Document, b: &Document) -> bool {
    a.id == b.id
        && a.deleted == b.deleted
        && serde_json::from_str::<Value>(&a.payload).ok()
            == serde_json::from_str::<Value>(&b.payload).ok()
}

pub fn view(db: &Connection, workspace: &str) -> Result<DraftSnapshot, String> {
    let rows = rows(db, workspace)?;
    let mut versions = Vec::new();
    for (doc, _, _) in &rows {
        if !doc.deleted {
            let version = decode(doc)?;
            versions.push(version);
        }
    }
    let mut query =
        sql(db.prepare("SELECT id,text FROM client_draft_dismissed WHERE workspace=?1"))?;
    let dismissed = sql(query.query_map([workspace], |r| Ok((r.get(0)?, r.get(1)?))))?
        .collect::<Result<BTreeSet<(String, String)>, _>>()
        .map_err(|e| e.to_string())?;
    // Dirty means pending replication, not an uncommitted renderer edit. All
    // these branches are committed; reconcile them so conflicts remain visible.
    let view = drafts::reconcile(&versions, &BTreeMap::new(), &dismissed)?;
    let revision = sql(db
        .query_row(
            "SELECT revision FROM client_draft_scopes WHERE workspace=?1",
            [workspace],
            |r| crate::store::unsigned(r, 0),
        )
        .optional())?
    .unwrap_or(0);
    let error = sql(db
        .query_row(
            "SELECT error FROM client_draft_sync WHERE workspace=?1",
            [workspace],
            |r| r.get::<_, String>(0),
        )
        .optional())?
    .unwrap_or_default();
    Ok(DraftSnapshot {
        view,
        workspace_id: workspace.into(),
        revision,
        error,
    })
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DraftRow {
    pub new_document_state: Document,
    pub assumed_master_state: Option<Document>,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct PendingDrafts {
    pub rows: Vec<DraftRow>,
    pub checkpoint: u64,
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum DraftResult {
    Snapshot(DraftSnapshot),
    Pending(PendingDrafts),
}

fn project<T: for<'de> Deserialize<'de> + Default>(value: &Value) -> T {
    if value.is_object() {
        T::deserialize(value).unwrap_or_default()
    } else {
        T::default()
    }
}
#[derive(Default, Deserialize)]
struct DraftInput {
    #[serde(default, deserialize_with = "optional_field")]
    workspace: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    error: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    checkpoint: Option<u64>,
}
#[derive(Default, Deserialize)]
struct DraftDismissal {
    #[serde(default, deserialize_with = "optional_field")]
    id: Option<String>,
    #[serde(default, deserialize_with = "optional_field")]
    text: Option<String>,
}
fn execute(db: &Connection, request: &Request) -> Result<DraftResult, String> {
    let p = &request.params;
    let input: DraftInput = project(p);
    let workspace = input
        .workspace
        .as_deref()
        .filter(|v| crate::projections::valid_workspace(v))
        .ok_or("Invalid draft workspace.")?;
    match request.method.as_str() {
        "drafts.view" => view(db, workspace).map(DraftResult::Snapshot),
        "drafts.sync_status" => {
            let message = input.error.as_deref().unwrap_or("");
            sql(db.execute("INSERT INTO client_draft_sync VALUES(?1,?2) ON CONFLICT(workspace) DO UPDATE SET error=excluded.error",params![workspace,message]))?;
            view(db, workspace).map(DraftResult::Snapshot)
        }
        "drafts.import" => {
            let versions: Vec<DraftVersion> =
                Vec::<DraftVersion>::deserialize(&p["versions"]).map_err(|e| e.to_string())?;
            if versions.len() > 10000 {
                return Err("Too many saved draft branches.".into());
            }
            for version in versions {
                version.validate()?;
                let incoming = document(&version);
                let existing = keyed_row(db, workspace, &version.id)?;
                match existing {
                    Some((local, master, _)) => {
                        let merged = document(&drafts::merge(&decode(&local)?, &version)?);
                        save(db, workspace, &merged, master.as_ref(), true)?;
                    }
                    None => save(db, workspace, &incoming, None, true)?,
                }
            }
            revision(db, workspace)?;
            view(db, workspace).map(DraftResult::Snapshot)
        }
        "drafts.edit" => {
            let changes = p["changes"]
                .as_object()
                .ok_or("Draft changes are required.")?;
            let device = sql(db.query_row(
                "SELECT device FROM client_draft_device WHERE singleton=1",
                [],
                |r| r.get::<_, String>(0),
            ))?;
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_millis() as u64;
            for (session, text) in changes {
                let text = optional_field::<_, String>(text)
                    .map_err(|e| e.to_string())?
                    .filter(|text| text.chars().count() <= 2_000_000)
                    .ok_or("Invalid draft text.")?;
                let records = rows(db, workspace)?;
                let versions = records
                    .iter()
                    .filter(|(d, _, _)| !d.deleted)
                    .map(|(d, _, _)| decode(d))
                    .collect::<Result<Vec<_>, _>>()?;
                let version = drafts::edit(&device, session, text, &versions, now)?;
                let master = records
                    .iter()
                    .find(|(d, _, _)| d.id == version.id)
                    .and_then(|(_, master, _)| master.as_ref());
                save(db, workspace, &document(&version), master, true)?;
            }
            revision(db, workspace)?;
            view(db, workspace).map(DraftResult::Snapshot)
        }
        "drafts.dismiss" => {
            let version: DraftDismissal = project(&p["version"]);
            let (id, text) = (
                version.id.as_deref().ok_or("Missing draft identity.")?,
                version.text.as_deref().ok_or("Missing draft text.")?,
            );
            sql(db.execute(
                "INSERT OR IGNORE INTO client_draft_dismissed VALUES(?1,?2,?3)",
                params![workspace, id, text],
            ))?;
            revision(db, workspace)?;
            view(db, workspace).map(DraftResult::Snapshot)
        }
        "drafts.pending" => {
            let rows = rows(db, workspace)?
                .into_iter()
                .filter(|(_, _, dirty)| *dirty)
                .take(100)
                .map(|(local, master, _)| DraftRow {
                    new_document_state: local,
                    assumed_master_state: master,
                })
                .collect::<Vec<_>>();
            let checkpoint = sql(db
                .query_row(
                    "SELECT checkpoint FROM client_draft_scopes WHERE workspace=?1",
                    [workspace],
                    |r| crate::store::unsigned(r, 0),
                )
                .optional())?
            .unwrap_or(0);
            Ok(DraftResult::Pending(PendingDrafts { rows, checkpoint }))
        }
        "drafts.receive" => {
            let incoming: Vec<Document> =
                Vec::<Document>::deserialize(&p["documents"]).map_err(|e| e.to_string())?;
            for master in incoming {
                let master_version = decode(&master)?;
                let old = keyed_row(db, workspace, &master.id)?;
                match old {
                    Some((local, previous, dirty)) => {
                        if previous.as_ref().is_some_and(|v| v.seq > master.seq) {
                            continue;
                        }
                        if dirty && !same(&local, &master) {
                            let merged =
                                document(&drafts::merge(&master_version, &decode(&local)?)?);
                            save(
                                db,
                                workspace,
                                &merged,
                                Some(&master),
                                !same(&merged, &master),
                            )?;
                        } else {
                            save(db, workspace, &master, Some(&master), false)?;
                        }
                    }
                    None => save(db, workspace, &master, Some(&master), false)?,
                }
            }
            revision(db, workspace)?;
            if let Some(checkpoint) = input.checkpoint.filter(|v| *v <= 9_007_199_254_740_991) {
                sql(db.execute("UPDATE client_draft_scopes SET checkpoint=max(checkpoint,?2) WHERE workspace=?1",params![workspace,checkpoint as i64]))?;
            }
            view(db, workspace).map(DraftResult::Snapshot)
        }
        "drafts.acknowledge" => {
            let sent: Vec<Document> =
                Vec::<Document>::deserialize(&p["documents"]).map_err(|e| e.to_string())?;
            for master in sent {
                decode(&master)?;
                if let Some((local, _, _)) = keyed_row(db, workspace, &master.id)? {
                    save(db, workspace, &local, Some(&master), !same(&local, &master))?;
                }
            }
            revision(db, workspace)?;
            view(db, workspace).map(DraftResult::Snapshot)
        }
        _ => Err("Unknown draft command.".into()),
    }
}

pub fn dispatch(db: &mut Connection, request: &Request) -> Response {
    let mut run = || -> Result<Response, String> {
        let transaction =
            sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
        let receipt = matches!(
            request.method.as_str(),
            "drafts.edit" | "drafts.dismiss" | "drafts.import"
        );
        if receipt && let Some(response) = sql(crate::store::replay(&transaction, request))? {
            return Ok(response);
        }
        let value = execute(&transaction, request)?;
        // Both result variants contain only JSON values and string-keyed maps.
        let value = serde_json::to_value(value).expect("draft result is JSON-serializable");
        let response = Response::success(request.id.clone(), value);
        if receipt {
            sql(crate::store::record(&transaction, request, &response))?;
        }
        sql(transaction.commit())?;
        Ok(response)
    };
    match run() {
        Ok(response) => response,
        Err(error) => Response::failure(request.id.clone(), "draft_error", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn draft_commands_sender_preserve_order_rollback_and_checkpoint_after_restart() {
        use crate::delivery::StoreCommand;
        use tokio::sync::{mpsc, oneshot};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receiver) = mpsc::channel::<StoreCommand>(8);
        let store_path = path.clone();
        let worker = std::thread::spawn(move || {
            let mut store = crate::store::Store::open(&store_path).unwrap();
            while let Some(command) = receiver.blocking_recv() {
                command.execute(&mut store);
            }
        });
        async fn send(
            sender: &mpsc::Sender<StoreCommand>,
            id: &str,
            method: &str,
            params: Value,
        ) -> Response {
            let (reply, receive) = oneshot::channel();
            sender
                .send(StoreCommand::Request(
                    Request {
                        id: id.into(),
                        method: method.into(),
                        params,
                    },
                    reply,
                ))
                .await
                .unwrap();
            receive.await.unwrap()
        }
        let workspace = "a".repeat(32);
        let invalid = send(
            &sender,
            "bad-workspace",
            "drafts.import",
            json!({"workspace":"bad","versions":null}),
        )
        .await;
        assert_eq!(invalid.error.unwrap().message, "Invalid draft workspace.");
        let invalid = send(
            &sender,
            "bad-edit",
            "drafts.edit",
            json!({"workspace":workspace,"changes":{"a":"first","b":null}}),
        )
        .await;
        assert_eq!(invalid.error.unwrap().message, "Invalid draft text.");
        let invalid = send(
            &sender,
            "bad-dismiss",
            "drafts.dismiss",
            json!({"workspace":workspace,"version":{"id":null,"text":null}}),
        )
        .await;
        assert_eq!(invalid.error.unwrap().message, "Missing draft identity.");
        let invalid = send(&sender, "bad-receive", "drafts.receive", json!({"workspace":workspace,"documents":[{"id":"d:r","payload":"not-json","seq":1}],"checkpoint":99})).await;
        assert!(invalid.error.is_some());
        drop(sender);
        worker.join().unwrap();
        let mut store = crate::store::Store::open(&path).unwrap();
        let pending = store.dispatch(Request {
            id: "pending".into(),
            method: "drafts.pending".into(),
            params: json!({"workspace":workspace}),
        });
        assert_eq!(pending.result, Some(json!({"rows":[],"checkpoint":0})));
    }

    #[test]
    fn borrowed_document_and_version_decode_preserves_legacy_errors() {
        for value in [
            json!(null),
            json!({}),
            json!([null]),
            json!([{"id":1}]),
            json!([["a"]]),
        ] {
            let old =
                serde_json::from_value::<Vec<Document>>(value.clone()).map_err(|e| e.to_string());
            let new = Vec::<Document>::deserialize(&value).map_err(|e| e.to_string());
            assert_eq!(old.err(), new.err(), "{value}");
            let old = serde_json::from_value::<Vec<DraftVersion>>(value.clone())
                .map_err(|e| e.to_string());
            let new = Vec::<DraftVersion>::deserialize(&value).map_err(|e| e.to_string());
            assert_eq!(old.err(), new.err(), "{value}");
        }
    }

    #[test]
    fn draft_input_scalars_keep_legacy_type_handling() {
        for value in [
            json!(null),
            json!(true),
            json!(1),
            json!(1.0),
            json!("text"),
            json!([]),
            json!({}),
        ] {
            let params = json!({"workspace":value,"error":value,"checkpoint":value});
            let input: DraftInput = project(&params);
            assert_eq!(input.workspace.as_deref(), value.as_str());
            assert_eq!(input.error.as_deref(), value.as_str());
            assert_eq!(input.checkpoint, value.as_u64());
        }
    }
    fn command(store: &mut crate::store::Store, id: &str, method: &str, mut p: Value) -> Value {
        p["workspace"] = json!("a".repeat(32));
        let response = store.dispatch(Request {
            id: id.into(),
            method: method.into(),
            params: p,
        });
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.unwrap()
    }
    #[test]
    fn snapshot_wire_contains_workspace_revision_and_error() {
        let mut store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let initial = command(&mut store, "initial", "drafts.view", json!({}));
        assert_eq!(
            initial,
            json!({"drafts":{}, "conflicts":[], "workspaceId":"a".repeat(32), "revision":0, "error":""})
        );
        let edited = command(
            &mut store,
            "edit",
            "drafts.edit",
            json!({"changes":{"room":"saved"}}),
        );
        assert_eq!(edited["revision"], 1);
        assert_eq!(edited["drafts"]["room"], "saved");
        let status = command(
            &mut store,
            "status",
            "drafts.sync_status",
            json!({"error":"offline"}),
        );
        assert_eq!(status["error"], "offline");
        assert_eq!(status["workspaceId"], "a".repeat(32));
        assert_eq!(status["revision"], 1);
    }

    #[test]
    fn draft_receive_normalizes_shared_legacy_fixtures_without_rewriting_storage() {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../../../contracts/draft-compatibility.json"))
                .unwrap();
        for case in cases {
            let mut store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
            let payload = case["payload"].to_string();
            let response = store.dispatch(Request {
                id: "receive".into(),
                method: "drafts.receive".into(),
                params: json!({"workspace":"a".repeat(32),"documents":[{
                    "id":"phone:chat","payload":payload,"seq":1,"_deleted":false
                }]}),
            });
            if let Some(updated) = case["updated"].as_u64() {
                assert!(
                    response.error.is_none(),
                    "{}: {:?}",
                    case["name"],
                    response.error
                );
                let stored = rows(&store.connection, &"a".repeat(32)).unwrap();
                assert_eq!(stored[0].0.payload, payload, "stored bytes must survive");
                let version = decode(&stored[0].0).unwrap();
                assert_eq!(version.updated, updated, "{}", case["name"]);
                assert_eq!(response.result.unwrap()["drafts"]["chat"], "x");
            } else {
                assert!(response.error.is_some(), "{}", case["name"]);
            }
        }
    }

    #[test]
    fn synchronization_decodes_only_the_keyed_draft_per_incoming_document() {
        const STORED: usize = 8;
        const BATCH: usize = 3;
        let workspace = "a".repeat(32);
        for method in ["drafts.receive", "drafts.acknowledge", "drafts.import"] {
            let mut store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
            let versions: Vec<DraftVersion> = (0..STORED)
                .map(|index| {
                    serde_json::from_value(json!({
                        "id":format!("device:room{index}"), "device":"device",
                        "session":format!("room{index}"), "text":"saved", "updated":1
                    }))
                    .unwrap()
                })
                .collect();
            let documents: Vec<_> = versions.iter().map(document).collect();
            for doc in &documents {
                save(&store.connection, &workspace, doc, Some(doc), false).unwrap();
            }
            // A matching ID in another workspace must never be read or changed.
            let other_workspace = "b".repeat(32);
            let mut other = documents[0].clone();
            other.payload = "unrelated invalid payload".into();
            save(&store.connection, &other_workspace, &other, None, true).unwrap();
            STORED_DRAFT_DECODES.set(0);
            let response = store.dispatch(Request {
                id: method.into(),
                method: method.into(),
                params: json!({"workspace":workspace, "documents":documents[..BATCH],
                    "versions":versions[..BATCH], "checkpoint":7}),
            });
            assert!(response.error.is_none(), "{method}: {:?}", response.error);
            // The response view decodes STORED records once. The batch itself
            // must decode only its BATCH matching records, independent of STORED.
            assert_eq!(STORED_DRAFT_DECODES.get() - STORED, BATCH, "{method}");
            assert_eq!(
                serde_json::to_value(&rows(&store.connection, &other_workspace).unwrap()[0].0)
                    .unwrap(),
                serde_json::to_value(&other).unwrap()
            );
            let result = response.result.unwrap();
            assert_eq!(result["drafts"]["room0"], "saved");
        }
    }
    #[test]
    fn imported_branches_keep_conflicts_and_retry_cannot_undo_later_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut store = crate::store::Store::open(&path).unwrap();
        let input = json!({"versions":[
            {"id":"a:room","device":"a","session":"room","text":"first","updated":1,"extension":{"kept":true}},
            {"id":"b:room","device":"b","session":"room","text":"second","updated":2}
        ]});
        let imported = command(&mut store, "import", "drafts.import", input.clone());
        assert_eq!(imported["drafts"]["room"], "second");
        assert_eq!(imported["conflicts"][0]["text"], "first");
        assert_eq!(imported["conflicts"][0]["extension"]["kept"], true);
        assert_eq!(
            command(&mut store, "pending", "drafts.pending", json!({}))["rows"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        command(
            &mut store,
            "edit",
            "drafts.edit",
            json!({"changes":{"room":"new edit"}}),
        );
        drop(store);
        let mut store = crate::store::Store::open(&path).unwrap();
        command(&mut store, "import", "drafts.import", input);
        let current = command(&mut store, "current", "drafts.view", json!({}));
        assert_eq!(current["drafts"]["room"], "new edit");
        assert!(current["conflicts"].as_array().unwrap().is_empty());
        let invalid = store.dispatch(Request {
            id: "bad-import".into(),
            method: "drafts.import".into(),
            params: json!({"workspace":"a".repeat(32),"versions":[
                {"id":"a:other","device":"a","session":"other","text":"must roll back","updated":1},
                {"id":"bad","device":"a","session":"room","text":"bad","updated":1}
            ]}),
        });
        assert!(invalid.error.is_some());
        let positional = store.dispatch(Request {
            id: "positional-import".into(),
            method: "drafts.import".into(),
            params: json!({"workspace":"a".repeat(32),"versions":[
                ["a:room", "room", "a", "text", 1]
            ]}),
        });
        assert!(positional.error.is_some());
        assert!(
            command(&mut store, "after-bad", "drafts.view", json!({}))["drafts"]
                .get("other")
                .is_none()
        );
    }
    #[test]
    fn pending_producer_reads_old_documents_before_emitting_typed_rows() {
        let mut store = crate::store::Store::open(std::path::Path::new(":memory:")).unwrap();
        let workspace = "a".repeat(32);
        let local = json!({"id":"device:room","payload":"{}","seq":7,"legacy":true});
        let master = json!({"id":"device:room","payload":"{}","seq":3,"_deleted":true,"extra":[1]});
        store
            .connection
            .execute(
                "INSERT INTO client_draft_branches VALUES(?1,?2,?3,?4,1)",
                params![
                    workspace,
                    "device:room",
                    local.to_string(),
                    master.to_string()
                ],
            )
            .unwrap();
        let pending = command(&mut store, "pending", "drafts.pending", json!({}));
        assert_eq!(
            pending["rows"],
            json!([{"newDocumentState":{"id":"device:room","payload":"{}","seq":7,"_deleted":false},"assumedMasterState":{"id":"device:room","payload":"{}","seq":3,"_deleted":true}}])
        );
    }

    #[test]
    fn late_push_ack_and_replayed_edit_cannot_erase_newer_text_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut store = crate::store::Store::open(&path).unwrap();
        command(
            &mut store,
            "first",
            "drafts.edit",
            json!({"changes":{"room":"first"}}),
        );
        let pending = command(&mut store, "pending", "drafts.pending", json!({}));
        command(
            &mut store,
            "second",
            "drafts.edit",
            json!({"changes":{"room":"second 🦀"}}),
        );
        drop(store);
        let mut store = crate::store::Store::open(&path).unwrap();
        command(
            &mut store,
            "ack",
            "drafts.acknowledge",
            json!({"documents":[pending["rows"][0]["newDocumentState"]]}),
        );
        command(
            &mut store,
            "first",
            "drafts.edit",
            json!({"changes":{"room":"first"}}),
        );
        let view = command(&mut store, "view", "drafts.view", json!({}));
        assert_eq!(view["drafts"]["room"], "second 🦀");
        assert_eq!(
            command(&mut store, "pending2", "drafts.pending", json!({}))["rows"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
