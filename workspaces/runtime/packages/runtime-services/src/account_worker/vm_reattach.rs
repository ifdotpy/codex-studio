//! Restore only proved existing VM readers. Launch and input recovery stay separate.
use super::*;
use rusqlite::OptionalExtension;
use studio_runtime_process::ProviderLaunchSpec;

/// Maximum actor IDs loaded per startup read; unit: active VM actors.
pub(super) const STARTUP_PAGE_ACTORS: i64 = 64;
/// Startup reads use AgentIndexedReads' account/live-row index and rowid pages.
pub(super) const STARTUP_CANDIDATES: &str = "SELECT rowid,id FROM runtime_agents \
    WHERE account_key=?1 AND deleted_at IS NULL AND rowid>?2 \
    AND execution_mode='vm' AND in_flight=1 ORDER BY rowid LIMIT ?3";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    request: preparation::VmProviderLaunchRequest,
    attempt: String,
    thread: String,
    turn: String,
}

/// A current row alone cannot authorize an old process after a same-epoch move.
/// The immutable claim and the submitted attempt must bind the same identity.
fn binding(
    connection: &rusqlite::Connection,
    actor: &str,
    account: &str,
    generation: u64,
) -> Result<Option<Binding>, WorkerError> {
    let Some(record) = AgentRepository::get(connection, &AgentId::from(actor))? else {
        return Ok(None);
    };
    let fields = &record.extra;
    let Some(provider) = fields
        .get("provider")
        .and_then(Value::as_str)
        .filter(|provider| matches!(*provider, "codex" | "claude"))
    else {
        return Ok(None);
    };
    if !crate::linux_vm_provider::vm_actor_matches(
        connection,
        actor,
        record.epoch,
        account,
        Some(provider),
    )? || fields.get("inFlight") != Some(&Value::Bool(true))
        || [
            "executionMove",
            "lazyAccountTransfer",
            "accountTransferId",
            "workspaceOperation",
        ]
        .iter()
        .any(|key| fields.get(*key).is_some_and(|value| !value.is_null()))
        || ["remoteWorker", "remoteAnchor"]
            .iter()
            .any(|key| fields.get(*key) == Some(&Value::Bool(true)))
        || fields
            .get("executionSettingsAccountKey")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(account))
    {
        return Ok(None);
    }
    let Some(attempt) = fields.get("startAttempt").filter(|value| value.is_object()) else {
        return Ok(None);
    };
    let text = |value: &Value| {
        value
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    let (Some(id), Some(thread), Some(turn), Some(cwd)) = (
        text(&attempt["id"]),
        text(&attempt["threadId"]),
        text(&attempt["turnId"]),
        fields.get("cwd").and_then(text),
    ) else {
        return Ok(None);
    };
    if attempt["epoch"].as_i64() != Some(record.epoch)
        || attempt["accountKey"].as_str() != Some(account)
        || attempt["submitted"] != true
        || fields.get("threadId").and_then(Value::as_str) != Some(thread.as_str())
        || fields.get("turnId").and_then(Value::as_str) != Some(turn.as_str())
        || fields
            .get("nativeResponseTurn")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(turn.as_str()))
        || !std::path::Path::new(&cwd).is_absolute()
    {
        return Ok(None);
    }
    let home = fields
        .get("layrHome")
        .and_then(Value::as_str)
        .unwrap_or(crate::linux_vm_provider::DEFAULT_GUEST_HOME);
    if !std::path::Path::new(home).is_absolute() {
        return Ok(None);
    }
    // Read only the original authority fields, never the saved claim's prompts.
    let proved = connection
        .query_row(
            "SELECT json_extract(record,'$.submitted')=1 \
         AND json_extract(record,'$.epoch')=?2 AND json_extract(record,'$.accountKey')=?3 \
         AND json_extract(record,'$.threadId')=?4 AND json_extract(record,'$.turnId')=?5 \
         AND json_extract(record,'$.claimResult.agent.id')=?6 \
         AND json_extract(record,'$.claimResult.agent.epoch')=?2 \
         AND json_extract(record,'$.claimResult.agent.accountKey')=?3 \
         AND json_extract(record,'$.claimResult.agent.provider')=?7 \
         AND json_extract(record,'$.claimResult.agent.executionMode')='vm' \
         AND json_extract(record,'$.claimResult.agent.cwd')=?8 \
         AND COALESCE(json_extract(record,'$.claimResult.agent.layrHome'),?9)=?10 \
         FROM runtime_execution_attempts WHERE id=?1",
            rusqlite::params![
                id,
                record.epoch,
                account,
                thread,
                turn,
                actor,
                provider,
                cwd,
                crate::linux_vm_provider::DEFAULT_GUEST_HOME,
                home
            ],
            |row| row.get::<_, Option<bool>>(0),
        )
        .optional()?
        .flatten()
        .unwrap_or(false);
    if !proved {
        return Ok(None);
    }
    Ok(Some(Binding {
        request: preparation::VmProviderLaunchRequest {
            agent_id: actor.into(),
            epoch: record.epoch,
            generation,
            account_key: account.into(),
            provider: provider.into(),
            cwd,
            guest_home: home.into(),
        },
        attempt: id,
        thread,
        turn,
    }))
}

