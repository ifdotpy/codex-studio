//! Existing budget tables: typed JSON ledger plus uniquely keyed usage rows.
use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{
    AgentId, AgentRecord, EventWriteRepository, JsonRecord, RecordKeyMode, RecordQueryShape,
    StoreError, StoreResult, Table,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    pub id: String,
    pub agent: String,
    pub kind: String,
    pub tokens: u64,
    pub at: f64,
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
        let mut statement=db.prepare("SELECT id FROM runtime_agents WHERE coalesce(nullif(json_extract(record,'$.rootId'),''),id)=?1 ORDER BY id")?;
        let ids = statement
            .query_map([root], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        ids.iter().map(|id| Self::agent(db, id)).collect()
    }
    pub fn usage(db: &Connection, id: &str) -> StoreResult<Option<BudgetUsageRow>> {
        let row=db.query_row("SELECT id,agent,kind,tokens,at FROM runtime_budget_usage WHERE id=?1",[id],|row| Ok(json!({"id":row.get::<_,String>(0)?,"agent":row.get::<_,String>(1)?,"kind":row.get::<_,String>(2)?,"tokens":row.get::<_,i64>(3)?,"at":row.get::<_,f64>(4)?}))).optional()?;
        row.map(|row| {
            validate_usage(&row)?;
            Ok(serde_json::from_value(row)?)
        })
        .transpose()
    }
    pub fn insert_usage(tx: &Transaction<'_>, row: &BudgetUsageRow) -> StoreResult<()> {
        validate_usage(&serde_json::to_value(row)?)?;
        tx.execute(
            "INSERT INTO runtime_budget_usage(id,agent,kind,tokens,at) VALUES (?1,?2,?3,?4,?5)",
            params![
                row.id,
                row.agent,
                row.kind,
                i64::try_from(row.tokens).map_err(|_| StoreError::SchemaMismatch(
                    "invalid runtime_budget_usage/tokens".into()
                ))?,
                row.at
            ],
        )?;
        Ok(())
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
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_entities')",[],|r|r.get(0))?;
        if exists && tx.query_row("SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='agent' AND id=?1 AND deleted=0)",[id],|r|r.get::<_,bool>(0))? {
            EventWriteRepository::set_agent_sync_values(tx,id,fields)?;
        }
        Ok(())
    }
    pub fn events(db: &Connection, ids: &[String]) -> StoreResult<Vec<Value>> {
        let mut rows = Vec::new();
        let mut query=db.prepare("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events WHERE id=?1")?;
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
        tx.execute("UPDATE runtime_events SET status=?1,error=CASE WHEN ?2 THEN NULL ELSE error END WHERE id=?3",params![status,clear_error,id])?;
        Ok(())
    }
    pub fn retire_action(tx: &Transaction<'_>, request: &str, attempt: &Value) -> StoreResult<()> {
        tx.execute("UPDATE runtime_native_action_receipts SET outcome=?1 WHERE id=?2 AND json_extract(receipt,'$.attemptId')=?3 AND json_extract(outcome,'$.status')='pending'",
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
    Ok(())
}
const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_budget",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_budget_usage",
        columns: &["id", "agent", "kind", "tokens", "at"],
    },
];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[ConditionalTableRequirement {
        table_exists: "runtime_budget",
        tables: TABLES,
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
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
    if !db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_budget_usage')",[],|r|r.get::<_,bool>(0))? {return Ok(report);}
    let mut statement = db.prepare("SELECT id,agent,kind,tokens,at FROM runtime_budget_usage")?;
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
        if let Some(problem) =
            studio_runtime_schema::first_error("PersistedBudgetUsage", &Value::Object(value))
                .map_err(StoreError::SchemaMismatch)?
        {
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
