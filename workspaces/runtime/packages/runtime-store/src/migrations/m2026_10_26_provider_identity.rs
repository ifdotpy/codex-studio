//! Dated provider identity migration; ordinary readers accept only explicit providers.

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use serde_json::Value;
use studio_runtime_schema::ProviderKind;
use studio_runtime_schema::shared_values::TITLE_EFFECT_SIGNATURE;

use crate::{StoreError, StoreResult};

pub(super) type Accounts = BTreeMap<String, ProviderKind>;
type Patches = Vec<(String, ProviderKind)>;

/// Existing analytics metadata key proving provider identity migration completed; format tag.
const ANALYTICS_PROVIDER_FORMAT_KEY: &str = "providerIdentityFormat";
const PROVIDER_IDENTITY_FORMAT: &str = "2026-10-26";
/// The only historical registry version accepted by the dated migration.
const LEGACY_REGISTRY_FORMAT_VERSION: u32 = 1;

/// Original provider leaf for a title fingerprint awaiting dated conversion; JSON field name.
pub const TITLE_FINGERPRINT_PROVIDER_2026_10_26: &str = "titleFingerprintProvider20261026";

fn invalid(source: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("provider identity migration 2026-10-26: {source}"))
}

fn historical_provider(value: Option<&Value>, fallback: ProviderKind) -> StoreResult<ProviderKind> {
    match value {
        None | Some(Value::Null) => Ok(fallback),
        Some(value) => ProviderKind::of_field(Some(value)).map_err(|_| invalid("unknown provider")),
    }
}

/// The 2026-10-26 registry upgrade, before strict account-policy decoding.
/// Remove only after supported states and restorable backups have crossed this
/// format, or upgrades from older formats are explicitly retired.
pub fn migrate_registry_2026_10_26(value: &mut Value) -> StoreResult<bool> {
    let current = studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION;
    let legacy = match value.get("version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(LEGACY_REGISTRY_FORMAT_VERSION) => true,
        Some(version) if version == u64::from(current) => false,
        _ => return Err(invalid("unsupported registry version")),
    };
    let accounts = value
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid("registry accounts must be an object"))?;
    for account in accounts.values_mut() {
        let object = account
            .as_object_mut()
            .ok_or_else(|| invalid("account must be an object"))?;
        let provider = if legacy {
            historical_provider(object.get("provider"), ProviderKind::Codex)?
        } else {
            ProviderKind::of_field(object.get("provider"))
                .map_err(|_| invalid("current account requires explicit provider"))?
        };
        if object.get("provider").is_none_or(Value::is_null) {
            object.insert("provider".into(), Value::String(provider.key().into()));
        }
    }
    if legacy {
        value["version"] = Value::from(current);
    }
    if let Some((pointer, _)) =
        studio_runtime_schema::first_error("account-registry/AccountRegistryRecord", value)
            .map_err(StoreError::SchemaValidation)?
    {
        return Err(invalid(&format!("invalid registry at {pointer}")));
    }
    Ok(legacy)
}

#[derive(Clone, Copy)]
enum ProviderFormat {
    Historical,
    Current,
}

fn provider_patch(
    value: &Value,
    path: &str,
    field: &str,
    fallback: ProviderKind,
    patches: &mut Patches,
    format: ProviderFormat,
) -> StoreResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid(&format!("{path} must be an object")))?;
    let provider = match format {
        ProviderFormat::Historical => historical_provider(object.get(field), fallback)?,
        ProviderFormat::Current => ProviderKind::of_field(object.get(field))
            .map_err(|_| invalid("current identity requires an explicit known provider"))?,
    };
    if object.get(field).is_none_or(Value::is_null) {
        patches.push((format!("{path}.{field}"), provider));
    }
    Ok(())
}

