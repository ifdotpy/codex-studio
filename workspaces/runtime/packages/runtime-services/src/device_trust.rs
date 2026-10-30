//! The shared list of paired servers and trusted device keys (docs/decisions/
//! 2026-10-28-target-client-server-architecture.md, "Device identity").
//!
//! The list is the `runtime_access_clients` table itself. A paired `ui` row is a trusted
//! device key with its grant. A `revoked` `ui` row is the tombstone of a device key. A paired
//! `server` row is a server of the owner. Each server shares the list with its paired servers
//! through the `device_trust` orchestration action, and each side merges what it receives:
//!
//! - A device that one server trusts becomes a paired row on the others, so it reaches
//!   every server of the owner without a new pairing.
//! - A revocation is final. A merge never changes a revoked row back, and a merge turns a
//!   paired row into a revoked row. A returning device pairs again with a new key.
//! - The local row decides. A merge never changes the grant or the key of a paired row.
//!   The grant of a request comes from this row, never from the request.
//! - Only paired servers are shared. A server revocation stays local, as before.
//!
//! A row without a `grant` field is `full`: every pairing that exists before this list
//! keeps working. Each old pairing is its own device entry; two pairs of one device cannot
//! be proven, so they stay two devices.

use std::sync::Arc;

use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError};

use crate::multi_server_auth::{self as auth, AccessError, refused};
use crate::multi_server_orchestration::{ExchangeOutcome, OrchestrationDriver};

/// The orchestration action that carries the list. The reply is the receiver's list.
pub const ACTION: &str =
    studio_runtime_schema::persistence::ServerOperationKind::DeviceTrust.as_str();
pub const GRANT_FULL: &str = "full";
pub const GRANT_OBSERVE: &str = "observe";
/// Most entries in one list message (a count of rows).
const MAX_ENTRIES: usize = 4096;
const DEVICE_LABEL: &str = "Device";
const SERVER_LABEL: &str = "Paired server";

/// One row of the list on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    /// `ui` for a device, `server` for a paired server.
    pub kind: String,
    pub label: String,
    pub public_key: String,
    /// Servers only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Devices only: `full` or `observe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// Devices only: the tombstone of a revoked key.
    #[serde(default)]
    pub revoked: bool,
    /// Seconds since the epoch.
    pub created: f64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustList {
    pub entries: Vec<Entry>,
}

/// The grant of a stored row. A row without a grant is `full`.
pub fn grant_of(record: &Value) -> &'static str {
    match record.get("grant").and_then(Value::as_str) {
        Some(GRANT_OBSERVE) => GRANT_OBSERVE,
        _ => GRANT_FULL,
    }
}

