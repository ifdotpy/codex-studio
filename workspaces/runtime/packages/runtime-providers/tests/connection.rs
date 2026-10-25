use serde_json::{Value, json};
use studio_runtime_providers::{
    Action, Frame, InitializationStatus, Lifecycle, MAX_FRAME_BYTES, Method, ProviderKind,
    RequestFrame, RequestId, RequestOutcome, RequestTicket, ResponseBody, SessionState,
    encode_line, validate_initialize_params,
};

fn state(claude: bool) -> SessionState {
    let (mut state, actions) = SessionState::new(
        9,
        if claude {
            ProviderKind::Claude
        } else {
            ProviderKind::Codex
        }
        .wire(),
        0,
        100,
        studio_runtime_providers::initialize_params(
            studio_runtime_providers::ClientName::AccountSession,
        ),
    )
    .expect("scripted test operation");
    assert!(matches!(actions.as_slice(), [Action::Write(_)]));
    state
        .receive(Frame::Response {
            id: if claude {
                RequestId::String("claude:00000000000000000000000000000001".into())
            } else {
                RequestId::Number(1)
            },
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert_eq!(state.lifecycle(), Lifecycle::Ready);
    state
}

fn outgoing(actions: &[Action]) -> Vec<Value> {
    actions
        .iter()
        .filter_map(|action| match action {
            Action::Write(frame) => {
                serde_json::from_slice(&encode_line(frame).expect("valid write")).ok()
            }
            _ => None,
        })
        .collect()
}

#[test]
fn one_off_catalog_and_terminal_requests_are_correlated_and_resolved() {
    let shapes = [
        (
            Method::SkillsList,
            json!({"cwds":["/repo"],"forceReload":false}),
        ),
        (
            Method::McpServerStatusList,
            json!({"threadId":"thread","limit":100}),
        ),
        (
            Method::ThreadBackgroundTerminalsList,
            json!({"threadId":"thread","limit":100}),
        ),
        (
            Method::ThreadBackgroundTerminalsTerminate,
            json!({"threadId":"thread","processId":"42"}),
        ),
    ];
    for (method, params) in shapes {
        let mut session = state(false);
        let (ticket, actions) = session
            .submit(method, params.clone(), 1, 10, None, true)
            .expect("one-off request is admitted");
        let request = &outgoing(&actions)[0];
        assert_eq!(request["method"], method.as_str());
        assert_eq!(request["id"], 2);
        assert_eq!(request["params"], params);
        let settled = session
            .receive(Frame::Response {
                id: ticket.id,
                body: ResponseBody::Result(json!({"ok":true})),
                fields: Default::default(),
            })
            .expect("provider reply is correlated");
        assert!(matches!(
            &settled[0],
            Action::Resolve {
                outcome: RequestOutcome::Reply { late: false, .. },
                ..
            }
        ));
    }
}

#[test]
fn recovered_requests_keep_a_finite_configured_deadline() {
    let recovered_id = RequestId::Number(88);
    let (mut state, _) = SessionState::new_with_recovered(
        9,
        ProviderKind::Codex.wire(),
        10,
        5,
        studio_runtime_providers::initialize_params(
            studio_runtime_providers::ClientName::AccountSession,
        ),
        vec![(recovered_id.clone(), Method::ModelList, None)],
    )
    .expect("recovered request should register");
    assert!(state.expire(14).1.is_empty());
    assert_eq!(state.next_deadline_ms(), Some(15));
    state
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("initialize response");
    assert_eq!(state.next_deadline_ms(), Some(15));
    let (_, actions) = state.expire(15);
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Resolve {
            ticket: RequestTicket { id, .. },
            outcome: RequestOutcome::Unknown,
        } if id == &recovered_id
    )));
}

#[test]
fn initialization_validation_is_a_pure_refusal_before_session_creation() {
    assert!(validate_initialize_params(&json!({"capabilities":{}})).is_ok());
    let invalid = json!({"payload":"x".repeat(MAX_FRAME_BYTES)});
    assert!(matches!(
        validate_initialize_params(&invalid),
        Err(studio_runtime_providers::CodecError::FrameTooLarge { .. })
    ));
    assert!(matches!(
        SessionState::new(1, ProviderKind::Codex.wire(), 0, 10, invalid),
        Err(studio_runtime_providers::SessionError::Codec(
            studio_runtime_providers::CodecError::FrameTooLarge { .. }
        ))
    ));
}