fn agent_patches(
    value: &Value,
    path: &str,
    accounts: &Accounts,
    patches: &mut Patches,
    format: ProviderFormat,
) -> StoreResult<()> {
    provider_patch(
        value,
        path,
        "provider",
        ProviderKind::Codex,
        patches,
        format,
    )?;
    if let Some(history) = value.get("accountHistory").filter(|v| !v.is_null()) {
        let history = history
            .as_array()
            .ok_or_else(|| invalid("accountHistory must be an array"))?;
        for (index, record) in history.iter().enumerate() {
            let owner = record
                .get("accountKey")
                .and_then(Value::as_str)
                .and_then(|key| accounts.get(key))
                .copied()
                .unwrap_or(ProviderKind::Codex);
            provider_patch(
                record,
                &format!("{path}.accountHistory[{index}]"),
                "provider",
                owner,
                patches,
                format,
            )?;
        }
    }
    if let Some(lazy) = value.get("lazyAccountTransfer").filter(|v| !v.is_null())
        && let Some(source) = lazy.get("sourceState")
    {
        provider_patch(
            source,
            &format!("{path}.lazyAccountTransfer.sourceState"),
            "provider",
            ProviderKind::Codex,
            patches,
            format,
        )?;
    }
    Ok(())
}

fn patches(
    table: &str,
    value: &Value,
    accounts: &Accounts,
    format: ProviderFormat,
) -> StoreResult<Patches> {
    let mut patches = Vec::new();
    match table {
        "runtime_agents" | "analytics_agents" => {
            agent_patches(value, "$", accounts, &mut patches, format)?
        }
        "runtime_account_transfers" => {
            let target = value
                .get("targetAccountKey")
                .and_then(Value::as_str)
                .and_then(|key| accounts.get(key))
                .copied()
                .unwrap_or(ProviderKind::Codex);
            provider_patch(value, "$", "targetProvider", target, &mut patches, format)?;
            let members = value
                .get("members")
                .and_then(Value::as_object)
                .ok_or_else(|| invalid("transfer members"))?;
            for (id, member) in members {
                let path = format!("$.members.{}", serde_json::to_string(id)?);
                provider_patch(
                    member,
                    &path,
                    "provider",
                    ProviderKind::Codex,
                    &mut patches,
                    format,
                )?;
                if let Some(settings) = member.get("settings").filter(|v| !v.is_null()) {
                    provider_patch(
                        settings,
                        &format!("{path}.settings"),
                        "provider",
                        ProviderKind::Codex,
                        &mut patches,
                        format,
                    )?;
                }
            }
        }
        "runtime_analytics_projection" => {
            for (kind, payload) in value
                .as_object()
                .ok_or_else(|| invalid("analytics projection"))?
            {
                let observation = payload.get("observation").unwrap_or(payload);
                if let Some(agent) = observation.get("agent").filter(|v| !v.is_null()) {
                    let path = if payload.get("observation").is_some() {
                        format!("$.{kind}.observation.agent")
                    } else {
                        format!("$.{kind}.agent")
                    };
                    agent_patches(agent, &path, accounts, &mut patches, format)?;
                }
            }
        }
        _ => unreachable!("migration table owner"),
    }
    Ok(patches)
}

fn table_updates(
    db: &Connection,
    table: &'static str,
    accounts: &Accounts,
) -> StoreResult<Vec<(i64, String)>> {
    if !crate::schema::table_exists(db, table)? {
        return Ok(Vec::new());
    }
    let mut query = db.prepare(&format!("SELECT rowid,record FROM {table} ORDER BY rowid"))?;
    let rows = query.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut updates = Vec::new();
    for row in rows {
        let (id, mut raw) = row?;
        let value: Value = serde_json::from_str(&raw)?;
        let changes = patches(table, &value, accounts, ProviderFormat::Historical)
            .map_err(|error| invalid(&format!("{table} rowid {id}: {error}")))?;
        if changes.is_empty() {
            continue;
        }
        // SQLite edits only the named identity leaves, retaining observable member order.
        for (path, provider) in changes {
            raw = db.query_row(
                "SELECT json_set(?1,?2,?3)",
                rusqlite::params![raw, path, provider.key()],
                |row| row.get(0),
            )?;
        }
        updates.push((id, raw));
    }
    Ok(updates)
}

