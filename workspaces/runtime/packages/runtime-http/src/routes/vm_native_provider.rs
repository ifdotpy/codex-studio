use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use studio_runtime_providers::{RequestId, RequestTicket};
use studio_runtime_schema::shared_values::LINUX_VM_PROVIDER_HANDLE_PREFIX;
use studio_runtime_services::linux_vm_guest::GuestError;
use studio_runtime_services::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, NotSentReason, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    linux_vm_guest::GuestRpc,
};

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "vm_native_provider",
        routes: None,
        dependencies: &[],
    }
}

/// The VM guest is the provider endpoint for native actions on layr agents.
#[derive(Clone)]
pub(crate) struct VmNativeProvider {
    guest: Arc<dyn GuestRpc>,
    notification_sink: Arc<dyn AccountSessionPort>,
    scope: ConnectionScope,
    handle: String,
    agent_id: String,
    agent_epoch: u64,
}

impl VmNativeProvider {
    pub(crate) fn new(
        guest: Arc<dyn GuestRpc>,
        notification_sink: Arc<dyn AccountSessionPort>,
        account: &str,
        agent_id: &str,
        epoch: u64,
    ) -> Self {
        Self {
            guest,
            notification_sink,
            handle: format!("{LINUX_VM_PROVIDER_HANDLE_PREFIX}{agent_id}"),
            agent_id: agent_id.to_owned(),
            agent_epoch: epoch,
            scope: ConnectionScope {
                account: account.to_owned(),
                connection: format!("vm-guest:{agent_id}"),
                generation: epoch.saturating_add(1),
            },
        }
    }
}

impl AccountSessionPort for VmNativeProvider {
    fn ensure_session<'a>(
        &'a self,
        account: &'a str,
    ) -> BoxFuture<'a, Result<ConnectionScope, AccountSessionError>> {
        Box::pin(async move {
            self.current_scope(account)
                .ok_or(AccountSessionError::Refused("vm-provider/account"))
        })
    }

    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        (account == self.scope.account).then(|| self.scope.clone())
    }

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> BoxFuture<'a, Result<SessionReceipt, AccountSessionError>> {
        Box::pin(async move {
            if request.account != self.scope.account
                || request.expected_scope.as_ref() != Some(&self.scope)
                || request.operation.is_empty()
            {
                return Err(AccountSessionError::Refused("vm-provider/scope"));
            }
            let id = RequestId::String(format!(
                "retained:{}:{}",
                self.scope.account,
                studio_runtime_store::sha256_hex(request.operation.as_bytes())
            ));
            let ticket = RequestTicket {
                generation: self.scope.generation,
                id,
                method: request.method,
            };
            let timeout = request.deadline.saturating_duration_since(Instant::now());
            let result = self
                .request_guest(&request.operation, &ticket, &request.params, timeout)
                .await;
            let (certainty, result, not_sent_reason, ticket_value) = match result {
                Ok(value) => (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Reply(value)),
                    None,
                    Some(ticket),
                ),
                Err(studio_runtime_services::linux_vm_guest::GuestError::Rejected {
                    code,
                    detail,
                    uncertain: false,
                }) => (
                    RequestCertainty::Written,
                    Some(ProviderResponse::Rejected(
                        serde_json::json!({"code":code,"detail":detail}),
                    )),
                    None,
                    Some(ticket),
                ),
                Err(error) if error.uncertain() => {
                    (RequestCertainty::Unknown, None, None, Some(ticket))
                }
                Err(_) => (
                    RequestCertainty::NotSent,
                    None,
                    Some(NotSentReason::WorkerNotSent),
                    Some(ticket),
                ),
            };
            Ok(SessionReceipt {
                scope: self.scope.clone(),
                operation: request.operation,
                ticket: ticket_value,
                certainty,
                result,
                not_sent_reason,
                replayed: false,
            })
        })
    }
}

