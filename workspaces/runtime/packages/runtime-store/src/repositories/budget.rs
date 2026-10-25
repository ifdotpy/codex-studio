//! Existing budget tables: typed JSON ledger plus uniquely keyed usage rows.
use crate::CachedSql;
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{
    AgentId, AgentRecord, EventWriteRepository, JsonRecord, RecordKeyMode, RecordQueryShape,
    StoreError, StoreResult, Table,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::usage::UsageTokens;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct BudgetRecord(pub Value);
impl JsonRecord for BudgetRecord {
    type Id = AgentId;
    const TABLE: &'static str = "runtime_budget";
    const SCHEMA: &'static str = "PersistedBudgetState";
    const KEY_MODE: RecordKeyMode = RecordKeyMode::External;
    const QUERY_SHAPE: RecordQueryShape = RecordQueryShape::None;
    fn record_key(&self) -> Option<&Self::Id> {
        None
    }
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct BudgetUsageRow {
    /// Typed evidence and its original accounting placement; absent before ledger v5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<BudgetUsageDetails>,
    pub id: String,
    pub agent: String,
    pub kind: String,
    pub tokens: u64,
    pub at: f64,
    /// Immutable response categories in tokens; absent on pre-contract rows and notices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breakdown: Option<UsageTokens>,
}

/// Budget-specific placement of a shared persisted typed usage receipt.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetUsageDetails {
    pub ledger: studio_runtime_schema::usage::ledger::UsageLedgerRow,
    /// Whether the response belongs to the pre-budget baseline.
    pub before_cutoff: bool,
}

pub struct BudgetRepository;
impl BudgetRepository {
    pub fn agent(db: &Connection, id: &str) -> StoreResult<Value> {
        Table::<AgentRecord>::new(db)
            .get(&id.into())?
            .map(|r| r.value())
            .ok_or_else(|| StoreError::SchemaMismatch("budget agent is missing".into()))
    }
    pub fn team(db: &Connection, root: &str) -> StoreResult<Vec<Value>> {
        let mut statement=db.prepare_cached("SELECT id FROM runtime_agents WHERE root_id=?1 OR (COALESCE(root_id,'')='' AND id=?1) ORDER BY id")?;
        let ids = statement
            .query_map([root], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter().map(|id| Self::agent(db, id)).collect()
    }
    pub fn usage(db: &Connection, id: &str) -> StoreResult<Option<BudgetUsageRow>> {
        let row=db.query_row_cached("SELECT id,agent,kind,tokens,at,breakdown,details FROM runtime_budget_usage WHERE id=?1",[id],|row| Ok((json!({"id":row.get::<_,String>(0)?,"agent":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"tokens":row.get::<_,i64>(3)?,"at":row.get::<_,f64>(4)?}),row.get::<_,Option<String>>(5)?,row.get::<_,Option<String>>(6)?))).optional()?;
        row.map(|(mut row, raw, details)| {
            let breakdown = raw
                .map(|raw| serde_json::from_str::<UsageTokens>(&raw))
                .transpose()?;
            row["breakdown"] = serde_json::to_value(breakdown)?;
            row["details"] = details
                .map(|raw| serde_json::from_str::<Value>(&raw))
                .transpose()?
                .unwrap_or(Value::Null);
            validate_usage(&row)?;
            Ok(serde_json::from_value(row)?)
        })
        .transpose()
    }
    pub fn insert_usage(tx: &Transaction<'_>, row: &BudgetUsageRow) -> StoreResult<()> {
        validate_usage(&serde_json::to_value(row)?)?;
        tx.execute_cached(
            "INSERT INTO runtime_budget_usage(id,agent,kind,tokens,at,breakdown,details) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO UPDATE SET kind=excluded.kind,tokens=excluded.tokens,at=excluded.at,breakdown=excluded.breakdown,details=excluded.details",
            params![
                row.id,
                row.agent,
                row.kind,
                i64::try_from(row.tokens).map_err(|_| StoreError::SchemaMismatch(
                    "invalid runtime_budget_usage/tokens".into()
                ))?,
                row.at,
                row.breakdown.as_ref().map(serde_json::to_string).transpose()?,
                row.details.as_ref().map(serde_json::to_string).transpose()?
            ],
        )?;
        Ok(())
    }
    /// Lifetime tokens in the agent's recorded responses, including zero before any response.
    pub fn response_tokens(db: &Connection, agent: &str) -> StoreResult<u64> {
        let (tokens, invalid): (i64, i64) = db.query_row_cached(
            "SELECT COALESCE(SUM(tokens),0), COUNT(CASE WHEN typeof(tokens)!='integer' OR tokens<0 THEN 1 END) FROM runtime_budget_usage WHERE agent=?1 AND kind='response'",
            [agent],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if invalid != 0 {
            return Err(StoreError::SchemaMismatch(
                "invalid runtime_budget_usage/tokens".into(),
            ));
        }
        u64::try_from(tokens)
            .map_err(|_| StoreError::SchemaMismatch("invalid runtime_budget_usage/tokens".into()))
    }
    pub fn save_agent(tx: &Transaction<'_>, agent: &Value) -> StoreResult<()> {
        let record = AgentRecord::from_value(agent.clone())?;
        Table::<AgentRecord>::new(tx).put(&record)
    }
    pub fn patch_budget(tx: &Transaction<'_>, id: &str, patch: &Value) -> StoreResult<()> {
        let mut agent = Self::agent(tx, id)?;
        for name in ["tokensUsed", "tokenUsageAccounting"] {
            agent[name] = patch[name].clone();
        }
        Self::save_agent(tx, &agent)?;
        Self::sync_fields(
            tx,
            id,
            &[
                ("tokensUsed", patch["tokensUsed"].clone()),
                (
                    "tokenUsageAccounting",
                    patch["tokenUsageAccounting"].clone(),
                ),
            ],
        )
    }
    // Python does not create a renderer entity during historical capture.
    pub fn sync_fields(
        tx: &Transaction<'_>,
        id: &str,
        fields: &[(&str, Value)],
    ) -> StoreResult<()> {
        let exists:bool=tx.query_row_cached("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_entities')",[],|r|r.get(0))?;
        if exists && tx.query_row_cached("SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='agent' AND id=?1 AND deleted=0)",[id],|r|r.get::<_,bool>(0))? {
            EventWriteRepository::set_agent_sync_values(tx,id,fields)?;
        }
        Ok(())
    }
    pub fn events(db: &Connection, ids: &[String]) -> StoreResult<Vec<Value>> {
        let mut rows = Vec::new();
        let mut query=db.prepare_cached("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events WHERE id=?1")?;
        for id in ids {
            if let Some(row)=query.query_row([id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"text":r.get::<_,String>(3)?,"status":r.get::<_,String>(4)?,"created":r.get::<_,f64>(5)?,"epoch":r.get::<_,i64>(6)?,"turn_id":r.get::<_,Option<String>>(7)?,"error":r.get::<_,Option<String>>(8)?}))).optional()? {rows.push(row);}
        }
        Ok(rows)
    }
    pub fn set_event(
        tx: &Transaction<'_>,
        id: &str,
        status: &str,
        clear_error: bool,
    ) -> StoreResult<()> {
        tx.execute_cached("UPDATE runtime_events SET status=?1,error=CASE WHEN ?2 THEN NULL ELSE error END WHERE id=?3",params![status,clear_error,id])?;
        Ok(())
    }
    pub fn retire_action(tx: &Transaction<'_>, request: &str, attempt: &Value) -> StoreResult<()> {
        tx.execute_cached("UPDATE runtime_native_action_receipts SET outcome=?1 WHERE id=?2 AND json_extract(receipt,'$.attemptId')=?3 AND json_extract(outcome,'$.status')='pending'",
            params![json!({"status":"failed","notSubmitted":true,"error":"The budget wait belongs to an earlier agent state"}).to_string(),request,attempt.as_str()])?;
        Ok(())
    }
}
fn validate_usage(value: &Value) -> StoreResult<()> {
    if let Some((pointer, _)) = studio_runtime_schema::first_error("PersistedBudgetUsage", value)
        .map_err(StoreError::SchemaMismatch)?
    {
        return Err(StoreError::SchemaMismatch(format!(
            "invalid runtime_budget_usage{pointer}"
        )));
    }
    let row = serde_json::from_value::<BudgetUsageRow>(value.clone()).map_err(|error| {
        StoreError::SchemaMismatch(format!("invalid runtime_budget_usage/breakdown: {error}"))
    })?;
    if let Some(details) = &row.details {
        use studio_runtime_schema::usage::{UsageObservation, ledger::UsageIdentity};
        let ledger = &details.ledger;
        let thread = match &ledger.usage_identity {
            UsageIdentity::Cumulative { thread_id, .. } => thread_id.as_deref(),
            _ => None,
        };
        let (kind, total) = match &ledger.studio_usage {
            UsageObservation::Response { tokens, .. } => ("response", tokens.total()),
            UsageObservation::Gap { .. } => ("notice", Some(0)),
            UsageObservation::Cumulative { total, last, .. } => {
                ("notice", last.as_ref().unwrap_or(total).total())
            }
        };
        if ledger.receipt_key.as_deref() != Some(row.id.as_str())
            || ledger.usage_identity != UsageIdentity::new(&row.agent, thread, &ledger.studio_usage)
            || row.kind != kind
            || total != Some(row.tokens)
        {
            return Err(StoreError::SchemaMismatch(
                "invalid runtime_budget_usage/details identity or tokens".into(),
            ));
        }
        if let UsageObservation::Response { tokens, .. } = &ledger.studio_usage
            && row
                .breakdown
                .as_ref()
                .is_some_and(|breakdown| breakdown != tokens)
        {
            return Err(StoreError::SchemaMismatch(
                "invalid runtime_budget_usage/details breakdown".into(),
            ));
        }
    }
    Ok(())
}
const TABLES: &[TableRequirement] = &[TableRequirement {
    name: "runtime_budget",
    columns: &["id", "record"],
}];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_budget",
        tables: TABLES,
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TableRequirement {
        name: "runtime_budget_usage",
        columns: &[
            "id",
            "agent",
            "kind",
            "tokens",
            "at",
            "breakdown",
            "details",
        ],
    }],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};
