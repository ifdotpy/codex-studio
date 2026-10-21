use super::*;

fn snapshot() -> PreparationSnapshot {
    serde_json::from_value(json!({
        "actor":{"id":"lead","root_id":"lead","epoch":["thread",0],"is_lead":true,
            "forked_from":null,"prepared_context":null,"delivered_mode":null},
        "root":{"concurrency":32,"mode_revision":0},"delivered_manifests":[],"plan":null,
        "complaints":[],"workers":[],"delivered_reminders":[],"assets":[],
        "guidance":{"role":"Role guidance","progress":"progress.md"},"artifact_paths":{},
        "prepared_at":1.0,"operation_id":"prepare"
    }))
    .expect("snapshot")
}
fn event(id: &str, kind: &str, text: String) -> Event {
    Event {
        id: id.into(),
        kind: kind.into(),
        text,
        created: 1.0,
        metadata: EventMetadata {
            assets: vec![],
            accepted_at: None,
            extra: Map::new(),
        },
        preserve_progress: false,
        preserve_complaint: false,
    }
}
fn text(p: &PreparedSubmission) -> String {
    p.provider_inputs
        .iter()
        .filter_map(|p| match p {
            ProviderInput::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn aggregate_budget_covers_complaints_plan_decision_and_user_text() {
    let mut s = snapshot();
    for index in 0..32 {
        s.complaints.push(Complaint {
            id: format!("complaint-{index}"),
            lead_id: "lead".into(),
            recipient: Recipient::Lead,
            status: ComplaintStatus::Open,
            legacy_user_task: false,
            response_authors: vec![],
            author: format!("worker-{index}"),
            author_name: None,
            text: format!("Complaint {index}: {}", "Ж".repeat(12_000)),
            version: Some(1),
            created: 1.0,
        });
    }
    s.plan = Some(Plan {
        id: "lead".into(),
        root_id: "lead".into(),
        steps: vec![],
        explanation: "plan".repeat(20_000),
    });
    s.guidance.role = "role".repeat(10_000);
    s.guidance.progress = "progress".repeat(10_000);
    let user = "USER EXACT ".repeat(6_000);
    let decision = event("decision", "work_decision", "rejection".repeat(4_000));
    let rows = [event("user", "user", user.clone()), decision];
    let p = compile(&s, &rows).expect("compile");
    let output = text(&p);
    assert!(output.contains(&user));
    assert!(output.len() - user.len() <= STUDIO_INPUT_BYTES);
    let full = p.studio_input.as_ref().expect("spill");
    for index in 0..32 {
        assert!(full.contains(&format!("Complaint {index}:")));
    }
    assert!(full.contains("event:decision"));
    assert!(output.contains(&format!("studio-input:{}", sha256_hex(full.as_bytes()))));
    assert_eq!(
        p.metadata_patches["user"]["studioInput"]["size"],
        full.len()
    );
    assert_eq!(text(&compile(&s, &rows).expect("replay")), output);
}

#[test]
fn decision_projection_is_bounded_and_references_the_original_event() {
    let s = snapshot();
    let p = compile(&s, &[event("reject", "work_decision", "x".repeat(32_000))]).expect("compile");
    let output = text(&p);
    assert!(output.len() <= STUDIO_INPUT_BYTES);
    assert!(output.contains("event:reject"));
    assert!(output.contains("truncated"));
}

#[test]
fn roster_is_delivered_only_once_per_accepted_compaction_epoch() {
    let mut s = snapshot();
    s.team_state = Some(
        json!({"children":[{"id":"worker","name":"Worker","status":"running","tasks":["assignment"]}],
        "monitors":[{"id":"monitor","status":"running"}],"tasks":[{"id":"review","status":"review"}]}),
    );
    let rows = [event("wake", "user", "continue".into())];
    let cold = compile(&s, &rows).expect("cold");
    assert!(!text(&cold).contains("[Studio team state after compaction]"));
    s.delivered_manifests.push(cold.manifest);
    s.actor.epoch.1 += 1;
    let restored = compile(&s, &rows).expect("compacted");
    let output = text(&restored);
    assert_eq!(
        output
            .matches("[Studio team state after compaction]")
            .count(),
        1
    );
    for expected in [
        "worker",
        "assignment",
        "monitor",
        "review",
        "Do not respawn",
        "orchestration_status",
        "orchestration_task",
    ] {
        assert!(output.contains(expected), "{expected}");
    }
    let mut receipt = NativeReceipt {
        operation_id: s.operation_id.clone(),
        actor_id: "lead".into(),
        epoch: s.actor.epoch.clone(),
        event_ids: vec!["wake".into()],
        outcome: ReceiptOutcome::Unknown,
    };
    assert!(accept(&restored, &receipt, None).manifest_upsert.is_none());
    assert_eq!(text(&compile(&s, &rows).expect("retry")), output);
    receipt.outcome = ReceiptOutcome::Accepted {
        turn_id: "turn".into(),
    };
    s.delivered_manifests.push(
        accept(&restored, &receipt, None)
            .manifest_upsert
            .expect("accepted"),
    );
    assert!(
        !text(&compile(&s, &rows).expect("next turn"))
            .contains("[Studio team state after compaction]")
    );
    s.actor.epoch.1 += 1;
    assert!(
        text(&compile(&s, &rows).expect("second compaction"))
            .contains("[Studio team state after compaction]")
    );
}

#[test]
fn aggregate_limit_handles_many_wrappers_long_ids_and_multibyte_text() {
    let mut s = snapshot();
    s.actor.epoch.1 = 1;
    s.team_state = Some(
        json!({"children":(0..100).map(|i|json!({"id":i,"name":"😀".repeat(500)})).collect::<Vec<_>>() }),
    );
    let rows: Vec<_> = (0..200)
        .map(|i| {
            event(
                &format!("{i}{}", "long".repeat(200)),
                "monitor_exit",
                "😀".repeat(20_000),
            )
        })
        .collect();
    let p = compile(&s, &rows).expect("compile");
    assert!(text(&p).len() <= STUDIO_INPUT_BYTES);
    assert!(text(&p).contains("[Studio team state after compaction]"));
    assert!(
        p.studio_input
            .as_ref()
            .expect("spill")
            .contains("orchestration_status")
    );
}

#[test]
fn attachment_descriptions_and_receipt_clocks_share_the_aggregate_limit() {
    let mut s = snapshot();
    s.assets.push(Asset {
        id: "file".into(),
        owner_id: "lead".into(),
        owner_root_id: "lead".into(),
        owner_deleted: false,
        image: false,
        path: format!("/{}", "long-path/".repeat(6_000)),
        name: "file".repeat(10_000),
        size: 1,
        view: Map::new(),
    });
    let mut row = event(&"event".repeat(10_000), "followup", "user text".into());
    row.metadata.assets.push("file".into());
    row.metadata.accepted_at = Some(1.0);
    let p = compile(&s, &[row]).unwrap();
    assert!(text(&p).len() - "user text".len() <= STUDIO_INPUT_BYTES);
    assert!(text(&p).contains("user text"));
    let full = p.studio_input.as_ref().unwrap();
    assert!(full.contains("[Time awareness, message receipt time]"));
    assert!(full.contains("Attached file:"));
    assert!(full.contains(&s.assets[0].path));
}

#[test]
fn a_thread_replacement_alone_does_not_repeat_the_compaction_roster() {
    let mut s = snapshot();
    s.actor.epoch = Epoch(Some("old-thread".into()), 3);
    let p = compile(&s, &[event("wake", "user", "continue".into())]).unwrap();
    s.delivered_manifests.push(p.manifest);
    s.actor.epoch.0 = Some("new-thread".into());
    s.team_state = Some(json!({"children":[],"monitors":[],"tasks":[]}));
    assert!(!needs_team_restore(&s.actor.epoch, &s.delivered_manifests));
    assert!(
        !text(&compile(&s, &[event("next", "user", "continue".into())]).unwrap())
            .contains("[Studio team state after compaction]")
    );
    s.actor.epoch.1 += 1;
    assert!(needs_team_restore(&s.actor.epoch, &s.delivered_manifests));
}

#[test]
fn bounded_assignments_keep_input_origin_and_reports_remain_data() {
    let mut s = snapshot();
    s.provider = Some(ProviderKind::Claude);
    s.guidance.role = "role".repeat(20_000);
    let mut followup = event("followup", "followup", "assignment".repeat(32_000));
    followup
        .metadata
        .extra
        .insert("senderId".into(), json!("parent"));
    let task_text = followup.text.clone();
    let rows = [
        event("decision", "work_decision", "decision".repeat(32_000)),
        event("ready", "work_ready", "ready".repeat(32_000)),
        followup,
        event("report", "monitor_exit", "report".repeat(32_000)),
    ];
    let p = compile(&s, &rows).unwrap();
    assert!(text(&p).len() - task_text.len() <= STUDIO_INPUT_BYTES);
    assert!(p.provider_inputs.iter().any(|input| matches!(input,
        ProviderInput::Text { text, origin: Some(ProviderInputOrigin::Human) } if text.contains(&task_text))));
    assert!(p.provider_inputs.windows(2).all(|pair| match pair {
        [
            ProviderInput::Text { origin: left, .. },
            ProviderInput::Text { origin: right, .. },
        ] => left != right,
        _ => true,
    }));
    let full = p.studio_input.as_ref().unwrap();
    assert!(full.contains("event:decision"));
    assert!(full.contains("event:ready"));
    for (kind, expected) in [
        ("work_decision", ProviderInputOrigin::Human),
        ("work_ready", ProviderInputOrigin::Human),
        ("followup", ProviderInputOrigin::Human),
        ("monitor_exit", ProviderInputOrigin::StudioEvent),
    ] {
        let origin = p.provider_inputs.iter().find_map(|input| match input {
            ProviderInput::Text { text, origin }
                if text.contains(&format!("[Orchestration event: {kind}]")) =>
            {
                Some(*origin)
            }
            _ => None,
        });
        assert_eq!(origin, Some(Some(expected)), "{kind}");
    }
    let ProviderInput::Text { origin, .. } = &p.provider_inputs[0] else {
        panic!("text")
    };
    assert_eq!(*origin, Some(ProviderInputOrigin::StudioEvent));
}

#[test]
fn system_followups_without_a_sender_remain_bounded() {
    for sender in [None, Some("")] {
        let mut s = snapshot();
        s.provider = Some(ProviderKind::Claude);
        let mut row = event(
            "system-followup",
            "followup",
            "system notice".repeat(32_000),
        );
        if let Some(sender) = sender {
            row.metadata.extra.insert("senderId".into(), json!(sender));
        }
        let p = compile(&s, &[row]).unwrap();
        assert!(text(&p).len() <= STUDIO_INPUT_BYTES);
        assert!(text(&p).contains("event:system-followup"));
        assert!(p.provider_inputs.iter().all(|input| matches!(
            input,
            ProviderInput::Text {
                origin: Some(ProviderInputOrigin::StudioEvent),
                ..
            }
        )));
    }
}
