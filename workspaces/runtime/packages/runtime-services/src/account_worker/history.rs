//! Terminal transcript effects share the lifecycle transaction and its replay receipt.
use crate::notification_dispatch::notices::{error_message as message, provider_label};
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
        let label = provider_label(&agent.value());
        let error = error.cloned().unwrap_or_else(
            || json!({"message": format!("{label} ended this turn with an error.")}),
        );
        let text = message(&error, label);
        let id = format!("{}:native-notice:error:{turn}", agent.id.0);
        let record = json!({"id":id,"role":"system","title":label,"text":text.chars().take(20_000).collect::<String>(),"truncated":text.chars().count()>20_000,"at":at,"nativeNotice":"error","turnId":turn,"threadId":agent.extra.get("threadId"),"nativeError":error,"cyberAccessProgram":agent.extra.get("cyberAccessProgram")});
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
    // The ended turn, and an earlier turn that ended without its report, can
    // no longer report the end of their items (Python `turn/completed`).
    crate::task_settlement::settle_agent_tasks(tx, &agent.id.0, at)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_runtime_store::RuntimeStore;

    #[tokio::test]
    async fn provider_identity_failed_turn_notice_keeps_unknown_identity_generic() {
        let root = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        for (index, provider) in [
            None,
            Some(Value::Null),
            Some(json!("future")),
            Some(json!(7)),
            Some(json!("codex")),
            Some(json!("claude")),
        ]
        .into_iter()
        .enumerate()
        {
            let mut agent =
                AgentRecord::from_value(json!({"id":"a","rootId":"a","epoch":0,"status":"failed"}))
                    .expect("agent");
            if let Some(provider) = provider {
                agent.extra.insert("provider".into(), provider);
            }
            let expected = match agent.extra.get("provider").and_then(Value::as_str) {
                Some("codex") => "Codex",
                Some("claude") => "Claude",
                _ => "Provider",
            };
            store
                .transaction(move |tx| {
                    terminal(tx, &agent, &format!("turn-{index}"), "failed", None, 1.0)
                })
                .await
                .expect("terminal notice");
            let raw: String = store
                .test_reader()
                .expect("reader")
                .query_row(
                    "SELECT record FROM runtime_items WHERE id=?1",
                    [format!("a:native-notice:error:turn-{index}")],
                    |row| row.get(0),
                )
                .expect("notice");
            let notice: Value = serde_json::from_str(&raw).expect("notice JSON");
            assert_eq!(notice["title"], expected);
            assert_eq!(
                notice["text"],
                format!("{expected} ended this turn with an error.")
            );
        }
    }
}
