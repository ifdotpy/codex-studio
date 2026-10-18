use super::storage::{Replay, replay, submitted};
use super::*;
impl Worker {
    fn retained_scope(&self) -> ConnectionScope {
        ConnectionScope {
            account: self.account.clone(),
            connection: self.connection_id.clone(),
            generation: self.generation.0,
        }
    }
    pub(in crate::account_worker) async fn retained_request(
        &mut self,
        command: RequestCommand,
    ) -> Result<(), WorkerError> {
        let RequestCommand { request, reply } = command;
        let stale_scope = request.scope != self.retained_scope();
        if request.scope.account != self.account {
            let _ignored = reply.send(Err(PortError::Identity));
            return Ok(());
        }
        if reply.is_closed()
            || tokio::time::Instant::from_std(request.deadline) <= tokio::time::Instant::now()
        {
            let _ignored = reply.send(Err(PortError::NotSent));
            return Ok(());
        }
        if let Err(error) = validate(&request) {
            let _ignored = reply.send(Err(error));
            return Ok(());
        }
        if self.retained_requests.len() + self.pending.len() >= MAX_PENDING_RESPONSES {
            let _ignored = reply.send(Err(PortError::Capacity));
            return Ok(());
        }
        let deferred_request = request.clone();
        let call = Call {
            scope: request.scope,
            operation: request.operation,
            actor: request.actor,
            transfer: request.transfer,
            deleted_actor_session: request.deleted_actor_session,
            method: request.method.as_str().into(),
            params: request.params,
        };
        let key = call.key();
        let check = call.clone();
        let prior = self
            .db("retained_replay", move |store| async move {
                replay(&store, &check).await
            })
            .await?;
        let retry_ticket = match prior {
            Err(error) => {
                let _ignored = reply.send(Err(error));
                return Ok(());
            }
            Ok(Replay::Return(receipt)) => {
                let _ignored = reply.send(Ok(receipt));
                return Ok(());
            }
            Ok(Replay::Miss) if stale_scope => {
                // The recorded connection is gone. Without a durable submitted
                // receipt, never send its request through a replacement worker.
                let _ignored = reply.send(Err(PortError::Unknown));
                return Ok(());
            }
            Ok(Replay::Miss) => None,
            Ok(Replay::Retry(ticket)) if stale_scope => {
                // Retry is permitted only on the connection that proved the
                // prior attempt NotSent. Preserve that certainty across restart.
                let mut receipt = call.receipt(ticket, RequestResult::NotSent);
                receipt.replayed = true;
                let _ignored = reply.send(Ok(receipt));
                return Ok(());
            }
            Ok(Replay::Retry(ticket)) => Some(ticket),
        };
        let actor_id = call.actor.as_ref().map(|actor| actor.agent.clone());
        let actor_lookup = actor_id.clone();
        let is_vm_actor = if let Some(actor) = actor_lookup {
            self.db("retained_actor_mode", move |store| async move {
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        let record =
                            AgentRepository::get(connection, &AgentId::from(actor.as_str()))?;
                        Ok(record.is_some_and(|record| {
                            record.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
                        }))
                    })
                    .await
            })
            .await?
        } else {
            false
        };
        if is_vm_actor
            && let Some(actor) = actor_id.as_deref()
            && !self.vm_sessions.contains_key(actor)
            && self.resolving_vm_actors.contains(actor)
        {
            if self.queued_commands.len() >= COMMAND_CAPACITY {
                let _ignored = reply.send(Err(PortError::Capacity));
            } else {
                self.queued_commands
                    .push_back(AccountCommand::RetainedRequest(Box::new(RequestCommand {
                        request: deferred_request,
                        reply,
                    })));
            }
            return Ok(());
        }
        if is_vm_actor
            && let Some(actor) = actor_id.as_deref()
            && let Some(actor_identity) = call.actor.as_ref()
            && !self
                .vm_session_matches_actor(actor, &call.scope.account, actor_identity.epoch)
                .await?
        {
            let _ignored = reply.send(Err(PortError::NotSent));
            return Ok(());
        }
        let control = if is_vm_actor {
            let Some(control) = actor_id
                .as_deref()
                .and_then(|actor| self.vm_sessions.get(actor))
                .map(|session| session.control.clone())
            else {
                let _ignored = reply.send(Err(PortError::NotSent));
                return Ok(());
            };
            control
        } else {
            self.control.clone()
        };
        let journal_actor_id = if is_vm_actor { actor_id.clone() } else { None };
        let remaining = tokio::time::Instant::from_std(request.deadline)
            .saturating_duration_since(tokio::time::Instant::now());
        let deadline = tokio::time::Instant::from_std(request.deadline);
        let prepare = control.prepare(RequestSpec {
            id: RequestId::String(key.clone()),
            operation_id: OperationId(key),
            method: request.method,
            params: call.params.clone(),
            thread_id: call.params["threadId"].as_str().map(str::to_owned),
            lightweight: true,
            timeout_ms: remaining.as_millis().try_into().unwrap_or(u64::MAX),
        });
        let prepared = self
            .drive(async {
                tokio::time::timeout_at(deadline, prepare)
                    .await
                    .unwrap_or(Err(SessionTaskError::Closed))
            })
            .await;
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(_) => {
                let _ignored = reply.send(Err(PortError::NotSent));
                return Ok(());
            }
        };
        if self.closing
            || reply.is_closed()
            || tokio::time::Instant::from_std(request.deadline) <= tokio::time::Instant::now()
        {
            let _ignored = reply.send(Err(PortError::NotSent));
            return Ok(());
        }
        let recovery = prepared.recovery_record();
        let ticket = recovery.ticket.clone();
        let save_call = call.clone();
        let authorized = self
            .db("retained_submitted", move |s| async move {
                submitted(&s, &save_call, &recovery, retry_ticket.as_ref()).await
            })
            .await?;
        if let Err(error) = authorized {
            let _ignored = reply.send(Err(error));
            return Ok(());
        }
        self.retained_requests.insert(
            ticket.id.clone(),
            PendingRequest {
                call,
                ticket: ticket.clone(),
                actor_id: journal_actor_id.clone(),
                reply: Some(reply),
                result: None,
                result_cursor: 0,
            },
        );
        // Once intent is durable, cancellation cannot authorize another write.
        if self.closing
            || tokio::time::Instant::from_std(request.deadline) <= tokio::time::Instant::now()
        {
            self.retained_outcome(
                &SessionOutcome::Write {
                    operation_id: OperationId(String::new()),
                    ticket: Some(ticket),
                    outcome: WriteOutcome::NotSent,
                    recovery: None,
                },
                0,
                journal_actor_id.as_deref(),
            )
            .await?;
            return self.complete_retained().await;
        }
        if let Err(error) = self
            .drive(async {
                tokio::time::timeout_at(deadline, control.submit(prepared))
                    .await
                    .unwrap_or(Err(SessionTaskError::AdmissionUnknown))
            })
            .await
        {
            let outcome = if definite_rejection(&error) {
                WriteOutcome::NotSent
            } else {
                WriteOutcome::Unknown
            };
            self.retained_outcome(
                &SessionOutcome::Write {
                    operation_id: OperationId(String::new()),
                    ticket: Some(ticket),
                    outcome,
                    recovery: None,
                },
                0,
                journal_actor_id.as_deref(),
            )
            .await?;
            self.complete_retained().await?;
        }
        Ok(())
    }
    pub(in crate::account_worker) async fn retained_outcome(
        &mut self,
        outcome: &SessionOutcome,
        cursor: u64,
        actor_id: Option<&str>,
    ) -> Result<bool, WorkerError> {
        let ticket = match outcome {
            SessionOutcome::Write {
                ticket: Some(t), ..
            }
            | SessionOutcome::Resolved { ticket: t, .. } => t,
            _ => return Ok(false),
        };
        let Some(pending) = self.retained_requests.get(&ticket.id) else {
            return Ok(false);
        };
        if pending.actor_id.as_deref() != actor_id {
            return Ok(false);
        }
        if &pending.ticket != ticket {
            return Err(WorkerError::PreparationRefused(
                "retained-session/ticket-mismatch".into(),
            ));
        }
        let result = match outcome {
            SessionOutcome::Resolved {
                outcome:
                    RequestOutcome::Reply {
                        body: ResponseBody::Result(value),
                        ..
                    },
                ..
            } => Some(RequestResult::Reply(value.clone())),
            SessionOutcome::Resolved {
                outcome:
                    RequestOutcome::Reply {
                        body: ResponseBody::Error(value),
                        ..
                    },
                ..
            } => Some(RequestResult::Rejected(value.clone())),
            SessionOutcome::Resolved {
                outcome: RequestOutcome::Unknown,
                ..
            } => Some(RequestResult::Unknown),
            SessionOutcome::Write {
                outcome: WriteOutcome::NotSent,
                ..
            } => Some(RequestResult::NotSent),
            SessionOutcome::Write {
                outcome: WriteOutcome::Unknown,
                ..
            } => Some(RequestResult::Unknown),
            _ => None,
        };
        let key = pending.call.key();
        let value = result.clone();
        let written = match outcome {
            SessionOutcome::Write {
                recovery: Some(recovery),
                ..
            } => Some(store::record(recovery)),
            _ => None,
        };
        let accepted = self
            .db("retained_result", move |s| async move {
                scheduler::lifecycle_transaction_with(&s, move |tx| {
                    if let Some(written) = written {
                        store::replace(tx, &key, "written", written)?;
                    }
                    let Some(value) = value else {
                        return Ok(None);
                    };
                    if let Some(old) = store::receipt(tx, &key, "result")? {
                        let previous: RequestResult = serde_json::from_value(old["result"].clone())
                            .map_err(|_| WorkerError::Capacity)?;
                        if matches!(
                            previous,
                            RequestResult::Reply(_) | RequestResult::Rejected(_)
                        ) {
                            return Ok(Some((
                                previous,
                                old["cursor"].as_u64().ok_or(WorkerError::Capacity)?,
                            )));
                        }
                    }
                    store::replace(tx, &key, "result", json!({"result":value,"cursor":cursor}))?;
                    Ok(Some((value, cursor)))
                })
                .await
            })
            .await?;
        if let Some((result, cursor)) = accepted
            && let Some(pending) = self.retained_requests.get_mut(&ticket.id)
        {
            pending.result = Some(result);
            pending.result_cursor = cursor;
        }
        Ok(true)
    }
    pub(in crate::account_worker) async fn complete_retained(&mut self) -> Result<(), WorkerError> {
        if self.retained_requests.is_empty() && self.retained_barriers.is_empty() {
            return self.idle_local_work_completed().await;
        }
        // Only a live managed callback can make the checkpoint wait cyclically:
        // its provider reply is later in the journal, while the earlier callback
        // cursor cannot be acknowledged until the callback writes its response.
        // Verify each candidate reply event was committed before returning it.
        // Keep its correlation until checkpoint; after a crash, replay still
        // reports Unknown for this beyond-checkpoint result and never resends.
        let account = self.account.clone();
        let retained_account = account.clone();
        let generation = self.generation.0;
        let mut scopes: BTreeSet<String> = self
            .retained_requests
            .values()
            .map(|pending| self.journal_scope(pending.actor_id.as_deref()))
            .collect();
        // A journal barrier always belongs to the host session, including when
        // no retained requests are currently waiting.
        scopes.insert(account.clone());
        let request_scopes: BTreeMap<_, _> = self
            .retained_requests
            .iter()
            .map(|(id, pending)| (id.clone(), self.journal_scope(pending.actor_id.as_deref())))
            .collect();
        let candidates: BTreeSet<(String, u64)> =
            self.retained_requests
                .values()
                .filter(|pending| {
                    pending.result_cursor > 0
                        && self.managed_callback_cursors.iter().any(
                            |(actor, actor_epoch, callback_cursor)| {
                                // Callback ownership follows the transport lane.
                                // Host requests can still carry an authorized actor,
                                // but their replies are journaled on the host session.
                                let transport_epoch = pending.actor_id.as_ref().and_then(|_| {
                                    pending.call.actor.as_ref().map(|owner| owner.epoch)
                                });
                                actor == &pending.actor_id
                                    && *actor_epoch == transport_epoch
                                    && *callback_cursor < pending.result_cursor
                            },
                        )
                })
                .map(|pending| {
                    (
                        self.journal_scope(pending.actor_id.as_deref()),
                        pending.result_cursor,
                    )
                })
                .collect();
        let (checkpoints, committed_replies) = self
            .db("retained_barrier", move |s| async move {
                s.transaction_with(move |tx| {
                    let mut checkpoints = BTreeMap::new();
                    for scope in scopes {
                        checkpoints
                            .insert(scope.clone(), store::checkpoint(tx, &scope, generation)?);
                    }
                    let committed = candidates
                        .into_iter()
                        .filter_map(|(scope, cursor)| {
                            match store::journal_committed(tx, &scope, generation, cursor) {
                                Ok(true) => Some(Ok((scope, cursor))),
                                Ok(false) => None,
                                Err(error) => Some(Err(error)),
                            }
                        })
                        .collect::<Result<BTreeSet<_>, _>>()?;
                    Ok((checkpoints, committed))
                })
                .await
            })
            .await?;
        self.retained_requests.retain(|id, pending| {
            let scope = request_scopes
                .get(id)
                .cloned()
                .unwrap_or_else(|| retained_account.clone());
            let checkpoint = checkpoints.get(&scope).copied().unwrap_or(0);
            if pending.result_cursor > checkpoint {
                if let Some(result @ (RequestResult::Reply(_) | RequestResult::Rejected(_))) =
                    pending.result.clone()
                    && committed_replies.contains(&(scope, pending.result_cursor))
                    && let Some(reply) = pending.reply.take()
                {
                    let _ignored =
                        reply.send(Ok(pending.call.receipt(pending.ticket.clone(), result)));
                }
                return true;
            }
            if let Some(result) = pending.result.take() {
                if let Some(reply) = pending.reply.take() {
                    let _ignored = reply.send(Ok(pending
                        .call
                        .receipt(pending.ticket.clone(), result.clone())));
                }
                return result == RequestResult::Unknown;
            }
            true
        });
        if self.retained_barriers.is_empty() {
            return self.idle_local_work_completed().await;
        }
        self.retained_barriers.retain_mut(|pending| {
            let result = if pending.cursor <= checkpoints.get(&account).copied().unwrap_or(0) {
                Some(Ok(()))
            } else if tokio::time::Instant::from_std(pending.command.deadline)
                <= tokio::time::Instant::now()
            {
                Some(Err(PortError::Deadline))
            } else {
                None
            };
            if let Some(result) = result {
                let (dummy, _) = oneshot::channel();
                let reply = std::mem::replace(&mut pending.command.reply, dummy);
                let _ignored = reply.send(result);
                false
            } else {
                !pending.command.reply.is_closed()
            }
        });
        self.idle_local_work_completed().await
    }
    pub(in crate::account_worker) async fn retained_barrier(
        &mut self,
        command: BarrierCommand,
    ) -> Result<(), WorkerError> {
        if command.scope != self.retained_scope() {
            let _ignored = command.reply.send(Err(PortError::Identity));
            return Ok(());
        }
        self.retained_barriers
            .retain(|b| !b.command.reply.is_closed());
        if self.retained_barriers.len() >= COMMAND_CAPACITY {
            let _ignored = command.reply.send(Err(PortError::Capacity));
            return Ok(());
        }
        let control = self.control.clone();
        // No store permit is retained while the session delivers its prefix.
        let deadline = tokio::time::Instant::from_std(command.deadline);
        let barrier = async move {
            tokio::time::timeout_at(deadline, control.journal_barrier())
                .await
                .unwrap_or(Err(SessionTaskError::Closed))
        };
        match self.drive(barrier).await {
            Ok(cursor) => self.retained_barriers.push(PendingBarrier {
                cursor: cursor.0,
                command,
            }),
            Err(_) => {
                let _ignored = command.reply.send(Err(PortError::Deadline));
            }
        }
        self.complete_retained().await
    }
}