impl Worker {
    pub(super) async fn restore_existing_vm_sessions(&mut self) -> Result<(), WorkerError> {
        if self.resources.guest_rpc.is_none() {
            return Ok(());
        }
        let mut after = 0i64;
        loop {
            let account = self.account.clone();
            let page = self
                .db("vm_reattach_candidates", move |store| async move {
                    store
                        .read_async("runtime-services/vm_reattach", move |connection| {
                            let mut statement = connection.prepare(STARTUP_CANDIDATES)?;
                            let rows = statement.query_map(
                                rusqlite::params![account, after, STARTUP_PAGE_ACTORS],
                                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                            )?;
                            rows.collect::<Result<Vec<_>, _>>()
                                .map_err(WorkerError::from)
                        })
                        .await
                })
                .await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            after = *last;
            for (_, actor) in page {
                if !self.restore_existing_vm_actor(&actor).await {
                    eprintln!(
                        "VM reader held for actor {actor}: existing process authority is unproved"
                    );
                }
            }
        }
        Ok(())
    }

    /// Failure holds this actor only. Never fall back to a launch or input replay.
    pub(in crate::account_worker) async fn restore_existing_vm_actor(
        &mut self,
        actor: &str,
    ) -> bool {
        if self.vm_sessions.contains_key(actor) {
            return true;
        }
        if !self.resolving_vm_actors.insert(actor.to_owned()) {
            return false;
        }
        let result = self.open_existing_vm_actor(actor).await;
        self.resolving_vm_actors.remove(actor);
        result.unwrap_or(false)
    }

    async fn vm_reattach_binding(&mut self, actor: &str) -> Result<Option<Binding>, WorkerError> {
        let actor = actor.to_owned();
        let account = self.account.clone();
        let generation = self.generation.0;
        self.db("vm_reattach_authorize", move |store| async move {
            store
                .read_async("runtime-services/vm_reattach", move |connection| {
                    binding(connection, &actor, &account, generation)
                })
                .await
        })
        .await
    }

    async fn open_existing_vm_actor(&mut self, actor: &str) -> Result<bool, WorkerError> {
        let Some(guest) = self.resources.guest_rpc.clone() else {
            return Ok(false);
        };
        let Some(proof) = self.vm_reattach_binding(actor).await? else {
            return Ok(false);
        };
        let request = proof.request.clone();
        let store = self.bridge.store.clone();
        let identity = crate::linux_vm_provider::GuestProviderIdentity {
            agent_id: request.agent_id.clone(),
            epoch: request.epoch,
            generation: request.generation,
            account_key: request.account_key.clone(),
            provider: request.provider.clone(),
        };
        let factory = self
            .drive(async move {
                crate::linux_vm_provider::GuestProviderTaskFactory::existing(store, guest, identity)
                    .await
                    .map_err(|_| {
                        WorkerError::PreparationRefused("vm-agent/existing-process-unproved".into())
                    })
            })
            .await?;
        if self.closing || self.vm_reattach_binding(actor).await?.as_ref() != Some(&proof) {
            return Ok(false);
        }
        let generation = self.generation;
        let session = SessionHandle::open(
            Arc::new(factory),
            ProviderHandle::for_account(actor),
            OpenOptions {
                clock: studio_runtime_process::provider_seam::SessionClock::default(),
                launch: ProviderLaunchSpec::new(
                    Vec::new(),
                    BTreeMap::new(),
                    request.cwd.clone().into(),
                ),
                expected_generation: Some(generation),
            },
            SessionTaskConfig {
                initialize_params: studio_runtime_providers::initialize_params(
                    studio_runtime_providers::ClientName::AccountSession,
                ),
                generation,
                wire: request
                    .provider
                    .parse::<studio_runtime_providers::ProviderKind>()
                    .map_err(|_| {
                        WorkerError::PreparationRefused("vm-agent/provider-unproved".into())
                    })?
                    .wire(),
                timeout_ms: self.timeout_ms,
                initialize_operation: OperationId(format!(
                    "vm:{actor}:initialize:{}:{}",
                    request.epoch, generation.0
                )),
                request_id_prefix: None,
                // A reader reattach never restores a native write into the send lane.
                recovered: Vec::new(),
            },
        )
        .await?;
        self.install_vm_session(request, session, None, true)?;
        Ok(true)
    }

    pub(super) fn install_vm_session(
        &mut self,
        request: preparation::VmProviderLaunchRequest,
        mut session: SessionHandle,
        bootstrap: Option<Value>,
        restored: bool,
    ) -> Result<(), WorkerError> {
        let control = session.control.clone();
        let mut events = std::mem::replace(&mut session.events, mpsc::channel(1).1);
        self.vm_incarnation_counter = self
            .vm_incarnation_counter
            .checked_add(1)
            .ok_or(WorkerError::TaskFailed)?;
        let actor_incarnation = self.vm_incarnation_counter;
        let source = VmActorIdentity {
            actor_id: request.agent_id.clone(),
            epoch: request.epoch,
            incarnation: actor_incarnation,
        };
        let sender = self.vm_event_tx.clone();
        self.vm_event_forwarders.push(tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if sender
                    .send(VmForwardedMessage::Event {
                        source: source.clone(),
                        event,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
            let _sent = sender.send(VmForwardedMessage::Exit { source }).await;
        }));
        let had_active = self.has_active_claude_vm_sessions();
        self.callback_scan = true;
        self.vm_sessions.insert(
            request.agent_id.clone(),
            ActorProviderSession {
                session,
                epoch: request.epoch,
                incarnation: actor_incarnation,
                control,
                launch_request: request,
                bootstrap,
            },
        );
        let now = tokio::time::Instant::now();
        self.vm_credential_refresh_at = vm_refresh_deadline_after_session_change(
            had_active,
            self.has_active_claude_vm_sessions(),
            self.resources.vm_provider_launch.is_some(),
            self.vm_credential_refresh_at,
            now,
        );
        if restored && !had_active && self.vm_credential_refresh_at.is_some() {
            // Restore the normal cadence, not an immediate credential operation.
            self.vm_credential_refresh_at = Some(now + VM_CREDENTIAL_REFRESH_INTERVAL);
        }
        Ok(())
    }
}