inventory::submit! {crate::repositories::SchemaRegistration{repository:"budget",requirements:&REQUIREMENTS,}}

/// The usage ledger is a scalar table, so it cannot use the id/record checker.
pub fn check_usage_readonly(db: &Connection) -> StoreResult<crate::StateCheckTable> {
    let mut report = crate::StateCheckTable {
        table: "runtime_budget_usage",
        row_count: 0,
        rejected_count: 0,
        failures: vec![],
    };
    if !db.query_row_cached("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_budget_usage')",[],|r|r.get::<_,bool>(0))? {return Ok(report);}
    let has_breakdown: bool = db.query_row_cached(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_budget_usage') WHERE name='breakdown')",
        [], |row| row.get(0),
    )?;
    let has_details:bool=db.query_row_cached("SELECT EXISTS(SELECT 1 FROM pragma_table_info('runtime_budget_usage') WHERE name='details')",[],|row|row.get(0))?;
    let sql = format!(
        "SELECT id,agent,kind,tokens,at,{},{} FROM runtime_budget_usage",
        if has_breakdown { "breakdown" } else { "NULL" },
        if has_details { "details" } else { "NULL" }
    );
    let mut statement = db.prepare_cached(&sql)?;
    let mut rows = statement.query([])?;
    let mut failures = std::collections::BTreeMap::new();
    while let Some(row) = rows.next()? {
        report.row_count += 1;
        let mut value = serde_json::Map::new();
        for (index, name) in ["id", "agent", "kind", "tokens", "at"]
            .into_iter()
            .enumerate()
        {
            let field = match row.get_ref(index)? {
                rusqlite::types::ValueRef::Integer(v) => json!(v),
                rusqlite::types::ValueRef::Real(v) => json!(v),
                rusqlite::types::ValueRef::Text(v) => std::str::from_utf8(v)
                    .map(|v| json!(v))
                    .unwrap_or(Value::Null),
                _ => Value::Null,
            };
            value.insert(name.into(), field);
        }
        let breakdown = match row.get_ref(5)? {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Text(bytes) => serde_json::from_slice::<UsageTokens>(bytes)
                .ok()
                .and_then(|tokens| serde_json::to_value(tokens).ok())
                .unwrap_or_else(|| json!("invalid usage breakdown")),
            _ => json!("invalid usage breakdown"),
        };
        value.insert("breakdown".into(), breakdown);
        let details = match row.get_ref(6)? {
            rusqlite::types::ValueRef::Null => Value::Null,
            rusqlite::types::ValueRef::Text(raw) => serde_json::from_slice::<Value>(raw)
                .unwrap_or_else(|_| json!("invalid usage details")),
            _ => json!("invalid usage details"),
        };
        value.insert("details".into(), details);
        let value = Value::Object(value);
        let problem = studio_runtime_schema::first_error("PersistedBudgetUsage", &value)
            .map_err(StoreError::SchemaMismatch)?
            .or_else(|| {
                serde_json::from_value::<BudgetUsageRow>(value.clone())
                    .err()
                    .map(|_| ("/breakdown".into(), "usageTokens".into()))
            })
            .or_else(|| {
                validate_usage(&value)
                    .err()
                    .map(|_| ("/details".into(), "usageLedger".into()))
            });
        if let Some(problem) = problem {
            report.rejected_count += 1;
            *failures.entry(problem).or_insert(0) += 1;
        }
    }
    report.failures = failures
        .into_iter()
        .map(|((pointer, keyword), count)| crate::StateCheckFailure {
            pointer,
            keyword,
            count,
        })
        .collect();
    Ok(report)
}
