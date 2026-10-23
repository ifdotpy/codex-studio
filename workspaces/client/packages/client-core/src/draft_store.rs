//! Draft edits commit before acknowledgment. Replication cannot erase a newer edit.
use crate::{
    drafts::{self, DraftVersion},
    projections::Document,
    protocol::{Request, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

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
    // Value's ordered map matches the server's canonical key order.
    Document {
        id: version.id.clone(),
        seq: 0,
        deleted: false,
        payload: serde_json::to_value(version).unwrap().to_string(),
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
    sql(db.execute("INSERT INTO client_draft_branches VALUES(?1,?2,?3,?4,?5) ON CONFLICT(workspace,id) DO UPDATE SET document=excluded.document,master=excluded.master,dirty=excluded.dirty",
        params![workspace,local.id,serde_json::to_string(local).unwrap(),master.map(|v|serde_json::to_string(v).unwrap()),dirty]))?;
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

pub fn view(db: &Connection, workspace: &str) -> Result<Value, String> {
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
    let mut value =
        serde_json::to_value(drafts::reconcile(&versions, &BTreeMap::new(), &dismissed)?).unwrap();
    let revision = sql(db
        .query_row(
            "SELECT revision FROM client_draft_scopes WHERE workspace=?1",
            [workspace],
            |r| crate::store::unsigned(r, 0),
        )
        .optional())?
    .unwrap_or(0);
    value["revision"] = json!(revision);
    value["workspaceId"] = json!(workspace);
    value["error"] = json!(
        sql(db
            .query_row(
                "SELECT error FROM client_draft_sync WHERE workspace=?1",
                [workspace],
                |r| r.get::<_, String>(0)
            )
            .optional())?
        .unwrap_or_default()
    );
    Ok(value)
}

fn execute(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    let workspace = p["workspace"]
        .as_str()
        .filter(|v| crate::projections::valid_workspace(v))
        .ok_or("Invalid draft workspace.")?;
    match request.method.as_str() {
        "drafts.view" => view(db, workspace),
        "drafts.sync_status" => {
            let message = p["error"].as_str().unwrap_or("");
            sql(db.execute("INSERT INTO client_draft_sync VALUES(?1,?2) ON CONFLICT(workspace) DO UPDATE SET error=excluded.error",params![workspace,message]))?;
            view(db, workspace)
        }
        "drafts.import" => {
            let versions: Vec<DraftVersion> =
                serde_json::from_value(p["versions"].clone()).map_err(|e| e.to_string())?;
            if versions.len() > 10000 {
                return Err("Too many saved draft branches.".into());
            }
            for version in versions {
                let incoming = document(&version);
                decode(&incoming)?;
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
            view(db, workspace)
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
                let text = text
                    .as_str()
                    .filter(|text| text.chars().count() <= 2_000_000)
                    .ok_or("Invalid draft text.")?;
                let records = rows(db, workspace)?;
                let versions = records
                    .iter()
                    .filter(|(d, _, _)| !d.deleted)
                    .map(|(d, _, _)| decode(d))
                    .collect::<Result<Vec<_>, _>>()?;
                let version = drafts::edit(&device, session, text.into(), &versions, now)?;
                let master = records
                    .iter()
                    .find(|(d, _, _)| d.id == version.id)
                    .and_then(|(_, master, _)| master.as_ref());
                save(db, workspace, &document(&version), master, true)?;
            }
            revision(db, workspace)?;
            view(db, workspace)
        }
        "drafts.dismiss" => {
            let version = &p["version"];
            let (id, text) = (
                version["id"].as_str().ok_or("Missing draft identity.")?,
                version["text"].as_str().ok_or("Missing draft text.")?,
            );
            sql(db.execute(
                "INSERT OR IGNORE INTO client_draft_dismissed VALUES(?1,?2,?3)",
                params![workspace, id, text],
            ))?;
            revision(db, workspace)?;
            view(db, workspace)
        }
        "drafts.pending" => {
            let rows=rows(db,workspace)?.into_iter().filter(|(_,_,dirty)|*dirty).take(100).map(|(local,master,_)|json!({"newDocumentState":local,"assumedMasterState":master})).collect::<Vec<_>>();
            let checkpoint = sql(db
                .query_row(
                    "SELECT checkpoint FROM client_draft_scopes WHERE workspace=?1",
                    [workspace],
                    |r| crate::store::unsigned(r, 0),
                )
                .optional())?
            .unwrap_or(0);
            Ok(json!({"rows":rows,"checkpoint":checkpoint}))
        }
        "drafts.receive" => {
            let incoming: Vec<Document> =
                serde_json::from_value(p["documents"].clone()).map_err(|e| e.to_string())?;
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
            if let Some(checkpoint) = p["checkpoint"]
                .as_u64()
                .filter(|v| *v <= 9_007_199_254_740_991)
            {
                sql(db.execute("UPDATE client_draft_scopes SET checkpoint=max(checkpoint,?2) WHERE workspace=?1",params![workspace,checkpoint as i64]))?;
            }
            view(db, workspace)
        }
        "drafts.acknowledge" => {
            let sent: Vec<Document> =
                serde_json::from_value(p["documents"].clone()).map_err(|e| e.to_string())?;
            for master in sent {
                decode(&master)?;
                if let Some((local, _, _)) = keyed_row(db, workspace, &master.id)? {
                    save(db, workspace, &local, Some(&master), !same(&local, &master))?;
                }
            }
            revision(db, workspace)?;
            view(db, workspace)
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
            {"id":"a:room","device":"a","session":"room","text":"first","updated":1},
            {"id":"b:room","device":"b","session":"room","text":"second","updated":2}
        ]});
        let imported = command(&mut store, "import", "drafts.import", input.clone());
        assert_eq!(imported["drafts"]["room"], "second");
        assert_eq!(imported["conflicts"][0]["text"], "first");
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
        assert!(
            command(&mut store, "after-bad", "drafts.view", json!({}))["drafts"]
                .get("other")
                .is_none()
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
