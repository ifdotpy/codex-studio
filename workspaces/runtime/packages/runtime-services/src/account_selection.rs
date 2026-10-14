//! Empty-chat account selection with injected registry facts; no credential access.
use crate::account_transfer::{TransferError, read_record, remove};
use serde_json::{Value, json};
use studio_runtime_store::{AccountTransferRepository, AgentRepository, RuntimeStore, StoreError};

pub struct SelectionSnapshot {
    agent: Value,
}
impl SelectionSnapshot {
    pub fn read(raw: &str) -> Result<Self, TransferError> {
        let agent = read_record(raw, "PersistedAgentRecord", "account selection agent")?;
        Ok(Self { agent })
    }
    /// The adapter supplies the directory and empty-chat check from its transaction.
    pub fn decide(
        &self,
        target: &str,
        account: Option<&Value>,
        directory: &str,
        empty: bool,
    ) -> Result<Value, TransferError> {
        let mut agent = self.agent.clone();
        let truthy = crate::event_write::truthy;
        if agent
            .get("excludedAccountKeys")
            .and_then(Value::as_array)
            .is_some_and(|keys| {
                keys.iter().all(Value::is_string) && keys.iter().any(|key| key == target)
            })
        {
            return Err(TransferError::Refused(
                "This account is disabled for this chat",
            ));
        }
        let source = agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_owned();
        if source == target && agent["cwd"] == directory {
            return Ok(agent);
        }
        let account = account.ok_or(TransferError::Refused("Unknown account"))?;
        if truthy(account.get("deleted")) {
            return Err(TransferError::Refused(
                "This account was deleted. Select another account",
            ));
        }
        if truthy(account.get("disconnected")) {
            return Err(TransferError::Refused(
                "Reconnect this account before selecting it",
            ));
        }
        if !empty || truthy(agent.get("inFlight")) {
            return Err(TransferError::Refused(
                "The account is fixed after the first message. Create a new chat",
            ));
        }
        if agent["cwd"] != directory {
            let revision = agent
                .get("projectFolderRevision")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(TransferError::Refused("Invalid project revision"))?;
            remove(&mut agent, &["projectFolder"])?;
            agent["projectFolderRevision"] = json!(revision);
        }
        let provider = account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("codex");
        if !matches!(provider, "codex" | "claude") {
            return Err(TransferError::Refused("Unknown account provider"));
        }
        if agent
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("codex")
            != provider
        {
            agent["provider"] = json!(provider);
            agent["model"] = json!(if provider == "claude" {
                "default"
            } else {
                "gpt-6-astra"
            });
            agent["effort"] = json!("medium");
            agent["nativeEffort"] = json!("medium");
            agent["fastMode"] = json!(false);
            agent["workerDefaults"] = json!({"model":"gpt-6-luna","effort":"high","fastMode":false,"daybreakEnabled":false,"delegationDepth":1});
            remove(
                &mut agent,
                &["pendingSettings", "pendingSettingsAccountKey"],
            )?;
        }
        if source != target {
            // A continuation cancellation needs the usage-resume service's atomic write.
            if matches!(
                agent.pointer("/usageResume/status").and_then(Value::as_str),
                Some("scheduled" | "started")
            ) {
                return Err(TransferError::Requires(
                    "account-transfer/usage-resume-cancellation",
                ));
            }
            if agent.get("executionSettingsAccountKey").is_none() {
                agent["executionSettingsAccountKey"] = json!(source);
            }
            agent["daybreakEnabled"] = json!(false);
            agent["cyberAccessProgram"] = json!("standard");
            remove(
                &mut agent,
                &["pendingSettings", "pendingSettingsAccountKey"],
            )?;
            if truthy(agent.get("workerDefaults")) {
                agent["workerDefaults"]["daybreakEnabled"] = json!(false);
                agent["workerDefaults"]["cyberAccessProgram"] = json!("standard");
            }
        }
        agent["accountKey"] = json!(target);
        agent["cwd"] = json!(directory);
        Self::read(&agent.to_string())?;
        Ok(agent)
    }
}
pub fn select(
    store: &RuntimeStore,
    accounts: &dyn Fn() -> Result<crate::account_policy::AccountsSnapshot, TransferError>,
    id: &str,
    target: &str,
    cwd: Option<&str>,
) -> Result<Value, TransferError> {
    if cwd.is_some_and(|value| crate::event_write::python_strip(value).is_empty()) {
        return Err(TransferError::Refused(
            "Select an existing project directory",
        ));
    }
    let directory = cwd
        .map(crate::agents_create::project_directory)
        .transpose()
        .map_err(|_| TransferError::Refused("Select an existing project directory"))?;
    let checked_directory = match &directory {
        Some(directory) => directory.clone(),
        None => {
            let raw = AccountTransferRepository::agent(&store.read()?, id)?
                .ok_or(TransferError::Refused("Unknown managed agent"))?;
            SelectionSnapshot::read(&raw)?.agent["cwd"]
                .as_str()
                .ok_or(TransferError::Refused(
                    "Select an existing project directory",
                ))?
                .to_owned()
        }
    };
    // Filesystem work precedes the writer lock; the transaction rechecks that it
    // still acts on this exact directory before using the result.
    let directory_exists = std::path::Path::new(&checked_directory).is_dir();
    store.transaction_with_error(|tx| {
        // This port reads the in-memory registry only; no auth refresh or provider I/O.
        let accounts = accounts()?;
        let account = accounts
            .accounts
            .iter()
            .chain(&accounts.archived_accounts)
            .find(|row| row["id"] == target);
        let raw = AccountTransferRepository::agent(tx, id)?
            .ok_or(TransferError::Refused("Unknown managed agent"))?;
        let snapshot = SelectionSnapshot::read(&raw)?;
        let directory = directory
            .as_deref()
            .or_else(|| snapshot.agent["cwd"].as_str())
            .ok_or(TransferError::Refused(
                "Select an existing project directory",
            ))?;
        let context = crate::sync_entity_context::agent_view_context(tx, &snapshot.agent, None)?;
        let value = snapshot.decide(target, account, directory, context.empty_lead)?;
        if value != snapshot.agent {
            if directory != checked_directory {
                return Err(TransferError::Refused(
                    "Project directory changed during account selection",
                ));
            }
            if !directory_exists {
                return Err(TransferError::Refused(
                    "Select an existing project directory",
                ));
            }
            crate::agents_create::ensure_project(tx, &value)
                .map_err(|_| TransferError::Refused("Cannot register the selected project"))?;
            AgentRepository::put(
                tx,
                &serde_json::from_value(value.clone()).map_err(StoreError::from)?,
            )?;
            return Ok(crate::agents_create::publish_agent(tx, &value)?);
        }
        let mut view = value;
        crate::agents_create::mode_fields(&mut view);
        crate::sync_views::agent_entity_view(&view, &context)
            .ok_or(TransferError::Refused("Invalid selection agent"))
    })
}
