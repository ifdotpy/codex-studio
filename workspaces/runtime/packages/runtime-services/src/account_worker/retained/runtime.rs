use super::storage::{replay, submitted};
use super::*;
impl Worker {
    fn retained_scope(&self) -> ConnectionScope {
        ConnectionScope {
            account: self.account.clone(),
            connection: self.generation.0.to_string(),
            generation: self.generation.0,
        }
    }
    pub(in crate::account_worker) async fn retained_request(
        &mut self,
        command: RequestCommand,
    ) -> Result<(), WorkerError> {
        let RequestCommand { request, reply } = command;
        if request.scope != self.retained_scope() {
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
        let call = Call {
            scope: request.scope,
            operation: request.operation,
            actor: request.actor,
            method: request.method.as_str().into(),
            params: request.params,
        };
        let key = call.key();
        let check = call.clone();
        let prior = self
            .db("retained_replay", move |s| replay(s, &check))
            .await?;
        match prior {
            Err(error) => {
                let _ignored = reply.send(Err(error));
                return Ok(());
            }
            Ok(Some(receipt)) => {
                let _ignored = reply.send(Ok(receipt));
                return Ok(());
            }
            Ok(None) => {}
        }
        let control = self.control.clone();
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
            .db("retained_submitted", move |s| {
                submitted(s, &save_call, &recovery)
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
            )
            .await?;
            return self.complete_retained().await;
        }
        let control = self.control.clone();
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
            .db("retained_result", move |s| {
                scheduler::lifecycle_transaction_with(s, |tx| {
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
            return Ok(());
        }
        // Called only after the containing journal has committed. Keep Unknown
        // correlations for a late positive response; terminal replies retire.
        let account = self.account.clone();
        let generation = self.generation.0;
        let checkpoint = self
            .db("retained_barrier", move |s| {
                s.transaction_with(|tx| store::checkpoint(tx, &account, generation))
            })
            .await?;
        self.retained_requests.retain(|_, pending| {
            if pending.result_cursor > checkpoint {
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
            return Ok(());
        }
        self.retained_barriers.retain_mut(|pending| {
            let result = if pending.cursor <= checkpoint {
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
        Ok(())
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