/// The shared form of a stored row, or `None` for a row that is not shared.
pub fn entry_of(record: &Value) -> Option<Entry> {
    let text = |key: &str| record.get(key).and_then(Value::as_str);
    let kind = text("kind")?;
    let revoked = match text("status")? {
        "paired" => false,
        "revoked" => true,
        _ => return None,
    };
    if !matches!(kind, "ui" | "server") || (kind == "server" && revoked) {
        return None;
    }
    let device = kind == "ui";
    Some(Entry {
        id: text("id")?.to_owned(),
        kind: kind.to_owned(),
        label: text("label")?.to_owned(),
        public_key: text("publicKey")?.to_owned(),
        origin: if device {
            None
        } else {
            text("origin").map(str::to_owned)
        },
        grant: device.then(|| grant_of(record).to_owned()),
        revoked,
        created: record.get("created").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

/// The entry in the form this server stores, or `None` when it is invalid for this server.
fn checked(entry: &Entry, local_server: &str) -> Option<Entry> {
    let mut entry = entry.clone();
    if !auth::identifier(&entry.id)
        || entry.public_key.is_empty()
        || entry.public_key.len() > 4096
        || !entry.created.is_finite()
        || entry.created < 0.0
    {
        return None;
    }
    let label: String = entry.label.trim().chars().take(80).collect();
    match entry.kind.as_str() {
        "ui" => {
            if !matches!(entry.grant.as_deref(), Some(GRANT_FULL | GRANT_OBSERVE)) {
                return None;
            }
            entry.origin = None;
            entry.label = if label.is_empty() {
                DEVICE_LABEL.to_owned()
            } else {
                label
            };
        }
        "server" => {
            if entry.revoked || entry.id == local_server {
                return None;
            }
            entry.origin = Some(auth::normalize_origin(entry.origin.as_deref()?).ok()?);
            entry.grant = None;
            entry.label = if label.is_empty() {
                SERVER_LABEL.to_owned()
            } else {
                label
            };
        }
        _ => return None,
    }
    Some(entry)
}

fn new_record(entry: &Entry, now: f64) -> Value {
    let server = entry.kind == "server";
    let mut record = json!({
        "id": entry.id,
        "clientId": entry.id,
        "serverId": if server { Some(entry.id.clone()) } else { None },
        "kind": entry.kind,
        "label": entry.label,
        "origin": entry.origin,
        "publicKey": entry.public_key,
        "tailscaleUser": Value::Null,
        "status": if entry.revoked { "revoked" } else { "paired" },
        "created": entry.created,
        "lastAccess": Value::Null,
        "revoked": if entry.revoked { json!(now) } else { Value::Null },
    });
    if let Some(grant) = &entry.grant {
        record["grant"] = json!(grant);
    }
    record
}

/// The row to store after a merge of `entry` into `local`, or `None` when the row stays.
/// `entry` must come from `checked`.
pub fn merge_into(local: Option<&Value>, entry: &Entry, now: f64) -> Option<Value> {
    let status = local
        .and_then(|record| record.get("status"))
        .and_then(Value::as_str);
    match (local, status) {
        // A tombstone is final.
        (Some(_), Some("revoked")) => None,
        (Some(record), Some("paired")) => {
            let same = record.get("publicKey").and_then(Value::as_str)
                == Some(entry.public_key.as_str())
                && record.get("kind").and_then(Value::as_str) == Some(entry.kind.as_str());
            if !(same && entry.revoked && entry.kind == "ui") {
                return None;
            }
            let mut record = record.clone();
            record["status"] = json!("revoked");
            record["revoked"] = json!(now);
            Some(record)
        }
        (Some(_), _) => None,
        (None, _) => Some(new_record(entry, now)),
    }
}

/// Every shared row of this server, ordered by id.
pub fn read_entries(tx: &Transaction<'_>) -> Result<Vec<Entry>, StoreError> {
    let mut statement = tx.prepare("SELECT record FROM runtime_access_clients ORDER BY id")?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows
        .iter()
        .filter_map(|raw| serde_json::from_str::<Value>(raw).ok())
        .filter_map(|record| entry_of(&record))
        .collect())
}

/// Merge `entries` from the server `actor` into the table. A row that does not change is
/// not written.
pub fn apply(
    tx: &Transaction<'_>,
    local_server: &str,
    actor: &str,
    entries: &[Entry],
    now: f64,
) -> Result<(), StoreError> {
    for entry in entries
        .iter()
        .filter_map(|entry| checked(entry, local_server))
    {
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_access_clients WHERE id=?1",
                [&entry.id],
                |row| row.get(0),
            )
            .optional()?;
        let local = match raw.as_deref().map(serde_json::from_str::<Value>) {
            None => None,
            Some(Ok(record)) => Some(record),
            // A row this code cannot read is never replaced by a merge.
            Some(Err(_)) => continue,
        };
        let Some(record) = merge_into(local.as_ref(), &entry, now) else {
            continue;
        };
        tx.execute(
            "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![entry.id, record.to_string()],
        )?;
        tx.execute(
            "INSERT INTO runtime_access_audit(client,actor,action,created) VALUES(?1,?2,?3,?4)",
            params![
                entry.id,
                actor,
                if entry.revoked { "revoke" } else { "trust" },
                now
            ],
        )?;
    }
    Ok(())
}

/// Receiver side of the `device_trust` action: merge the sender's list and answer with
/// this server's list.
pub fn receive(
    tx: &Transaction<'_>,
    local_server: &str,
    sender: &str,
    payload: &Value,
    now: f64,
) -> Result<TrustList, AccessError> {
    let list = TrustList::deserialize(payload)
        .ok()
        .filter(|list| list.entries.len() <= MAX_ENTRIES)
        .ok_or_else(|| refused(400, "invalid_request", "Invalid device trust list"))?;
    apply(tx, local_server, sender, &list.entries, now)?;
    Ok(TrustList {
        entries: read_entries(tx)?,
    })
}

/// A short fingerprint of a list; equal lists give equal fingerprints.
pub fn digest(entries: &[Entry]) -> String {
    studio_runtime_store::sha256_hex(
        serde_json::to_string(entries)
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// The current list of this server.
pub async fn read(store: &RuntimeStore) -> Result<Vec<Entry>, StoreError> {
    store
        .read_async("runtime-services/device_trust", |tx| read_entries(tx))
        .await
}

/// Send this server's list to the paired server `server` and merge its answer. Returns the
/// fingerprint of the merged local list, or `None` when the server did not answer. The
/// merge is idempotent, so a lost answer is repaired by the next call.
pub async fn sync_peer(
    driver: &OrchestrationDriver,
    store: &Arc<RuntimeStore>,
    server: &str,
) -> Option<String> {
    let entries = read(store).await.ok()?;
    let key = studio_runtime_schema::persistence::RequestIdentity::issue_root();
    let now = crate::event_write::unix_time().ok()?;
    let reply = match driver
        .exchange(
            server,
            ACTION,
            &json!({ "entries": entries }),
            &key,
            now as i64,
        )
        .await
    {
        Ok(ExchangeOutcome::Applied(reply)) => reply,
        _ => return None,
    };
    let theirs: TrustList = serde_json::from_str(reply.value.get()).ok()?;
    if theirs.entries.len() > MAX_ENTRIES {
        return None;
    }
    let local_server = store.local_server_id().ok().flatten().unwrap_or_default();
    let server = server.to_owned();
    let merged = store
        .transaction_with(move |tx| {
            apply(tx, &local_server, &server, &theirs.entries, now)?;
            read_entries(tx)
        })
        .await
        .ok()?;
    Some(digest(&merged))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::multi_server_auth::{AccessSnapshot, SignedRequest};
    use rusqlite::Connection;

    const ORIGIN: &str = "https://server-c.tail1234.ts.net";
    const OLD_1: &str = "2f0c9f5e-0000-4000-8000-000000000001";
    const OLD_2: &str = "2f0c9f5e-0000-4000-8000-000000000002";

    /// One server's table.
    struct Server {
        id: &'static str,
        db: Connection,
    }

    fn entry(grant: &str, revoked: bool) -> Entry {
        Entry {
            id: "dev-phone".into(),
            kind: "ui".into(),
            label: "Phone".into(),
            public_key: "key-phone".into(),
            origin: None,
            grant: Some(grant.into()),
            revoked,
            created: 1.0,
        }
    }

    impl Server {
        fn new(id: &'static str) -> Self {
            let db = Connection::open_in_memory().unwrap();
            db.execute_batch(
                "CREATE TABLE runtime_access_clients (id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_access_audit (
                   seq INTEGER PRIMARY KEY AUTOINCREMENT, client TEXT NOT NULL,
                   actor TEXT NOT NULL, action TEXT NOT NULL, created REAL NOT NULL);",
            )
            .unwrap();
            Self { id, db }
        }
        /// Pair a device here, as the pairing route stores it.
        fn pair(&self, id: &str, key: &str, grant: Option<&str>) {
            let mut record = json!({
                "id": id, "clientId": id, "serverId": null, "kind": "ui", "label": "Phone",
                "origin": null, "publicKey": key, "tailscaleUser": "owner",
                "status": "paired", "created": 1.0, "lastAccess": null, "revoked": null,
            });
            if let Some(grant) = grant {
                record["grant"] = json!(grant);
            }
            self.put(id, &record);
        }
        fn put(&self, id: &str, record: &Value) {
            self.db
                .execute(
                    "INSERT OR REPLACE INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    params![id, record.to_string()],
                )
                .unwrap();
        }
        fn row(&self, id: &str) -> Option<Value> {
            self.db
                .query_row(
                    "SELECT record FROM runtime_access_clients WHERE id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .unwrap()
                .map(|raw| serde_json::from_str(&raw).unwrap())
        }
        /// The revoke action of `multi_server::manage`.
        fn revoke(&self, id: &str, now: f64) {
            let mut record = self.row(id).unwrap();
            record["status"] = json!("revoked");
            record["revoked"] = json!(now);
            self.put(id, &record);
        }
        fn list(&mut self) -> TrustList {
            let tx = self.db.transaction().unwrap();
            TrustList {
                entries: read_entries(&tx).unwrap(),
            }
        }
        /// Receive the list of `from`, as the action does.
        fn take(&mut self, from: &str, list: &TrustList) {
            let tx = self.db.transaction().unwrap();
            receive(
                &tx,
                self.id,
                from,
                &serde_json::to_value(list).unwrap(),
                100.0,
            )
            .unwrap();
            tx.commit().unwrap();
        }
        /// One exchange with `peer`: send this list, merge the answer.
        fn sync(&mut self, peer: &mut Server) {
            let mine = self.list();
            peer.take(self.id, &mine);
            let theirs = peer.list();
            let tx = self.db.transaction().unwrap();
            apply(&tx, self.id, peer.id, &theirs.entries, 100.0).unwrap();
            tx.commit().unwrap();
        }
        fn status(&self, id: &str) -> Option<String> {
            self.row(id)
                .map(|record| record["status"].as_str().unwrap().to_owned())
        }
        /// Whether the request path knows the client: only the signature of the test
        /// request is invalid, so that refusal is the one that means "known".
        fn admits(&self, id: &str) -> bool {
            let raw = self.row(id).map(|record| record.to_string());
            let snapshot =
                AccessSnapshot::from_records(id, raw.as_deref(), "", None, false).unwrap();
            let headers = [
                ("x-studio-client", id),
                ("x-studio-server", self.id),
                ("x-studio-timestamp", "1000000"),
                ("x-studio-nonce", "abcdefghijklmnopqrstuvwxyz012345"),
                ("x-studio-request-id", "request-1"),
                ("x-studio-signature", "unused"),
            ]
            .map(|(name, value)| (name.to_owned(), value.to_owned()));
            let request = SignedRequest::from_headers("GET", "/api/x", &headers, b"").unwrap();
            match crate::multi_server_auth::verify_before_observe(
                &snapshot,
                &request,
                self.id,
                1_000_000.0,
            ) {
                Err(AccessError::Refused { code, .. }) => code == "invalid_signature",
                Ok(()) => true,
                Err(_) => false,
            }
        }
    }

    #[test]
    fn trust_on_server_a_reaches_server_b_with_its_grant() {
        let (mut a, mut b) = (Server::new("server-a"), Server::new("server-b"));
        a.pair("dev-phone", "key-phone", None);
        a.pair("dev-watch", "key-watch", Some(GRANT_OBSERVE));
        assert!(!b.admits("dev-phone"));
        a.sync(&mut b);
        assert!(b.admits("dev-phone"));
        assert_eq!(grant_of(&b.row("dev-phone").unwrap()), GRANT_FULL);
        assert_eq!(grant_of(&b.row("dev-watch").unwrap()), GRANT_OBSERVE);
        assert_eq!(b.row("dev-phone").unwrap()["publicKey"], "key-phone");
    }

    #[test]
    fn trust_spreads_to_a_server_that_only_pairs_with_b() {
        let (mut a, mut b, mut c) = (
            Server::new("server-a"),
            Server::new("server-b"),
            Server::new("server-c"),
        );
        a.pair("dev-phone", "key-phone", None);
        a.sync(&mut b);
        b.sync(&mut c);
        assert!(c.admits("dev-phone"));
    }

    #[test]
    fn revocation_on_b_reaches_a_and_wins_over_a_concurrent_re_add() {
        let (mut a, mut b) = (Server::new("server-a"), Server::new("server-b"));
        a.pair("dev-phone", "key-phone", None);
        a.sync(&mut b);
        // Concurrent: B revokes while A, which has not heard of it, still lists the key.
        b.revoke("dev-phone", 50.0);
        let stale = a.list();
        assert!(!stale.entries[0].revoked);
        // The stale list from A reaches B: the key stays revoked.
        b.take("server-a", &stale);
        assert_eq!(b.status("dev-phone").as_deref(), Some("revoked"));
        assert!(!b.admits("dev-phone"));
        // The list of B reaches A: the key is revoked there too.
        a.sync(&mut b);
        assert_eq!(a.status("dev-phone").as_deref(), Some("revoked"));
        assert!(!a.admits("dev-phone"));
        // A later add of the same key, on either side, changes nothing.
        let add = TrustList {
            entries: vec![entry(GRANT_FULL, false)],
        };
        a.take("server-b", &add);
        b.take("server-a", &add);
        assert!(!a.admits("dev-phone") && !b.admits("dev-phone"));
    }

    #[test]
    fn a_revocation_for_a_key_a_server_never_saw_is_kept_as_a_tombstone() {
        let (mut a, mut b, mut c) = (
            Server::new("server-a"),
            Server::new("server-b"),
            Server::new("server-c"),
        );
        a.pair("dev-phone", "key-phone", None);
        a.revoke("dev-phone", 50.0);
        a.sync(&mut b);
        assert_eq!(b.status("dev-phone").as_deref(), Some("revoked"));
        b.sync(&mut c);
        // C then hears an add from a stale copy: the tombstone wins.
        c.take(
            "server-x",
            &TrustList {
                entries: vec![entry(GRANT_FULL, false)],
            },
        );
        assert!(!c.admits("dev-phone"));
    }

    #[test]
    fn a_device_unknown_to_a_server_is_refused() {
        let a = Server::new("server-a");
        assert!(!a.admits("dev-stranger"));
    }

    #[test]
    fn a_merge_never_changes_the_key_or_the_grant_of_a_paired_row() {
        let (mut a, mut b) = (Server::new("server-a"), Server::new("server-b"));
        a.pair("dev-phone", "key-phone", Some(GRANT_OBSERVE));
        b.pair("dev-phone", "other-key", None);
        a.sync(&mut b);
        let row = b.row("dev-phone").unwrap();
        assert_eq!(row["publicKey"], "other-key");
        assert_eq!(grant_of(&row), GRANT_FULL);
    }

    #[test]
    fn rows_from_before_the_list_keep_working_and_stay_separate_devices() {
        let (mut a, mut b) = (Server::new("server-a"), Server::new("server-b"));
        // Two pairs made before the list: no grant field, random client ids, two keys.
        a.pair(OLD_1, "key-1", None);
        a.pair(OLD_2, "key-2", None);
        for id in [OLD_1, OLD_2] {
            assert!(a.admits(id), "the old pairing keeps working on A");
            assert_eq!(grant_of(&a.row(id).unwrap()), GRANT_FULL);
        }
        let list = a.list();
        assert_eq!(list.entries.len(), 2);
        assert!(
            list.entries
                .iter()
                .all(|entry| entry.grant.as_deref() == Some(GRANT_FULL))
        );
        a.sync(&mut b);
        assert!(b.admits(OLD_1) && b.admits(OLD_2));
        // Each stays a device of its own: revoking one leaves the other.
        b.revoke(OLD_1, 60.0);
        b.sync(&mut a);
        assert!(!a.admits(OLD_1));
        assert!(a.admits(OLD_2));
    }

    #[test]
    fn paired_servers_are_shared_but_their_revocation_is_not() {
        let (mut a, mut b) = (Server::new("server-a"), Server::new("server-b"));
        let server = json!({
            "id": "server-c", "clientId": "server-c", "serverId": "server-c", "kind": "server",
            "label": "C", "origin": ORIGIN, "publicKey": "key-c", "tailscaleUser": "owner",
            "status": "paired", "created": 1.0, "lastAccess": null, "revoked": null,
        });
        a.put("server-c", &server);
        a.sync(&mut b);
        assert_eq!(b.status("server-c").as_deref(), Some("paired"));
        assert_eq!(b.row("server-c").unwrap()["origin"], ORIGIN);
        let mut revoked = server;
        revoked["status"] = json!("revoked");
        a.put("server-c", &revoked);
        a.sync(&mut b);
        assert_eq!(b.status("server-c").as_deref(), Some("paired"));
    }

    #[test]
    fn invalid_entries_are_ignored() {
        let mut a = Server::new("server-a");
        let edit = |change: &dyn Fn(&mut Entry)| {
            let mut value = entry(GRANT_FULL, false);
            change(&mut value);
            value
        };
        let entries = vec![
            edit(&|e| e.grant = Some("root".into())),
            edit(&|e| e.grant = None),
            edit(&|e| e.id = "../x".into()),
            edit(&|e| e.public_key = String::new()),
            edit(&|e| e.kind = "agent".into()),
            edit(&|e| {
                e.kind = "server".into();
                e.id = "server-a".into();
                e.origin = Some(ORIGIN.into());
            }),
        ];
        a.take("server-b", &TrustList { entries });
        assert!(a.list().entries.is_empty());
    }
}