#[test]
fn readiness_waits_for_the_initialized_write_receipt() {
    let (mut session, _) = SessionState::new(77, ProviderKind::Codex.wire(), 0, 10, json!({}))
        .expect("session creation");
    assert_eq!(
        session.initialization_status(),
        InitializationStatus::AwaitingReply
    );
    assert!(matches!(
        session.mark_initialized_notification_written(),
        Err(studio_runtime_providers::SessionError::InitializationNotReady)
    ));

    let actions = session
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("initialize response");
    assert!(actions.iter().any(|action| matches!(
        action,
        Action::Write(Frame::Notification(notification)) if notification.method == Method::Initialized
    )));
    assert_eq!(session.lifecycle(), Lifecycle::Ready);
    assert_eq!(
        session.initialization_status(),
        InitializationStatus::InitializedWritePending
    );
    session
        .mark_initialized_notification_written()
        .expect("write receipt completes initialization");
    assert_eq!(session.initialization_status(), InitializationStatus::Ready);
    session
        .mark_initialized_notification_written()
        .expect("duplicate completion receipt is idempotent");
}

#[test]
fn initialization_refusal_and_unanswered_transport_have_distinct_readiness() {
    let (mut refused, _) = SessionState::new(78, ProviderKind::Codex.wire(), 0, 10, json!({}))
        .expect("session creation");
    refused
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Error(json!({"code":-1})),
            fields: Default::default(),
        })
        .expect("initialize refusal");
    assert_eq!(
        refused.initialization_status(),
        InitializationStatus::Refused
    );

    let (mut lost, _) = SessionState::new(79, ProviderKind::Codex.wire(), 0, 10, json!({}))
        .expect("session creation");
    lost.transport_lost();
    assert_eq!(lost.initialization_status(), InitializationStatus::Unknown);

    let (mut write_lost, _) = SessionState::new(80, ProviderKind::Codex.wire(), 0, 10, json!({}))
        .expect("session creation");
    write_lost
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("initialize response");
    write_lost.transport_lost();
    assert_eq!(
        write_lost.initialization_status(),
        InitializationStatus::InitializedWritePending
    );
    write_lost
        .mark_initialized_notification_written()
        .expect("a late positive write receipt completes initialization");
    assert_eq!(
        write_lost.initialization_status(),
        InitializationStatus::Ready
    );
}