impl VmNativeProvider {
    async fn guest_call(
        &self,
        params: Value,
        request_id: Option<String>,
        timeout: Duration,
        uncertain: bool,
    ) -> Result<Value, GuestError> {
        let guest = Arc::clone(&self.guest);
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/vm-native-provider",
            move || guest.call("provider.rpc", &params, request_id.as_deref(), timeout),
        )
        .await
        .map_err(|_| GuestError::Protocol {
            message: "native provider worker failed",
            uncertain,
        })?
    }

    async fn request_guest(
        &self,
        operation: &str,
        ticket: &RequestTicket,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, GuestError> {
        if timeout.is_zero() {
            return Err(GuestError::InvalidRequest);
        }
        let deadline = Instant::now() + timeout;
        let scope = format!(
            "vm-native:{}",
            studio_runtime_store::sha256_hex(operation.as_bytes())
        );
        let info = self
            .guest_call(
                json!({"handle":self.handle,"action":"info"}),
                Some(format!("{scope}:info")),
                deadline.saturating_duration_since(Instant::now()),
                false,
            )
            .await?;
        // Resume from the last durably acknowledged event. `lastSeq` can include
        // this operation's reply if a prior call lost its response; starting
        // there would skip the only replayable proof of that write.
        let mut cursor =
            info.get("acknowledged")
                .and_then(Value::as_u64)
                .ok_or(GuestError::Protocol {
                    message: "invalid native supervisor acknowledgement cursor",
                    uncertain: false,
                })?;
        if info
            .get("lastSeq")
            .and_then(Value::as_u64)
            .is_none_or(|last| cursor > last)
        {
            return Err(GuestError::Protocol {
                message: "invalid native supervisor event range",
                uncertain: false,
            });
        }
        let digest = studio_runtime_store::sha256_hex(operation.as_bytes());
        let native_id = u64::from_str_radix(&digest[..16], 16).map_err(|_| GuestError::Identity)?
            & i64::MAX as u64;
        let envelope = json!({
            "handle": self.handle,
            "action": "write",
            "operationId": operation,
            "nativeId": native_id,
            "message": {
                "id": native_id,
                "method": ticket.method.as_str(),
                "params": params,
            },
        });
        let written = self
            .guest_call(
                envelope,
                Some(operation.to_owned()),
                deadline.saturating_duration_since(Instant::now()),
                true,
            )
            .await?;
        let remote_id =
            written
                .get("remoteId")
                .and_then(Value::as_u64)
                .ok_or(GuestError::Protocol {
                    message: "native supervisor did not return a remote request identity",
                    uncertain: true,
                })?;

        // One guest journal has one owner for each event. This action owns
        // the notifications it delivers and its own reply. The agent's
        // provider session owns every other event (a server request such as
        // an approval or a tool call, a reply to its own request, stderr and
        // exit): it delivers, answers and settles that event before its ACK.
        // An ACK covers every earlier event and the guest deletes them, so
        // this action delivers and acknowledges only while each event so far
        // is its own. After the first event that it does not own, it only
        // scans for its reply and leaves that event and all later events in
        // the journal for the session, which reads them in order.
        let mut owned_prefix = true;
        while Instant::now() < deadline {
            let next = self
                .guest_call(
                    json!({"handle":self.handle,"action":"next","cursor":cursor}),
                    Some(format!("{scope}:next:{cursor}")),
                    deadline.saturating_duration_since(Instant::now()),
                    true,
                )
                .await
                .map_err(|_| GuestError::Protocol {
                    message: "native provider event polling failed after the write",
                    uncertain: true,
                })?;
            let Some(event) = next.get("event").filter(|event| !event.is_null()) else {
                if next.get("returnCode").is_some_and(|value| !value.is_null()) {
                    return Err(GuestError::Protocol {
                        message: "native provider exited before replying",
                        uncertain: true,
                    });
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            cursor = event
                .get("sequence")
                .and_then(Value::as_u64)
                .ok_or(GuestError::Protocol {
                    message: "invalid native event sequence",
                    uncertain: true,
                })?;
            let frame = if event.get("kind").and_then(Value::as_str) == Some("stdout") {
                let Some(payload) = event.get("payload").and_then(Value::as_str) else {
                    return Err(GuestError::Protocol {
                        message: "native provider event payload is unavailable",
                        uncertain: true,
                    });
                };
                let decoded = studio_runtime_providers::decode_journal_entry(
                    cursor,
                    self.scope.generation,
                    payload.as_bytes().to_vec(),
                );
                match decoded.payload {
                    studio_runtime_providers::DecodedJournalPayload::Frame(frame) => Some((
                        frame,
                        serde_json::from_str::<Value>(payload).map_err(|_| {
                            GuestError::Protocol {
                                message: "invalid native provider event frame",
                                uncertain: true,
                            }
                        })?,
                    )),
                    // Raw stdout belongs to the session's journal.
                    studio_runtime_providers::DecodedJournalPayload::RawStdout { .. } => None,
                }
            } else {
                None
            };
            let reply = frame.as_ref().is_some_and(|(_, value)| {
                value.get("method").is_none()
                    && value.get("id").and_then(Value::as_u64) == Some(remote_id)
            });
            let notification = frame.as_ref().is_some_and(|(frame, _)| match frame {
                studio_runtime_providers::Frame::Notification(_) => true,
                studio_runtime_providers::Frame::UnknownMethod(frame) => {
                    !frame.fields.contains_key("id")
                }
                _ => false,
            });
            owned_prefix &= reply || notification;
            if owned_prefix {
                if notification && let Some((frame, _)) = &frame {
                    self.notification_sink
                        .deliver_vm_notification(
                            &self.scope.account,
                            &self.agent_id,
                            self.agent_epoch,
                            cursor,
                            frame.clone(),
                        )
                        .await
                        .map_err(|_| GuestError::Protocol {
                            message: "native provider notification was not durably delivered",
                            uncertain: true,
                        })?;
                }
                self.ack_event(&scope, cursor, deadline).await?;
            }
            if reply && let Some((_, value)) = frame {
                return reply_body(value);
            }
        }
        Err(GuestError::Protocol {
            message: "native provider reply timed out",
            uncertain: true,
        })
    }

    async fn ack_event(
        &self,
        scope: &str,
        sequence: u64,
        deadline: Instant,
    ) -> Result<(), GuestError> {
        self.guest_call(
            json!({"handle":self.handle,"action":"ack","sequence":sequence}),
            Some(format!("{scope}:ack:{sequence}")),
            deadline.saturating_duration_since(Instant::now()),
            true,
        )
        .await
        .map(|_| ())
        .map_err(|_| GuestError::Protocol {
            message: "native provider event acknowledgement failed",
            uncertain: true,
        })
    }
}

fn reply_body(frame: Value) -> Result<Value, GuestError> {
    if let Some(error) = frame.get("error") {
        return Err(GuestError::Rejected {
            code: error.get("code").and_then(Value::as_str).map(str::to_owned),
            detail: error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned),
            uncertain: false,
        });
    }
    frame.get("result").cloned().ok_or(GuestError::Protocol {
        message: "native provider reply has no result",
        uncertain: true,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use studio_runtime_providers::Method;

    type NotificationDelivery = (String, String, u64, u64, studio_runtime_providers::Frame);
    type NotificationDeliveries = Arc<Mutex<Vec<NotificationDelivery>>>;

    struct NotificationSink {
        deliveries: NotificationDeliveries,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl AccountSessionPort for NotificationSink {
        fn deliver_vm_notification<'a>(
            &'a self,
            account: &'a str,
            agent_id: &'a str,
            epoch: u64,
            sequence: u64,
            frame: studio_runtime_providers::Frame,
        ) -> BoxFuture<'a, Result<(), AccountSessionError>> {
            Box::pin(async move {
                self.deliveries.lock().unwrap().push((
                    account.to_owned(),
                    agent_id.to_owned(),
                    epoch,
                    sequence,
                    frame,
                ));
                self.order.lock().unwrap().push("notification");
                Ok(())
            })
        }
    }

    fn notification_sink(
        order: Arc<Mutex<Vec<&'static str>>>,
    ) -> (Arc<NotificationSink>, NotificationDeliveries) {
        let deliveries = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(NotificationSink {
                deliveries: deliveries.clone(),
                order,
            }),
            deliveries,
        )
    }

    struct FakeGuest {
        calls: Mutex<Vec<(String, serde_json::Value)>>,
        fail_poll: bool,
        notification: bool,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl GuestRpc for FakeGuest {
        fn stream(
            &self,
            method: &str,
            params: &serde_json::Value,
            _request_id: Option<&str>,
            _timeout: std::time::Duration,
            _on_frame: &mut dyn FnMut(
                serde_json::Value,
            ) -> Result<
                (),
                studio_runtime_services::linux_vm_guest::GuestError,
            >,
        ) -> Result<serde_json::Value, studio_runtime_services::linux_vm_guest::GuestError>
        {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), params.clone()));
            if params["action"] == "ack" {
                self.order.lock().unwrap().push("ack");
            }
            if params["action"] == "next" && self.fail_poll {
                return Err(GuestError::Transport {
                    message: "fake poll failure".into(),
                    uncertain: false,
                });
            }
            Ok(match params["action"].as_str() {
                Some("info") => json!({"lastSeq":0,"acknowledged":0,"state":"running"}),
                Some("write") => json!({"remoteId":7,"accepted":true}),
                Some("next") if self.notification && params["cursor"] == 0 => json!({
                    "event":{"sequence":1,"kind":"stdout","payload":"{\"method\":\"account/updated\",\"params\":{}}"},
                    "returnCode":null
                }),
                Some("next") => {
                    let sequence = params["cursor"].as_u64().unwrap_or_default() + 1;
                    json!({
                        "event":{"sequence":sequence,"kind":"stdout","payload":"{\"id\":7,\"result\":{\"ok\":true}}"},
                        "returnCode":null
                    })
                }
                _ => json!({"ok":true}),
            })
        }
    }

    #[tokio::test]
    async fn compact_is_sent_to_the_fake_vm_guest_with_a_scoped_receipt() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_poll: false,
            notification: false,
            order: order.clone(),
        });
        let (sink, _) = notification_sink(order);
        let provider = VmNativeProvider::new(guest.clone(), sink, "acct", "vm-agent", 4);
        let scope = provider.current_scope("acct").unwrap();
        let receipt = provider
            .provider_request(ProviderRequest {
                account: "acct".into(),
                operation: "op-1".into(),
                actor: None,
                expected_scope: Some(scope.clone()),
                deleted_actor_session: None,
                method: Method::ThreadCompactStart,
                params: json!({"threadId":"guest-thread"}),
                deadline: Instant::now() + std::time::Duration::from_secs(3),
            })
            .await
            .expect("VM compact action retains its provider result");
        let calls = guest.calls.lock().unwrap();
        assert_eq!(calls[0].0, "provider.rpc");
        assert_eq!(calls[0].1["handle"], "linux-worker:vm-agent");
        assert!(calls.iter().all(|(method, _)| method == "provider.rpc"));
        assert_eq!(calls[1].1["action"], "write");
        assert_eq!(calls[1].1["operationId"], "op-1");
        assert_eq!(calls[1].1["message"]["method"], "thread/compact/start");
        assert_eq!(calls[1].1["message"]["params"]["threadId"], "guest-thread");
        assert_eq!(
            receipt.result,
            Some(ProviderResponse::Reply(json!({"ok":true})))
        );
        assert_eq!(receipt.scope, scope);
        assert_eq!(receipt.operation, "op-1");
        assert_eq!(receipt.certainty, RequestCertainty::Written);
        assert!(receipt.ticket.is_some());
    }

    #[tokio::test]
    async fn poll_failure_after_native_write_is_unknown() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_poll: true,
            notification: false,
            order: order.clone(),
        });
        let (sink, _) = notification_sink(order);
        let provider = VmNativeProvider::new(guest, sink, "acct", "vm-agent", 4);
        let receipt = provider
            .provider_request(ProviderRequest {
                account: "acct".into(),
                operation: "op-poll-failure".into(),
                actor: None,
                expected_scope: provider.current_scope("acct"),
                deleted_actor_session: None,
                method: Method::ThreadCompactStart,
                params: json!({"threadId":"guest-thread"}),
                deadline: Instant::now() + std::time::Duration::from_secs(3),
            })
            .await
            .expect("VM review action retains its provider result");
        assert_eq!(receipt.certainty, RequestCertainty::Unknown);
        assert!(receipt.ticket.is_some());
        assert!(receipt.not_sent_reason.is_none());
    }

    #[tokio::test]
    async fn prewrite_not_sent_receipt_keeps_its_request_ticket() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_poll: false,
            notification: false,
            order: order.clone(),
        });
        let (sink, _) = notification_sink(order);
        let provider = VmNativeProvider::new(guest, sink, "acct", "vm-agent", 4);
        let receipt = provider
            .provider_request(ProviderRequest {
                account: "acct".into(),
                operation: "op-expired".into(),
                actor: None,
                expected_scope: provider.current_scope("acct"),
                deleted_actor_session: None,
                method: Method::ThreadCompactStart,
                params: json!({"threadId":"guest-thread"}),
                deadline: Instant::now(),
            })
            .await
            .unwrap();
        assert_eq!(receipt.certainty, RequestCertainty::NotSent);
        assert!(receipt.ticket.is_some());
        assert_eq!(receipt.not_sent_reason, Some(NotSentReason::WorkerNotSent));
    }

    #[tokio::test]
    async fn guest_notification_is_delivered_before_its_event_is_acknowledged() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_poll: false,
            notification: true,
            order: order.clone(),
        });
        let (sink, deliveries) = notification_sink(order.clone());
        let provider = VmNativeProvider::new(guest, sink, "acct", "vm-agent", 4);
        let receipt = provider
            .provider_request(ProviderRequest {
                account: "acct".into(),
                operation: "op-notification".into(),
                actor: None,
                expected_scope: provider.current_scope("acct"),
                deleted_actor_session: None,
                method: Method::ThreadCompactStart,
                params: json!({"threadId":"guest-thread"}),
                deadline: Instant::now() + std::time::Duration::from_secs(3),
            })
            .await
            .unwrap();
        assert_eq!(receipt.certainty, RequestCertainty::Written);
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].0, "acct");
        assert_eq!(deliveries[0].1, "vm-agent");
        assert_eq!(deliveries[0].2, 4);
        assert_eq!(deliveries[0].3, 1);
        assert!(matches!(
            &deliveries[0].4,
            studio_runtime_providers::Frame::Notification(_)
        ));
        assert_eq!(*order.lock().unwrap(), ["notification", "ack", "ack"]);
    }

    #[tokio::test]
    async fn review_notification_is_delivered_before_its_event_is_acknowledged() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let guest = Arc::new(FakeGuest {
            calls: Mutex::new(Vec::new()),
            fail_poll: false,
            notification: true,
            order: order.clone(),
        });
        let (sink, deliveries) = notification_sink(order.clone());
        let provider = VmNativeProvider::new(guest, sink, "acct", "vm-agent", 4);
        let receipt = provider
            .provider_request(ProviderRequest {
                account: "acct".into(),
                operation: "op-review-notification".into(),
                actor: None,
                expected_scope: provider.current_scope("acct"),
                deleted_actor_session: None,
                method: Method::ReviewStart,
                params: json!({"threadId":"guest-thread"}),
                deadline: Instant::now() + std::time::Duration::from_secs(3),
            })
            .await
            .unwrap();
        assert_eq!(receipt.certainty, RequestCertainty::Written);
        assert_eq!(deliveries.lock().unwrap().len(), 1);
        assert_eq!(*order.lock().unwrap(), ["notification", "ack", "ack"]);
    }

    /// Answers `next` from a fixed journal; the write gets remote ID 7.
    struct ScriptedGuest {
        events: Vec<Value>,
        acks: Mutex<Vec<u64>>,
        order: Arc<Mutex<Vec<&'static str>>>,
    }

    impl GuestRpc for ScriptedGuest {
        fn stream(
            &self,
            _method: &str,
            params: &Value,
            _request_id: Option<&str>,
            _timeout: std::time::Duration,
            _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
        ) -> Result<Value, GuestError> {
            Ok(match params["action"].as_str() {
                Some("info") => json!({"lastSeq":0,"acknowledged":0,"state":"running"}),
                Some("write") => json!({"remoteId":7,"accepted":true}),
                Some("next") => {
                    let cursor = usize::try_from(params["cursor"].as_u64().unwrap()).unwrap();
                    let event = self.events.get(cursor).map(|payload| {
                        json!({"sequence":cursor + 1,"kind":"stdout","payload":payload.to_string()})
                    });
                    json!({"event":event,"returnCode":null})
                }
                Some("ack") => {
                    self.acks
                        .lock()
                        .unwrap()
                        .push(params["sequence"].as_u64().unwrap());
                    self.order.lock().unwrap().push("ack");
                    json!({"acknowledged":params["sequence"]})
                }
                _ => json!({"ok":true}),
            })
        }
    }

    // The guest deletes every event up to an ACK. A server request that the
    // action acknowledged was lost: the agent's provider session could no
    // longer read, answer or settle it.
    #[tokio::test]
    async fn server_requests_stay_in_the_guest_journal_for_the_session_in_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let guest = Arc::new(ScriptedGuest {
            events: vec![
                json!({"method":"account/updated","params":{}}),
                json!({"id":"approval-1","method":"item/commandExecution/requestApproval",
                    "params":{"threadId":"guest-thread","itemId":"item-1"}}),
                json!({"method":"item/started","params":{}}),
                // A provider request ID can equal the remote ID of this write.
                json!({"id":7,"method":"item/tool/call","params":{"threadId":"guest-thread"}}),
                json!({"id":7,"result":{"ok":true}}),
            ],
            acks: Mutex::new(Vec::new()),
            order: order.clone(),
        });
        let (sink, deliveries) = notification_sink(order.clone());
        let provider = VmNativeProvider::new(guest.clone(), sink, "acct", "vm-agent", 4);
        let receipt = provider
            .provider_request(ProviderRequest {
                account: "acct".into(),
                operation: "op-server-requests".into(),
                actor: None,
                expected_scope: provider.current_scope("acct"),
                deleted_actor_session: None,
                method: Method::ThreadCompactStart,
                params: json!({"threadId":"guest-thread"}),
                deadline: Instant::now() + std::time::Duration::from_secs(3),
            })
            .await
            .unwrap();
        assert_eq!(receipt.certainty, RequestCertainty::Written);
        assert_eq!(
            receipt.result,
            Some(ProviderResponse::Reply(json!({"ok":true})))
        );
        // Only the notification before the first server request is delivered
        // and acknowledged. The approval and every later event stay
        // unacknowledged, so the session reads them once, in journal order.
        let deliveries = deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].3, 1);
        assert_eq!(*guest.acks.lock().unwrap(), [1]);
        assert_eq!(*order.lock().unwrap(), ["notification", "ack"]);
    }
}