/// Stream only the affected identity families. Current data never enters the
/// historical defaulting branch and malformed identity stops startup unchanged.
fn validate_current_table(
    db: &Connection,
    table: &'static str,
    accounts: &Accounts,
) -> StoreResult<()> {
    if !crate::schema::table_exists(db, table)? {
        return Ok(());
    }
    let mut statement = db.prepare(&format!("SELECT rowid,record FROM {table} ORDER BY rowid"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let raw: String = row.get(1)?;
        let value = serde_json::from_str(&raw)?;
        patches(table, &value, accounts, ProviderFormat::Current).map_err(|_| {
            StoreError::InvalidStoredRecord {
                object: table,
                id: id.to_string(),
            }
        })?;
    }
    Ok(())
}

fn legacy_orphan_usage_updates(db: &Connection) -> StoreResult<Vec<(i64, String)>> {
    let mut statement = db.prepare(
        "SELECT usage.rowid,usage.record FROM analytics_usage AS usage
         WHERE json_type(usage.record,'$.studioUsage') IS NULL
           AND NOT EXISTS (SELECT 1 FROM analytics_agents AS agent
             WHERE agent.id=json_extract(usage.record,'$.agentId'))
         ORDER BY usage.rowid",
    )?;
    let mut rows = statement.query([])?;
    let mut updates = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let raw: String = row.get(1)?;
        let value: Value = serde_json::from_str(&raw)?;
        // Before this cutover the cost reader decoded ownerless rows as Codex.
        // Complete evidence becomes neutral; sparse categories remain unknown.
        let provider = match value.get("provider") {
            Some(provider) => ProviderKind::of_field(Some(provider))
                .map_err(|_| invalid("unknown analytics usage provider"))?,
            None => ProviderKind::Codex,
        };
        let observation = studio_runtime_schema::usage::persisted::parse_analytics_cost_usage_row(
            provider, &value,
        )
        .map_err(|error| invalid(&format!("analytics_usage rowid {id}: {error}")))?;
        let updated = if let Some(observation) = observation {
            db.query_row(
                "SELECT json_set(?1,'$.studioUsage',json(?2))",
                rusqlite::params![raw, serde_json::to_string(&observation)?],
                |row| row.get(0),
            )?
        } else {
            db.query_row(
                "SELECT json_set(?1,'$.provider',?2)",
                rusqlite::params![raw, provider.key()],
                |row| row.get(0),
            )?
        };
        updates.push((id, updated));
    }
    Ok(updates)
}

const CANVAS_TABLES: [&str; 3] = [
    "runtime_agents",
    "runtime_account_transfers",
    "runtime_analytics_projection",
];