#[test]
fn injected_deadline_marks_only_one_request_unknown_and_late_reply_is_positive_evidence() {
    let mut session = state(false);
    let (first, _) = session
        .submit(Method::TurnStart, json!({}), 0, 5, None, false)
        .expect("scripted test operation");
    let (second, _) = session
        .submit(Method::AccountRead, json!({}), 0, 50, None, false)
        .expect("scripted test operation");
    let (result, actions) = session.expire(5);
    assert_eq!(
        result.expect_err("expected scripted failure"),
        studio_runtime_providers::SessionError::DeadlineUnknown
    );
    assert_eq!(
        actions
            .iter()
            .filter(|action| matches!(
                action,
                Action::Resolve {
                    outcome: RequestOutcome::Unknown,
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(session.lifecycle(), Lifecycle::Ready);
    let late = session
        .receive(Frame::Response {
            id: first.id,
            body: ResponseBody::Result(json!({"turn":{}})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert!(matches!(
        &late[0],
        Action::Resolve {
            outcome: RequestOutcome::Reply { late: true, .. },
            ..
        }
    ));
    assert_eq!(
        session
            .receive(Frame::Response {
                id: second.id,
                body: ResponseBody::Result(json!({})),
                fields: Default::default()
            })
            .expect("scripted test operation")
            .len(),
        1
    );
}

#[test]
fn claude_serializes_selected_methods_per_thread_and_allows_other_threads() {
    let mut session = state(true);
    let (start, start_actions) = session
        .submit(
            Method::TurnStart,
            json!({"threadId":"a"}),
            0,
            50,
            Some("a".into()),
            false,
        )
        .expect("scripted test operation");
    assert_eq!(outgoing(&start_actions).len(), 1);
    let (steer, steer_actions) = session
        .submit(
            Method::TurnSteer,
            json!({"threadId":"a"}),
            0,
            50,
            Some("a".into()),
            false,
        )
        .expect("scripted test operation");
    assert!(steer_actions.is_empty());
    let (_, other_thread) = session
        .submit(
            Method::ThreadResume,
            json!({"threadId":"b"}),
            0,
            50,
            Some("b".into()),
            false,
        )
        .expect("scripted test operation");
    assert_eq!(outgoing(&other_thread).len(), 1);
    let (_, lightweight) = session
        .submit(
            Method::ThreadRead,
            json!({"threadId":"a"}),
            0,
            50,
            Some("a".into()),
            true,
        )
        .expect("scripted test operation");
    assert_eq!(outgoing(&lightweight).len(), 1);
    let released = session
        .receive(Frame::Response {
            id: start.id,
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert!(
        released.iter().any(
            |action| matches!(action, Action::ThreadReleased { ticket, .. } if ticket == &steer)
        )
    );
    assert_eq!(outgoing(&released).len(), 1);
}

#[test]
fn claude_cancel_removes_pending_and_orphan_and_duplicate_responses_are_ignored() {
    let mut session = state(true);
    let (active, _) = session
        .submit(
            Method::ThreadRead,
            json!({"threadId":"t","includeTurns":true}),
            0,
            20,
            Some("t".into()),
            false,
        )
        .expect("scripted test operation");
    let (queued, _) = session
        .submit(
            Method::TurnStart,
            json!({"threadId":"t"}),
            0,
            20,
            Some("t".into()),
            false,
        )
        .expect("scripted test operation");
    let release = session.cancel(&active).expect("scripted test operation");
    assert!(
        outgoing(&release).iter().any(|frame| frame["id"]
            == serde_json::to_value(&queued.id).expect("scripted test operation"))
    );
    let orphan = Frame::Response {
        id: active.id,
        body: ResponseBody::Result(json!({})),
        fields: Default::default(),
    };
    assert!(
        session
            .receive(orphan.clone())
            .expect("scripted test operation")
            .is_empty()
    );
    let settled = session
        .receive(Frame::Response {
            id: queued.id.clone(),
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert_eq!(settled.len(), 1);
    assert!(
        session
            .receive(Frame::Response {
                id: queued.id,
                body: ResponseBody::Result(json!({})),
                fields: Default::default()
            })
            .expect("scripted test operation")
            .is_empty()
    );
}

#[test]
fn old_generation_tickets_cannot_cancel_a_new_generation() {
    let mut old = state(true);
    let (ticket, _) = old
        .submit(Method::ThreadRead, json!({}), 0, 5, Some("t".into()), false)
        .expect("scripted test operation");
    let mut new = state(true);
    let foreign = RequestTicket {
        generation: 10,
        ..ticket
    };
    assert!(matches!(
        new.cancel(&foreign),
        Err(studio_runtime_providers::SessionError::StaleGeneration)
    ));
}

#[test]
fn orderly_close_makes_pending_unknown_and_drops_later_replies() {
    let mut session = state(false);
    let (ticket, _) = session
        .submit(Method::AccountRead, json!({}), 0, 50, None, false)
        .expect("scripted test operation");
    let closed = session.close();
    assert!(closed.iter().any(|action| matches!(
        action,
        Action::Resolve {
            outcome: RequestOutcome::Unknown,
            ..
        }
    )));
    assert!(
        session
            .receive(Frame::Response {
                id: ticket.id,
                body: ResponseBody::Result(json!({})),
                fields: Default::default()
            })
            .expect("scripted test operation")
            .is_empty()
    );
    let orphan = Frame::Response {
        id: RequestId::Number(987),
        body: ResponseBody::Result(json!({})),
        fields: Default::default(),
    };
    assert!(
        session
            .receive(orphan)
            .expect("scripted test operation")
            .is_empty()
    );
}

#[test]
fn initialization_error_moves_session_to_lost_without_initialized_notification() {
    let (mut session, _) = SessionState::new(31, ProviderKind::Codex.wire(), 0, 20, json!({}))
        .expect("scripted test operation");
    let actions = session
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Error(json!({"code":-1,"message":"no init"})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert_eq!(session.lifecycle(), Lifecycle::Lost);
    assert!(
        !actions
            .iter()
            .any(|action| matches!(action, Action::Write(_)))
    );
    assert!(matches!(
        session.submit(Method::ModelList, json!({}), 1, 20, None, false),
        Err(studio_runtime_providers::SessionError::NotReady)
    ));
}

#[test]
fn missing_claude_thread_id_is_rejected_without_claiming_a_serial_lane() {
    let mut session = state(true);
    assert!(
        session
            .submit(Method::TurnStart, json!({}), 1, 20, None, false)
            .is_err()
    );
    let (ticket, actions) = session
        .submit(
            Method::TurnStart,
            json!({"threadId":"t"}),
            1,
            20,
            Some("t".into()),
            false,
        )
        .expect("scripted test operation");
    assert_eq!(
        ticket.id,
        RequestId::String("claude:00000000000000000000000000000002".into())
    );
    assert_eq!(outgoing(&actions).len(), 1);
}

#[test]
fn frame_encoding_failure_does_not_register_or_claim_the_thread_lane() {
    let mut session = state(true);
    let too_large = json!({"blob":"x".repeat(studio_runtime_providers::MAX_FRAME_BYTES)});
    assert!(matches!(
        session.submit(Method::TurnStart, too_large, 1, 20, Some("t".into()), false,),
        Err(studio_runtime_providers::SessionError::Codec(
            studio_runtime_providers::CodecError::FrameTooLarge { .. }
        ))
    ));
    let (ticket, actions) = session
        .submit(
            Method::TurnStart,
            json!({"threadId":"t"}),
            1,
            20,
            Some("t".into()),
            false,
        )
        .expect("scripted test operation");
    assert_eq!(
        ticket.id,
        RequestId::String("claude:00000000000000000000000000000002".into())
    );
    assert_eq!(outgoing(&actions).len(), 1);
    assert_eq!(session.lifecycle(), Lifecycle::Ready);
}

#[test]
fn every_lifecycle_transition_and_pending_disposition_is_exercised() {
    let (mut initializing, _) = SessionState::new(41, ProviderKind::Codex.wire(), 0, 20, json!({}))
        .expect("scripted test operation");
    assert_eq!(initializing.lifecycle(), Lifecycle::Initializing);
    let ready = initializing
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert!(
        ready
            .iter()
            .any(|action| matches!(action, Action::Write(_)))
    );
    assert_eq!(initializing.lifecycle(), Lifecycle::Ready);

    let (lost_ticket, _) = initializing
        .submit(Method::ModelList, json!({}), 0, 20, None, false)
        .expect("scripted test operation");
    let lost = initializing.transport_lost();
    assert!(lost.iter().any(|action| matches!(
        action,
        Action::Resolve {
            outcome: RequestOutcome::Unknown,
            ..
        }
    )));
    assert_eq!(initializing.lifecycle(), Lifecycle::Lost);
    let closed = initializing.close();
    assert_eq!(initializing.lifecycle(), Lifecycle::Closed);
    assert!(
        closed.is_empty(),
        "already-unknown waiters are not resolved twice"
    );
    assert!(
        initializing
            .receive(Frame::Notification(
                studio_runtime_providers::NotificationFrame {
                    method: Method::TurnStarted,
                    params: None,
                    fields: Default::default(),
                }
            ))
            .expect("scripted test operation")
            .is_empty()
    );
    assert!(
        initializing
            .receive(Frame::Response {
                id: lost_ticket.id,
                body: ResponseBody::Result(json!({})),
                fields: Default::default(),
            })
            .expect("scripted test operation")
            .is_empty()
    );

    let (mut failed_init, _) = SessionState::new(42, ProviderKind::Codex.wire(), 0, 20, json!({}))
        .expect("scripted test operation");
    failed_init
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Error(json!({"code":-1})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert_eq!(failed_init.lifecycle(), Lifecycle::Lost);

    let (mut closed_init, _) = SessionState::new(43, ProviderKind::Codex.wire(), 0, 20, json!({}))
        .expect("scripted test operation");
    closed_init.close();
    assert_eq!(closed_init.lifecycle(), Lifecycle::Closed);

    let (mut ready_close, _) = SessionState::new(44, ProviderKind::Codex.wire(), 0, 20, json!({}))
        .expect("scripted test operation");
    ready_close
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    ready_close
        .submit(Method::ModelList, json!({}), 1, 20, None, false)
        .expect("scripted test operation");
    let close_actions = ready_close.close();
    assert_eq!(ready_close.lifecycle(), Lifecycle::Closed);
    assert!(close_actions.iter().any(|action| matches!(
        action,
        Action::Resolve {
            outcome: RequestOutcome::Unknown,
            ..
        }
    )));
}

#[test]
fn numeric_request_exactly_at_frame_limit_uses_its_actual_id_for_validation() {
    let params = json!({"payload":""});
    let empty = encode_line(&Frame::Request(RequestFrame {
        id: RequestId::Number(2),
        method: Method::ModelList,
        params: Some(params.clone()),
        fields: Default::default(),
    }))
    .expect("empty frame");
    let payload_len = MAX_FRAME_BYTES + 1 - empty.len();
    let params = json!({"payload":"x".repeat(payload_len)});
    let exact = encode_line(&Frame::Request(RequestFrame {
        id: RequestId::Number(2),
        method: Method::ModelList,
        params: Some(params.clone()),
        fields: Default::default(),
    }))
    .expect("actual id frame fits exactly at limit");
    assert_eq!(exact.len(), MAX_FRAME_BYTES + 1);

    let mut session = state(false);
    let (ticket, actions) = session
        .submit(Method::ModelList, params, 1, 10, None, false)
        .expect("valid maximum-size request");
    assert_eq!(ticket.id, RequestId::Number(2));
    let written = match &actions[0] {
        Action::Write(frame) => encode_line(frame).expect("valid frame"),
        other => panic!("expected request write, got {other:?}"),
    };
    assert_eq!(written.len(), exact.len());
}

#[test]
fn every_claude_serialized_method_waits_behind_same_thread_operation() {
    for method in [
        Method::TurnSteer,
        Method::ThreadResume,
        Method::ThreadFork,
        Method::ThreadRead,
        Method::ThreadRollback,
        Method::ClaudeSettings,
        Method::ClaudeState,
        Method::ClaudeFileBackups,
        Method::ThreadCompactStart,
    ] {
        let mut session = state(true);
        let (_, start) = session
            .submit(
                Method::TurnStart,
                json!({}),
                0,
                50,
                Some("same".into()),
                false,
            )
            .expect("scripted test operation");
        assert_eq!(outgoing(&start).len(), 1);
        let (_, queued) = session
            .submit(
                method,
                json!({"includeTurns":true}),
                0,
                50,
                Some("same".into()),
                false,
            )
            .expect("scripted test operation");
        assert!(queued.is_empty(), "{method:?} queues behind turn/start");
    }
}

#[test]
fn provider_frames_are_passed_to_dispatch_in_order_without_method_filtering() {
    let mut session = state(false);
    let messages = [
        json!({"method":"turn/started","params":{"threadId":"t"}}),
        json!({"id":71,"method":"future/request","params":{"raw":[1]},"extra":true}),
        json!({"method":"future/notice","params":{"opaque":true}}),
    ];
    for value in messages {
        let line = serde_json::to_vec(&value).expect("scripted test operation");
        let studio_runtime_providers::DecodedJournalPayload::Frame(frame) =
            studio_runtime_providers::decode_journal_entry(1, 1, line).payload
        else {
            panic!("fixture frame must decode");
        };
        assert_eq!(
            session
                .receive(frame.clone())
                .expect("scripted test operation"),
            vec![Action::Dispatch(frame)]
        );
    }
}

#[test]
fn all_recorded_replay_fixture_frames_parse_through_the_session() {
    let root = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/provider-replay"
    );
    for entry in std::fs::read_dir(root).expect("fixtures directory") {
        let path = entry.expect("fixture entry").path();
        if !path.extension().is_some_and(|ext| ext == "json") {
            continue;
        }
        let value: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("fixture")).expect("JSON");
        // RPC replies have a dedicated session witness below.
        if path.file_name().expect("name") == "codex-rpc-replies.json" {
            continue;
        }
        let mut session = state(value["provider"] == "claude");
        let mut expected = Vec::new();
        for key in ["events", "childEvents", "parentEvents"] {
            if let Some(events) = value[key].as_array() {
                expected.extend(events.iter().cloned());
            }
        }
        assert!(
            !expected.is_empty(),
            "{}: event fixture is empty",
            path.display()
        );
        for expected_frame in expected {
            let mut bytes = serde_json::to_vec(&expected_frame).expect("scripted test operation");
            bytes.push(b'\n');
            let studio_runtime_providers::DecodedJournalPayload::Frame(frame) =
                studio_runtime_providers::decode_journal_entry(1, 1, bytes).payload
            else {
                panic!("fixture frame must decode");
            };
            assert_eq!(
                serde_json::from_slice::<Value>(
                    &studio_runtime_providers::encode_line(&frame).expect("frame encodes")
                )
                .expect("encoded frame is JSON"),
                expected_frame,
                "{}: wire round trip preserves the recorded frame",
                path.display()
            );
            let actions = session
                .receive(frame.clone())
                .expect("scripted test operation");
            assert!(
                matches!(actions.as_slice(), [Action::Dispatch(delivered)] if delivered == &frame)
            );
        }
    }
}

#[test]
fn recorded_rpc_reply_templates_match_session_request_ids_and_methods() {
    let fixture: Value = serde_json::from_str(include_str!(
        "fixtures/provider-replay/codex-rpc-replies.json"
    ))
    .expect("scripted test operation");
    let mut session = state(false);
    let replies = fixture.as_object().expect("reply fixture object");
    let mut expected_id = 2;
    for (method_name, reply) in replies {
        if method_name == "initialize" {
            continue;
        }
        let method = Method::from_wire(method_name).expect("fixture method");
        let (ticket, actions) = session
            .submit(method, json!({}), 1, 100, None, false)
            .expect("scripted test operation");
        assert_eq!(ticket.id, RequestId::Number(expected_id));
        let request = outgoing(&actions).remove(0);
        assert_eq!(request["method"], reply["responseFor"]);
        assert_eq!(request["id"], expected_id);
        expected_id += 1;

        let mut response = reply["message"].clone();
        response["id"] = json!(ticket.id);
        let line = serde_json::to_vec(&response).expect("scripted test operation");
        let studio_runtime_providers::DecodedJournalPayload::Frame(frame) =
            studio_runtime_providers::decode_journal_entry(1, 1, line).payload
        else {
            panic!("reply must decode");
        };
        assert!(session
            .receive(frame)
            .expect("scripted test operation")
            .iter()
            .any(|action| matches!(action, Action::Resolve { ticket: settled, .. } if settled == &ticket)));
    }
}

#[test]
fn missing_and_orphan_reply_assertions_are_exercised_by_session_state() {
    for (fixture, preceding, missing) in [
        (
            include_str!("fixtures/provider-replay/lost-turn-start-reply.json"),
            &[Method::ThreadStart, Method::ThreadRead][..],
            Method::TurnStart,
        ),
        (
            include_str!("fixtures/provider-replay/restart-mid-turn-supervisor-replay.json"),
            &[Method::ThreadStart][..],
            Method::TurnStart,
        ),
    ] {
        let transcript: Value = serde_json::from_str(fixture).expect("scripted test operation");
        let expected = &transcript["expectedMissingReplies"][0];
        let mut session = state(false);
        for method in preceding {
            session
                .submit(*method, json!({}), 1, 100, None, false)
                .expect("scripted test operation");
        }
        let (ticket, actions) = session
            .submit(missing, json!({}), 1, 100, None, false)
            .expect("scripted test operation");
        assert_eq!(
            ticket.id,
            serde_json::from_value(expected["id"].clone()).expect("fixture id")
        );
        assert_eq!(expected["method"], missing.as_str());
        assert_eq!(outgoing(&actions)[0]["id"], expected["id"]);
        let unknown = session.transport_lost();
        assert!(unknown.iter().any(|action| matches!(action,
            Action::Resolve { ticket: resolved, outcome: RequestOutcome::Unknown }
                if resolved == &ticket
        )));
        if let Some(orphan) = transcript["allowedOrphanResponses"]
            .as_array()
            .and_then(|a| a.first())
        {
            let orphan_frame = Frame::Response {
                id: serde_json::from_value(orphan["id"].clone()).expect("fixture orphan id"),
                body: ResponseBody::Result(orphan["result"].clone()),
                fields: Default::default(),
            };
            assert!(
                session
                    .receive(orphan_frame)
                    .expect("scripted test operation")
                    .is_empty()
            );
        }
    }
}

#[test]
fn cancelled_book_ids_are_removed_but_never_reused() {
    let mut book = studio_runtime_providers::PendingBook::new();
    let id = RequestId::String("claude:fixture".into());
    book.register(id.clone(), Method::ThreadRead)
        .expect("scripted test operation");
    assert_eq!(
        book.cancel(&id).expect("scripted test operation"),
        Method::ThreadRead
    );
    assert_eq!(book.state(&id), None);
    assert_eq!(
        book.observe_response(&id),
        Err(studio_runtime_providers::BookError::UnknownId)
    );
    assert_eq!(
        book.register(id, Method::ThreadRead),
        Err(studio_runtime_providers::BookError::DuplicateId)
    );
}

#[test]
fn arrival_timestamp_classification_is_independent_of_expiry_order() {
    for received_at_ms in [10, 11, 12] {
        let mut replies = Vec::new();
        for expire_first in [false, true] {
            let mut session = state(false);
            let (ticket, _) = session
                .submit(Method::ModelList, json!({}), 1, 10, None, false)
                .expect("request registration");
            if expire_first {
                assert_eq!(session.expire(20).1.len(), 1);
                assert_eq!(session.next_deadline_ms(), None);
                assert!(session.expire(21).1.is_empty());
            }
            let actions = session
                .receive_at(
                    Frame::Response {
                        id: ticket.id,
                        body: ResponseBody::Result(json!({})),
                        fields: Default::default(),
                    },
                    received_at_ms,
                )
                .expect("timestamped response");
            assert!(matches!(&actions[0], Action::Resolve {
                outcome: RequestOutcome::Reply { late, .. }, ..
            } if *late == (received_at_ms >= 11)));
            replies.push(actions);
            assert!(session.expire(30).1.is_empty());
        }
        assert_eq!(replies[0], replies[1]);
    }
}

#[test]
fn rejecting_unsent_retires_expired_queue_and_releases_next_serialized_request() {
    for expired in [false, true] {
        let mut session = state(true);
        let (first, _) = session
            .submit(
                Method::TurnStart,
                json!({}),
                0,
                100,
                Some("thread".into()),
                false,
            )
            .expect("first");
        let (second, actions) = session
            .submit(
                Method::TurnStart,
                json!({}),
                0,
                10,
                Some("thread".into()),
                false,
            )
            .expect("queued second");
        assert!(actions.is_empty());
        let (third, actions) = session
            .submit(
                Method::TurnStart,
                json!({}),
                0,
                100,
                Some("thread".into()),
                false,
            )
            .expect("queued third");
        assert!(actions.is_empty());
        if expired {
            assert_eq!(session.expire(10).1.len(), 1);
        }
        assert!(
            session
                .reject_unsent(&second)
                .expect("retire second")
                .is_empty()
        );
        let released = session.reject_unsent(&first).expect("retire first");
        let writes = outgoing(&released);
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0]["id"],
            serde_json::to_value(&third.id).expect("id")
        );
        assert!(
            session
                .receive(reply_for_test(second.id))
                .expect("orphan response")
                .is_empty()
        );
        assert_eq!(session.next_deadline_ms(), Some(100));
    }
}

fn reply_for_test(id: RequestId) -> Frame {
    Frame::Response {
        id,
        body: ResponseBody::Result(json!({})),
        fields: Default::default(),
    }
}

#[test]
fn rejecting_unsent_validates_generation_and_supports_codex_unknown_tickets() {
    let mut session = state(false);
    let (ticket, _) = session
        .submit(Method::ModelList, json!({}), 0, 10, None, false)
        .expect("ticket");
    let mut stale = ticket.clone();
    stale.generation += 1;
    assert!(session.reject_unsent(&stale).is_err());
    let _ignored = session.expire(10);
    assert!(
        session
            .reject_unsent(&ticket)
            .expect("retire expired unsent")
            .is_empty()
    );
    assert_eq!(session.next_deadline_ms(), None);
    assert!(
        session
            .receive(reply_for_test(ticket.id.clone()))
            .expect("orphan")
            .is_empty()
    );
    let (retried, _) = session
        .submit_with_id(
            ticket.id.clone(),
            ticket.method,
            json!({}),
            10,
            10,
            None,
            false,
        )
        .expect("proven-unsent identity can be retried");
    assert_eq!(retried, ticket);
}

#[test]
fn arrival_timestamp_still_uses_original_deadline_after_transport_loss() {
    for arrived in [9, 10, 11] {
        let mut session = state(false);
        let (ticket, _) = session
            .submit(Method::ModelList, json!({}), 0, 10, None, false)
            .expect("ticket");
        session.transport_lost();
        let actions = session
            .receive_at(reply_for_test(ticket.id), arrived)
            .expect("late evidence");
        assert!(
            matches!(&actions[0], Action::Resolve { outcome: RequestOutcome::Reply { late, .. }, .. } if *late == (arrived >= 10))
        );
    }
}

#[test]
fn restored_request_preserves_original_deadline_and_prior_unknown() {
    for unknown in [false, true] {
        let mut session = state(false);
        let ticket = RequestTicket {
            generation: 9,
            id: RequestId::Number(80),
            method: Method::ModelList,
        };
        session
            .restore_request(&ticket, None, 5, unknown, false)
            .expect("valid recovery");
        assert_eq!(session.request_deadline_ms(&ticket.id), Some(5));
        assert_eq!(
            session.next_deadline_ms(),
            if unknown { None } else { Some(5) }
        );
        let (_, expired) = session.expire(20);
        assert_eq!(expired.len(), usize::from(!unknown));
        let actions = session
            .receive_at(
                Frame::Response {
                    id: ticket.id,
                    body: ResponseBody::Result(json!({})),
                    fields: Default::default(),
                },
                4,
            )
            .expect("late delivery, early arrival");
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::Resolve {
                outcome: RequestOutcome::Reply { late: false, .. },
                ..
            }
        )));
    }
}

#[test]
fn restored_serialized_and_lightweight_requests_preserve_thread_ownership() {
    for lightweight in [false, true] {
        let mut session = state(true);
        let restored = RequestTicket {
            generation: 9,
            id: RequestId::Number(80),
            method: Method::TurnStart,
        };
        session
            .restore_request(&restored, Some("thread".into()), 100, true, lightweight)
            .expect("restore");
        let (_, actions) = session
            .submit_with_id(
                RequestId::Number(81),
                Method::TurnStart,
                json!({"threadId":"thread","input":[]}),
                1,
                100,
                Some("thread".into()),
                false,
            )
            .expect("submit successor");
        assert_eq!(outgoing(&actions).len(), usize::from(lightweight));
        let actions = session
            .receive_at(
                Frame::Response {
                    id: restored.id,
                    body: ResponseBody::Result(json!({})),
                    fields: Default::default(),
                },
                2,
            )
            .expect("resolve restored");
        assert_eq!(outgoing(&actions).len(), usize::from(!lightweight));
    }
}

#[test]
fn deadline_index_expires_only_due_unanswered_requests_for_both_providers() {
    for claude in [false, true] {
        let mut session = state(claude);
        let tickets = [10, 20, 30, 30].map(|timeout| {
            session
                .submit(Method::ModelList, json!({}), 0, timeout, None, false)
                .expect("submit")
                .0
        });
        assert_eq!(session.next_deadline_ms(), Some(10));
        let replied = session
            .receive_at(
                Frame::Response {
                    id: tickets[0].id.clone(),
                    body: ResponseBody::Result(json!({})),
                    fields: Default::default(),
                },
                5,
            )
            .expect("reply");
        assert!(matches!(
            replied.as_slice(),
            [Action::Resolve {
                outcome: RequestOutcome::Reply { late: false, .. },
                ..
            }]
        ));
        assert_eq!(session.next_deadline_ms(), Some(20));
        let (result, nothing_due) = session.expire(19);
        assert!(result.is_ok() && nothing_due.is_empty());
        let (result, expired) = session.expire(30);
        assert!(result.is_err());
        let expired = expired
            .iter()
            .map(|action| match action {
                Action::Resolve {
                    ticket,
                    outcome: RequestOutcome::Unknown,
                } => ticket.id.clone(),
                other => panic!("unexpected action {other:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(expired.len(), 3);
        for ticket in &tickets[1..] {
            assert!(expired.contains(&ticket.id));
        }
        assert_eq!(session.next_deadline_ms(), None);
        assert!(session.expire(u64::MAX - 1).1.is_empty());
        let fresh = session
            .submit(Method::ModelList, json!({}), 40, 10, None, false)
            .expect("submit after expiry")
            .0;
        assert_eq!(session.next_deadline_ms(), Some(50));
        session.transport_lost();
        assert_eq!(session.next_deadline_ms(), None);
        assert!(session.expire(60).1.is_empty(), "{fresh:?}");
    }
}

#[test]
fn claude_thread_read_serializes_only_when_include_turns_is_true() {
    for include in [
        None,
        Some(json!(false)),
        Some(json!(true)),
        Some(json!("true")),
    ] {
        let mut session = state(true);
        session
            .submit(Method::TurnStart, json!({}), 0, 50, Some("t".into()), false)
            .expect("scripted test operation");
        let mut params = json!({"threadId":"t"});
        if let Some(value) = include.clone() {
            params["includeTurns"] = value;
        }
        session
            .preflight_with_id(
                RequestId::String("read".into()),
                Method::ThreadRead,
                &params,
                Some("t"),
                false,
            )
            .expect("scripted test operation");
        let (_, actions) = session
            .submit(Method::ThreadRead, params, 0, 50, Some("t".into()), false)
            .expect("scripted test operation");
        assert_eq!(
            outgoing(&actions).len(),
            usize::from(include != Some(json!(true)))
        );
    }
}

#[test]
fn malformed_reply_does_not_consume_pending_request_before_real_reply() {
    let mut session = state(false);
    let (ticket, _) = session
        .submit(Method::ModelList, json!({}), 0, 50, None, false)
        .expect("scripted test operation");
    for value in [
        json!({"id":ticket.id}),
        json!({"id":ticket.id,"result":{},"error":{}}),
    ] {
        let entry = studio_runtime_providers::decode_journal_value(1, 1, &value);
        if let studio_runtime_providers::DecodedJournalPayload::Frame(frame) = entry.payload {
            session.receive(frame).expect("scripted test operation");
        }
    }
    let actions = session
        .receive(reply_for_test(ticket.id.clone()))
        .expect("scripted test operation");
    assert!(actions.iter().any(
        |action| matches!(action, Action::Resolve { ticket: resolved, .. } if resolved == &ticket)
    ));
}

#[test]
fn recovered_same_thread_requests_hold_fresh_work_until_every_owner_settles() {
    for order in [[0, 1], [1, 0]] {
        for settlement in ["reply", "cancel", "unsent"] {
            let mut session = state(true);
            let restored = [80, 81].map(|id| RequestTicket {
                generation: 9,
                id: RequestId::Number(id),
                method: Method::ClaudeSettings,
            });
            for ticket in &restored {
                session
                    .restore_request(ticket, Some("t".into()), 10, false, false)
                    .expect("all unresolved same-thread identities restore without writes");
            }
            let (fresh, actions) = session
                .submit(
                    Method::TurnStart,
                    json!({"threadId":"t"}),
                    0,
                    100,
                    Some("t".into()),
                    false,
                )
                .expect("fresh request queues");
            assert!(actions.is_empty());
            if settlement != "cancel" {
                let (_, expired) = session.expire(10);
                assert!(
                    outgoing(&expired).is_empty(),
                    "unknown expiry retains reservations"
                );
            }
            for (index, owner) in order.into_iter().enumerate() {
                let ticket = &restored[owner];
                let actions = match settlement {
                    "cancel" => session.cancel(ticket).expect("cancel one owner"),
                    "unsent" => session
                        .reject_unsent(ticket)
                        .expect("retire one proven-unsent owner"),
                    _ => session
                        .receive(reply_for_test(ticket.id.clone()))
                        .expect("late reply settles one owner"),
                };
                let writes = outgoing(&actions);
                if index == 0 {
                    assert!(
                        writes.is_empty(),
                        "the other restored owner still holds the thread"
                    );
                } else {
                    assert_eq!(writes.len(), 1);
                    assert_eq!(writes[0]["id"], json!(fresh.id));
                }
            }
        }
    }
}
