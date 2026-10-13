use serde_json::{Value, json};
use studio_runtime_providers::{
    Action, Frame, Lifecycle, MAX_FRAME_BYTES, Method, RequestFrame, RequestId, RequestOutcome,
    RequestTicket, ResponseBody, SessionState, encode_line,
};

const INIT: &str = r#"{"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}"#;

fn state(claude: bool) -> SessionState {
    let (mut state, actions) = SessionState::new(
        9,
        claude,
        0,
        100,
        serde_json::from_str(INIT).expect("scripted test operation"),
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
            Action::Write(bytes) => serde_json::from_slice(bytes).ok(),
            _ => None,
        })
        .collect()
}

#[test]
fn initialize_handshake_and_python_caller_request_shapes() {
    let shapes = [
        (Method::TurnStart, json!({"threadId":"thread","input":[]})),
        (
            Method::TurnInterrupt,
            json!({"threadId":"thread","turnId":"turn"}),
        ),
        (Method::AccountRead, json!({"passive":true})),
        (Method::ModelList, json!({"limit":100,"includeHidden":true})),
    ];
    for (method, params) in shapes {
        let mut session = state(false);
        let (ticket, actions) = session
            .submit(method, params.clone(), 1, 10, None, false)
            .expect("scripted test operation");
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
            .expect("scripted test operation");
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
    let resolved = session
        .receive(Frame::Response {
            id: second.id,
            body: ResponseBody::Result(json!({})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert!(matches!(
        resolved.as_slice(),
        [Action::Resolve {
            outcome: RequestOutcome::Reply { late: false, .. },
            ..
        }]
    ));
}

#[test]
fn caller_allocated_request_identity_survives_session_recreation_without_ticket_collision() {
    let mut first = state(false);
    let id = RequestId::String("turn:agent:attempt-1:generation-4".into());
    let (old_ticket, _) = first
        .submit_with_id(id.clone(), Method::TurnStart, json!({}), 0, 50, None, false)
        .expect("old ticket registers");
    let mut second = state(false);
    let (new_ticket, _) = second
        .submit_with_id(
            RequestId::String("turn:agent:attempt-2:generation-5".into()),
            Method::TurnStart,
            json!({}),
            0,
            50,
            None,
            false,
        )
        .expect("new ticket registers");
    let old_reply = second
        .receive(Frame::Response {
            id: old_ticket.id.clone(),
            body: ResponseBody::Result(json!({"turn":{"id":"old"}})),
            fields: Default::default(),
        })
        .expect("old response is ignored by the reattached session");
    assert!(old_reply.is_empty());
    assert_ne!(old_ticket, new_ticket);
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
            json!({"threadId":"t"}),
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
    let (mut session, _) =
        SessionState::new(31, false, 0, 20, json!({})).expect("scripted test operation");
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
    let (mut initializing, _) =
        SessionState::new(41, false, 0, 20, json!({})).expect("scripted test operation");
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

    let (mut failed_init, _) =
        SessionState::new(42, false, 0, 20, json!({})).expect("scripted test operation");
    failed_init
        .receive(Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Error(json!({"code":-1})),
            fields: Default::default(),
        })
        .expect("scripted test operation");
    assert_eq!(failed_init.lifecycle(), Lifecycle::Lost);

    let (mut closed_init, _) =
        SessionState::new(43, false, 0, 20, json!({})).expect("scripted test operation");
    closed_init.close();
    assert_eq!(closed_init.lifecycle(), Lifecycle::Closed);

    let (mut ready_close, _) =
        SessionState::new(44, false, 0, 20, json!({})).expect("scripted test operation");
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
        Action::Write(bytes) => bytes,
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
            .submit(method, json!({}), 0, 50, Some("same".into()), false)
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
        let frame = studio_runtime_providers::LineDecoder::new()
            .push(&[line, vec![b'\n']].concat())
            .expect("scripted test operation")
            .remove(0);
        assert!(matches!(
            session
                .receive(frame)
                .expect("scripted test operation")
                .as_slice(),
            [Action::Dispatch(_)]
        ));
    }
}

#[test]
fn all_recorded_replay_fixture_frames_parse_through_the_session() {
    const FIXTURES: &[&str] = &[
        include_str!(
            "../../../apps/server/tests/fixtures/provider-replay/background-output-after-completion.json"
        ),
        include_str!("../../../apps/server/tests/fixtures/provider-replay/codex-rpc-replies.json"),
        include_str!(
            "../../../apps/server/tests/fixtures/provider-replay/child-completion-before-parent.json"
        ),
        include_str!("../../../apps/server/tests/fixtures/provider-replay/claude-first-turn.json"),
        include_str!(
            "../../../apps/server/tests/fixtures/provider-replay/lost-turn-start-reply.json"
        ),
        include_str!(
            "../../../apps/server/tests/fixtures/provider-replay/restart-mid-turn-supervisor-replay.json"
        ),
        include_str!(
            "../../../apps/server/tests/fixtures/provider-replay/token-usage-reasoning.json"
        ),
        include_str!("../../../apps/server/tests/fixtures/provider-replay/tool-request-http.json"),
    ];
    for fixture in FIXTURES {
        let value: Value = serde_json::from_str(fixture).expect("scripted test operation");
        let mut session = state(value["provider"] == "claude");
        let mut expected = Vec::new();
        for key in ["events", "childEvents", "parentEvents"] {
            if let Some(events) = value[key].as_array() {
                expected.extend(events.iter().cloned());
            }
        }
        for expected_frame in expected {
            let mut bytes = serde_json::to_vec(&expected_frame).expect("scripted test operation");
            bytes.push(b'\n');
            let frame = studio_runtime_providers::LineDecoder::new()
                .push(&bytes)
                .expect("scripted test operation")
                .remove(0);
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
        "../../../apps/server/tests/fixtures/provider-replay/codex-rpc-replies.json"
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
        let mut decoder = studio_runtime_providers::LineDecoder::new();
        let mut bytes = line;
        bytes.push(b'\n');
        let frame = decoder
            .push(&bytes)
            .expect("scripted test operation")
            .remove(0);
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
            include_str!(
                "../../../apps/server/tests/fixtures/provider-replay/lost-turn-start-reply.json"
            ),
            &[Method::ThreadStart, Method::ThreadRead][..],
            Method::TurnStart,
        ),
        (
            include_str!(
                "../../../apps/server/tests/fixtures/provider-replay/restart-mid-turn-supervisor-replay.json"
            ),
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
