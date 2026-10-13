//! Terminal transcript effects share the lifecycle transaction and its replay receipt.
use crate::notification_dispatch::notices::error_message as message;
use serde_json::{Value, json};
use studio_runtime_store::{AgentRecord, NotificationDispatchRepository, StoreResult, Transaction};
pub(crate) fn terminal(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    turn: &str,
    status: &str,
    error: Option<&Value>,
    at: f64,
) -> StoreResult<()> {
    if status == "failed" {
        let error = error
            .cloned()
            .unwrap_or_else(|| json!({"message":"Codex ended this turn with an error."}));
        let text = message(&error);
        let id = format!("{}:native-notice:error:{turn}", agent.id.0);
        let record = json!({"id":id,"role":"system","title":"Codex","text":text.chars().take(20_000).collect::<String>(),"truncated":text.chars().count()>20_000,"at":at,"nativeNotice":"error","turnId":turn,"threadId":agent.extra.get("threadId"),"nativeError":error,"cyberAccessProgram":agent.extra.get("cyberAccessProgram")});
        NotificationDispatchRepository::put_transcript_item(
            tx,
            &id,
            &agent.id.0,
            &record,
            at,
            &text,
            true,
        )?;
    }
    let attempt = agent.extra.get("startAttempt").unwrap_or(&Value::Null);
    let since = if attempt["turnId"] == turn {
        attempt["created"]
            .as_f64()
            .filter(|v| *v != 0.0)
            .map_or(0.0, |v| v - 60.0)
    } else {
        0.0
    };
    tx.execute("UPDATE runtime_items SET record=json_set(record,'$.turnStatus',?1) WHERE agent=?2 AND json_extract(record,'$.turnId')=?3 AND created>=?4",rusqlite::params![status,agent.id.0,turn,since])?;
    Ok(())
}
