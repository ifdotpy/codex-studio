//! Idempotent post-login repair; historical input and transcript rows stay intact.
use super::{LoginError, ports::LoginChats};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{AgentRecord, RuntimeStore, SchedulerRepository, Table};

pub struct StoredChats(pub Arc<RuntimeStore>);
impl LoginChats for StoredChats {
    fn refresh<'a>(&'a self, account: &'a str) -> super::ports::FutureResult<'a, ()> {
        Box::pin(async move {
            let account = account.to_owned();
            self.0
                .transaction(move |tx| {
                    for record in Table::<AgentRecord>::new(tx).list()? {
                        let agent = record.value();
                        if agent.get("accountKey").and_then(Value::as_str) == Some(account.as_str())
                            && agent.get("provider") == Some(&json!("claude"))
                            && let Some(error) = agent
                                .get("error")
                                .filter(|error| authentication_failure(error))
                        {
                            SchedulerRepository::repair_authenticated_failure(
                                tx,
                                &record.id.0,
                                record.epoch,
                                &account,
                                error,
                            )?;
                        }
                    }
                    Ok(())
                })
                .await
                .map_err(|_| LoginError::Persistence)
        })
    }
}
fn authentication_failure(error: &Value) -> bool {
    let text = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("");
    static ERROR: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    ERROR.get_or_init(||regex::Regex::new(r"(?i)oauth session expired|failed to authenticate|profile.s account changed|sign in with claude auth login")).as_ref().is_ok_and(|r|r.is_match(text))
}