/// Validate all sources before startup performs migration or recovery writes.
pub(crate) fn validate_2026_10_26(root: &Path, accounts: &Accounts) -> StoreResult<()> {
    use studio_runtime_schema::studio_errors::{
        MigrationPhase, MigrationRollbackOutcome, MigrationTarget,
    };
    for (database, tables, target) in [
        (
            "canvas.sqlite3",
            CANVAS_TABLES.as_slice(),
            MigrationTarget::Canvas,
        ),
        (
            "analytics.sqlite3",
            &["analytics_agents"][..],
            MigrationTarget::Analytics,
        ),
    ] {
        let definition = super::catalog()
            .find(|entry| entry.target == target)
            .ok_or(StoreError::RecordContract(
                "provider target missing from migration catalog",
            ))?;
        let result = (|| -> StoreResult<()> {
            let db =
                Connection::open_with_flags(root.join(database), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let legacy = if target == MigrationTarget::Canvas {
                !crate::agent_columns::has_explicit_provider_column(&db)?
            } else {
                analytics_needs_migration(&db)?
            };
            for table in tables {
                if legacy {
                    table_updates(&db, table, accounts)?;
                } else {
                    validate_current_table(&db, table, accounts)?;
                }
            }
            if legacy && target == MigrationTarget::Analytics {
                legacy_orphan_usage_updates(&db)?;
            }
            Ok(())
        })();
        result.map_err(|source| {
            super::failure(
                definition.id,
                target,
                MigrationPhase::Validate,
                MigrationRollbackOutcome::NotRequired,
                source,
            )
        })?;
    }
    Ok(())
}

/// 2026-10-26 upgrade in the existing Canvas startup transaction, before strict columns.
/// Remove only after supported states and restorable backups crossed this format,
/// or upgrades from older formats are explicitly retired, and all tagged title
/// effects have completed their fingerprint conversion or are terminal.
pub fn migrate_canvas_2026_10_26(tx: &Transaction<'_>, accounts: &Accounts) -> StoreResult<()> {
    if crate::agent_columns::has_explicit_provider_column(tx)? {
        return Ok(());
    }
    let updates = CANVAS_TABLES
        .into_iter()
        .map(|table| Ok((table, table_updates(tx, table, accounts)?)))
        .collect::<StoreResult<Vec<_>>>()?;
    if updates.iter().all(|(_, rows)| rows.is_empty()) {
        return Ok(());
    }
    if crate::schema::table_exists(tx, "runtime_operation_receipts")? {
        // Catalog-selected title settings are not persisted, so retain only the
        // original provider leaf. The finisher verifies the entire old hash
        // before converting it with its real snapshot and catalog.
        tx.execute(
            "UPDATE runtime_operation_receipts AS effect
             SET result=json_set(result,?1,NULL)
             WHERE signature=?2 AND json_type(result,'$.fingerprint')='text'
               AND EXISTS (SELECT 1 FROM runtime_agents AS agent
                 WHERE agent.id=json_extract(effect.result,'$.agent_id')
                   AND (json_type(agent.record,'$.provider') IS NULL
                        OR json_type(agent.record,'$.provider')='null'))",
            rusqlite::params![
                format!("$.{TITLE_FINGERPRINT_PROVIDER_2026_10_26}"),
                TITLE_EFFECT_SIGNATURE
            ],
        )?;
    }
    let mut preserved = Vec::new();
    for table in super::m2026_10_20_agents::MIGRATION_PRESERVED_TABLES {
        if crate::schema::table_exists(tx, table)? {
            tx.execute_batch(&format!(
                "CREATE TEMP TABLE migration_2026_10_26_{table} AS SELECT * FROM main.{table};"
            ))?;
            preserved.push(table);
        }
    }
    for (table, rows) in updates {
        for (id, raw) in rows {
            tx.execute(
                &format!("UPDATE {table} SET record=?1 WHERE rowid=?2"),
                rusqlite::params![raw, id],
            )?;
        }
    }
    for table in preserved {
        tx.execute_batch(&format!(
            "DELETE FROM main.{table}; INSERT INTO main.{table} SELECT * FROM temp.migration_2026_10_26_{table}; DROP TABLE temp.migration_2026_10_26_{table};"
        ))?;
    }
    Ok(())
}

pub(crate) fn migrate_analytics_2026_10_26(
    tx: &Transaction<'_>,
    accounts: &Accounts,
) -> StoreResult<()> {
    if !analytics_needs_migration(tx)? {
        return Ok(());
    }
    let agents = table_updates(tx, "analytics_agents", accounts)?;
    let usage = legacy_orphan_usage_updates(tx)?;
    for (id, raw) in agents {
        tx.execute(
            "UPDATE analytics_agents SET record=?1 WHERE rowid=?2",
            rusqlite::params![raw, id],
        )?;
    }
    for (id, raw) in usage {
        tx.execute(
            "UPDATE analytics_usage SET record=?1 WHERE rowid=?2",
            rusqlite::params![raw, id],
        )?;
    }
    tx.execute(
        "INSERT INTO analytics_meta(key,value) VALUES(?1,?2)",
        [ANALYTICS_PROVIDER_FORMAT_KEY, PROVIDER_IDENTITY_FORMAT],
    )?;
    Ok(())
}

fn analytics_needs_migration(db: &Connection) -> StoreResult<bool> {
    let format: Option<String> = db
        .query_row(
            "SELECT value FROM analytics_meta WHERE key=?1",
            [ANALYTICS_PROVIDER_FORMAT_KEY],
            |row| row.get(0),
        )
        .optional()?;
    match format.as_deref() {
        None => Ok(true),
        Some(PROVIDER_IDENTITY_FORMAT) => Ok(false),
        Some(_) => Err(invalid("unsupported analytics provider identity format")),
    }
}

#[cfg(test)]
mod tests {
    use crate::{RuntimeStore, StoreError};
    use serde_json::{Value, json};

    fn registry(root: &std::path::Path) {
        std::fs::create_dir_all(root.join("accounts")).expect("accounts directory");
        std::fs::write(root.join("accounts/registry.json"), json!({
            "version":1,"defaultAccountKey":"claude-account","logins":{},
            "accounts":{"claude-account":{"id":"claude-account","provider":"claude","home":"/fixture","label":"Fixture","source":"fixture","status":"ready"}}
        }).to_string()).expect("registry");
    }

    fn legacy_table(tx: &crate::Transaction<'_>) -> Result<(), StoreError> {
        let triggers = tx
            .prepare(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND tbl_name='runtime_agents'",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        tx.execute_batch("ALTER TABLE runtime_agents RENAME TO runtime_agents_current; DROP TABLE runtime_agents_current; CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);")?;
        for sql in triggers {
            tx.execute_batch(&sql)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn migration_2026_10_26_marks_only_bound_legacy_title_effects() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("fresh state");
        store.transaction(|tx| {
            legacy_table(tx)?;
            tx.execute_batch("CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL)")?;
            for (id, provider) in [("absent", None), ("null", Some(Value::Null)),
                ("codex", Some(json!("codex"))), ("claude", Some(json!("claude")))] {
                let mut agent = json!({"id":id,"rootId":id,"epoch":1,"status":"idle"});
                if let Some(provider) = provider { agent["provider"] = provider; }
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params![id,agent.to_string()])?;
            }
            for (id, agent, signature, fingerprint) in [
                ("absent-effect","absent",super::TITLE_EFFECT_SIGNATURE,Some("old-absent")),
                ("null-effect","null",super::TITLE_EFFECT_SIGNATURE,Some("old-null")),
                ("codex-effect","codex",super::TITLE_EFFECT_SIGNATURE,Some("explicit-codex")),
                ("claude-effect","claude",super::TITLE_EFFECT_SIGNATURE,Some("explicit-claude")),
                ("unbound-effect","absent",super::TITLE_EFFECT_SIGNATURE,None),
                ("foreign-effect","absent","another-operation",Some("foreign")),
                ("orphan-effect","missing",super::TITLE_EFFECT_SIGNATURE,Some("orphan")),
            ] {
                let mut effect = json!({"agent_id":agent,"request_id":id,"state":"written","title":"Saved title","epoch":1});
                if let Some(fingerprint) = fingerprint { effect["fingerprint"] = json!(fingerprint); }
                tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
                    rusqlite::params![id,signature,effect.to_string()])?;
            }
            Ok::<_, StoreError>(())
        }).await.expect("historical effects");
        let before = store
            .test_reader()
            .expect("reader")
            .prepare("SELECT id,signature,result FROM runtime_operation_receipts ORDER BY id")
            .expect("receipts")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .expect("receipt rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("receipt values");
        store
            .transaction(|tx| super::migrate_canvas_2026_10_26(tx, &super::Accounts::new()))
            .await
            .expect("dated provider tag conversion");
        let reader = store.test_reader().expect("reader");
        for (id, signature, raw) in before {
            let mut expected: Value = serde_json::from_str(&raw).expect("original effect");
            if matches!(id.as_str(), "absent-effect" | "null-effect") {
                expected[super::TITLE_FINGERPRINT_PROVIDER_2026_10_26] = Value::Null;
            }
            let (actual_signature, actual): (String, String) = reader
                .query_row(
                    "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("migrated effect");
            assert_eq!(actual_signature, signature);
            assert_eq!(
                serde_json::from_str::<Value>(&actual).expect("migrated JSON"),
                expected,
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn migration_2026_10_26_runs_through_store_open_and_reopen() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("fresh state");
        registry(root.path());
        store.transaction(|tx| {
            legacy_table(tx)?;
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('legacy',?1)",
                [json!({"id":"legacy","rootId":"legacy","epoch":7,"status":"idle","accountKey":"claude-account",
                    "accountHistory":[{"accountKey":"claude-account","provider":null,"threadId":"thread-exact"},{"accountKey":"retired"}],
                    "lazyAccountTransfer":{"id":"transfer-exact","sourceStateExact":true,"sourceState":{"model":"m"}},
                    "receipt":{"id":"request-exact","outcome":"unknown"}}).to_string()])?;
            tx.execute("INSERT INTO runtime_account_transfers(id,record) VALUES('transfer-exact',?1)",
                [r#"{"id":"transfer-exact","targetAccountKey":"claude-account","members":{"z":{"provider":null,"settings":{},"providerReceipts":[{"operation":"operation-exact","certainty":"unknown","ticket":{"id":7}}]},"a":{"provider":"claude"}}}"#])?;
            tx.execute("INSERT INTO runtime_analytics_projection(record) VALUES(?1)", [json!({"Analytics":{"observation":{"agent":{"id":"projection-agent","provider":null},"delivery":{"request":"exact"}}}}).to_string()])?;
            Ok::<_,StoreError>(())
        }).await.expect("legacy fixture");
        store.analytics_transaction(|tx,_| {
            tx.execute("DELETE FROM analytics_meta WHERE key=?1",[super::ANALYTICS_PROVIDER_FORMAT_KEY])?;
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('analytic',?1)",
                [json!({"id":"analytic","provider":null,"accountHistory":[{"accountKey":"claude-account"}]}).to_string()])?;
            Ok::<_,StoreError>(())
        }).await.expect("legacy analytics");
        let generation: i64 = store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT value FROM runtime_agent_record_generation WHERE id=1",
                [],
                |row| row.get(0),
            )
            .expect("generation");
        drop(store);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("migrated state");
        let raw: String = store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='legacy'",
                [],
                |row| row.get(0),
            )
            .expect("agent");
        let value: Value = serde_json::from_str(&raw).expect("agent JSON");
        assert_eq!(value["provider"], "codex");
        assert_eq!(value["accountHistory"][0]["provider"], "claude");
        assert_eq!(value["accountHistory"][1]["provider"], "codex");
        assert_eq!(
            value["lazyAccountTransfer"]["sourceState"]["provider"],
            "codex"
        );
        assert_eq!(value["lazyAccountTransfer"]["id"], "transfer-exact");
        let reader = store.test_reader().expect("reader");
        assert_eq!(
            reader
                .query_row(
                    "SELECT value FROM runtime_agent_record_generation WHERE id=1",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .expect("generation"),
            generation
        );
        let transfer: String = reader
            .query_row(
                "SELECT record FROM runtime_account_transfers WHERE id='transfer-exact'",
                [],
                |row| row.get(0),
            )
            .expect("transfer");
        assert!(
            transfer.find("\"z\"").expect("z") < transfer.find("\"a\"").expect("a"),
            "member order preserved"
        );
        let transfer: Value = serde_json::from_str(&transfer).expect("transfer JSON");
        assert_eq!(transfer["targetProvider"], "claude");
        assert_eq!(transfer["members"]["z"]["settings"]["provider"], "codex");
        assert_eq!(
            transfer["members"]["z"]["providerReceipts"],
            json!([{"operation":"operation-exact","certainty":"unknown","ticket":{"id":7}}])
        );
        let projection: String = reader
            .query_row(
                "SELECT record FROM runtime_analytics_projection",
                [],
                |row| row.get(0),
            )
            .expect("projection");
        let projection: Value = serde_json::from_str(&projection).expect("projection JSON");
        assert_eq!(
            projection["Analytics"]["observation"]["agent"]["provider"],
            "codex"
        );
        let analytic: String = store
            .analytics
            .lock()
            .expect("analytics")
            .query_row(
                "SELECT record FROM analytics_agents WHERE id='analytic'",
                [],
                |row| row.get(0),
            )
            .expect("analytics agent");
        let analytic: Value = serde_json::from_str(&analytic).expect("analytics JSON");
        assert_eq!(analytic["provider"], "codex");
        assert_eq!(analytic["accountHistory"][0]["provider"], "claude");
        assert_eq!(
            value["receipt"],
            json!({"id":"request-exact","outcome":"unknown"})
        );
        drop(store);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("repeat open");
        let reopened: String = store
            .test_reader()
            .expect("reader")
            .query_row(
                "SELECT record FROM runtime_agents WHERE id='legacy'",
                [],
                |row| row.get(0),
            )
            .expect("agent");
        assert_eq!(raw, reopened, "repeat migration changes no stored bytes");
    }

    #[tokio::test]
    async fn migration_2026_10_26_rejects_invalid_identity_before_any_record_changes() {
        for provider in [json!("gemini"), json!(1), json!({}), json!(true)] {
            let root = tempfile::tempdir().expect("state directory");
            let store = RuntimeStore::open(Some(root.path()))
                .await
                .expect("fresh state");
            store.transaction(|tx| {
                legacy_table(tx)?;
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES('legacy','{\"id\":\"legacy\"}')",[])?;
                Ok::<_,StoreError>(())
            }).await.expect("legacy root");
            let invalid = json!({"id":"invalid","provider":provider}).to_string();
            let persisted = invalid.clone();
            store
                .analytics_transaction(move |tx, _| {
                    tx.execute(
                        "DELETE FROM analytics_meta WHERE key=?1",
                        [super::ANALYTICS_PROVIDER_FORMAT_KEY],
                    )?;
                    tx.execute(
                        "INSERT INTO analytics_agents(id,record) VALUES('invalid',?1)",
                        [invalid],
                    )?;
                    Ok::<_, StoreError>(())
                })
                .await
                .expect("invalid source");
            drop(store);
            assert!(RuntimeStore::open(Some(root.path())).await.is_err());
            let canvas =
                crate::Connection::open(root.path().join("canvas.sqlite3")).expect("canvas");
            assert_eq!(
                canvas
                    .query_row(
                        "SELECT record FROM runtime_agents WHERE id='legacy'",
                        [],
                        |row| row.get::<_, String>(0)
                    )
                    .expect("unchanged root"),
                "{\"id\":\"legacy\"}"
            );
            let analytics =
                crate::Connection::open(root.path().join("analytics.sqlite3")).expect("analytics");
            assert_eq!(
                analytics
                    .query_row(
                        "SELECT record FROM analytics_agents WHERE id='invalid'",
                        [],
                        |row| row.get::<_, String>(0)
                    )
                    .expect("unchanged invalid"),
                persisted
            );
        }
    }

    #[tokio::test]
    async fn migration_2026_10_26_cutover_never_repairs_new_missing_identity() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("fresh current state");
        store.analytics_transaction(|tx,_| {
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('new-invalid','{\"id\":\"new-invalid\",\"provider\":null}')",[])?;
            Ok::<_,StoreError>(())
        }).await.expect("post-cutover corrupt source");
        assert!(store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('missing','{\"id\":\"missing\"}')",[])?;
            Ok::<_,StoreError>(())
        }).await.is_err(), "explicit provider column rejects new missing identity");
        drop(store);
        let analytics =
            crate::Connection::open(root.path().join("analytics.sqlite3")).expect("analytics");
        let canvas = crate::Connection::open(root.path().join("canvas.sqlite3")).expect("canvas");
        let analytics_before: i64 = analytics
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .expect("revision");
        let canvas_before: i64 = canvas
            .pragma_query_value(None, "data_version", |row| row.get(0))
            .expect("revision");
        let error = RuntimeStore::open(Some(root.path()))
            .await
            .err()
            .expect("current identity refused");
        assert!(matches!(
            error,
            StoreError::Migration {
                id: "2026-10-26-analytics-provider",
                target: studio_runtime_schema::studio_errors::MigrationTarget::Analytics,
                phase: studio_runtime_schema::studio_errors::MigrationPhase::Validate,
                ..
            }
        ));
        let raw: String = analytics
            .query_row(
                "SELECT record FROM analytics_agents WHERE id='new-invalid'",
                [],
                |row| row.get(0),
            )
            .expect("record");
        assert_eq!(raw, "{\"id\":\"new-invalid\",\"provider\":null}");
        assert_eq!(
            analytics
                .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                .expect("revision"),
            analytics_before
        );
        assert_eq!(
            canvas
                .pragma_query_value(None, "data_version", |row| row.get::<_, i64>(0))
                .expect("revision"),
            canvas_before
        );
        analytics.execute("UPDATE analytics_agents SET record='{\"id\":\"new-invalid\",\"provider\":\"codex\"}' WHERE id='new-invalid'",[]).expect("valid fixture identity");
        drop(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("valid current reopen"),
        );
    }

    #[tokio::test]
    async fn migration_2026_10_26_canvas_failure_rolls_back_all_identity_updates() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("fresh state");
        store.transaction(|tx| {
            legacy_table(tx)?;
            tx.execute_batch("INSERT INTO runtime_agents(id,record) VALUES('first','{\"id\":\"first\"}'),('blocked','{\"id\":\"blocked\"}'); CREATE TRIGGER refuse_provider_migration BEFORE UPDATE ON runtime_agents WHEN NEW.id='blocked' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;")?;
            Ok::<_,StoreError>(())
        }).await.expect("failure fixture");
        drop(store);
        assert!(RuntimeStore::open(Some(root.path())).await.is_err());
        let db = crate::Connection::open(root.path().join("canvas.sqlite3")).expect("canvas");
        let missing: i64 = db
            .query_row(
                "SELECT count(*) FROM runtime_agents WHERE json_type(record,'$.provider') IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("unchanged roots");
        assert_eq!(
            missing, 2,
            "the earlier identity update rolled back with the failed update"
        );
    }
}
