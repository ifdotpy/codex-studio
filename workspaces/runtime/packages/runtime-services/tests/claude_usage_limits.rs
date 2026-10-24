use serde_json::{Value, json};
use studio_runtime_services::{
    continuation::Snapshot,
    provider_adapter::{ProviderKind, adapter},
    usage_resume::{self, Cause, FailureObservation, LimitsObservation},
};

fn snapshot(model: &str) -> Snapshot {
    Snapshot::from_records(snapshot_records(model)).expect("snapshot")
}
fn snapshot_records(model: &str) -> Value {
    let record = json!({"id":"resume","accountKey":"default","threadId":"thread","epoch":1,
        "turnId":"turn","cause":"rate_limit","status":"scheduled","failedAt":900.0,
        "dueAt":990.0,"taskClaims":[]});
    json!({"agent":{
        "id":"a","rootId":"a","epoch":1,"status":"failed","accountKey":"default",
        "threadId":"thread","lastCompletedTurn":"turn","autoWake":true,"nativeFailureHold":true,
        "cwd":"/fixture","isLead":true,"provider":"claude","model":model,"usageResume":record
    },"record":record})
}
fn data(main_used: f64) -> Value {
    json!({"rateLimits":{"limitId":"claude","primary":{"usedPercent":main_used,"resetsAt":1300.0}},
    "rateLimitsByLimitId":{
        "claude-opus":{"limitId":"claude-opus","secondary":{"usedPercent":100.0,"resetsAt":9000.0}},
        "claude-sonnet":{"limitId":"claude-sonnet","secondary":{"usedPercent":20.0,"resetsAt":5000.0}}
    }})
}
fn observation(data: Value) -> LimitsObservation {
    LimitsObservation::from_record(
        json!({"accountKey":"default","at":1000.0,"data":data}),
        None,
    )
    .expect("limits")
}

#[test]
fn sonnet_wait_uses_main_reset_and_ignores_full_opus_bucket() {
    let s = snapshot("claude-sonnet-4-6");
    let full = observation(data(100.0));
    let recorded = usage_resume::record_failure(
        &s,
        FailureObservation {
            turn_id: "new-turn",
            cause: Cause::RateLimit,
            error_reset: None,
            auth_attempt: 0,
            limits: &full,
        },
        1000.0,
    )
    .expect("record failure");
    assert_eq!(
        recorded.record.expect("failure creates resume record")["dueAt"],
        1305.0
    );
    let changed = usage_resume::limits_changed(&s, &full, 1000.0).expect("limits changed");
    assert_eq!(
        changed
            .record
            .as_ref()
            .expect("limit change preserves resume record")["dueAt"],
        1305.0
    );
    let scheduled = Snapshot::from_records(json!({"agent":changed.agent,"record":changed.record}))
        .expect("rescheduled snapshot");
    let mut reset_data = data(20.0);
    reset_data["rateLimits"]["primary"]["resetsAt"] = json!(2300.0);
    let after_reset = LimitsObservation::from_record(
        json!({"accountKey":"default","at":1305.0,"data":reset_data}),
        None,
    )
    .expect("post-reset observation");
    let released =
        usage_resume::tick(&scheduled, &after_reset, 1305.0).expect("tick after main reset");
    assert_eq!(
        released
            .record
            .expect("reset release creates resume record")["status"],
        "started"
    );
    assert_eq!(released.effects.len(), 1);
}

#[test]
fn allowed_overage_releases_a_scheduled_resume() {
    let mut data = data(100.0);
    data["claudeRateLimit"] =
        json!({"rateLimitType":"five_hour","status":"rejected","overageStatus":"allowed"});
    let limits = observation(data);
    let s = snapshot("sonnet");
    let changed = usage_resume::limits_changed(&s, &limits, 1000.0).expect("limits changed");
    assert_eq!(
        changed.record.expect("limit change creates resume record")["dueAt"],
        1000.0
    );
    let released = usage_resume::tick(&s, &limits, 1000.0).expect("tick");
    assert_eq!(
        released.record.expect("resume tick creates record")["status"],
        "started"
    );
}

#[test]
fn unknown_models_and_buckets_keep_the_latest_reset() {
    for model in ["default", "custom-sonnet-proxy", "claude-new-model", ""] {
        let decision =
            usage_resume::tick(&snapshot(model), &observation(data(100.0)), 1000.0).expect("tick");
        let record = decision.record.expect("model tick creates record");
        assert_eq!(record["status"], "scheduled", "{model}");
        assert_eq!(record["dueAt"], 9005.0, "{model}");
    }
    let mut data = data(20.0);
    data["rateLimitsByLimitId"]["claude-unknown"] =
        json!({"secondary":{"usedPercent":100.0,"resetsAt":10000.0}});
    let decision =
        usage_resume::tick(&snapshot("sonnet"), &observation(data), 1000.0).expect("tick");
    assert_eq!(
        decision.record.expect("long reset creates resume record")["dueAt"],
        10005.0
    );
}

#[test]
fn plain_429_typed_error_schedules_a_rate_limit_resume() {
    let cause = adapter(ProviderKind::Claude)
        .classify_error(&json!({
            "message":"429 Too Many Requests", "codexErrorInfo":"rateLimitExceeded"
        }))
        .expect("typed cause");
    assert_eq!(cause, Cause::RateLimit);
    let limits = observation(data(100.0));
    let decision = usage_resume::record_failure(
        &snapshot("sonnet"),
        FailureObservation {
            turn_id: "plain-429",
            cause,
            error_reset: None,
            auth_attempt: 0,
            limits: &limits,
        },
        1000.0,
    )
    .expect("record failure");
    let record = decision.record.expect("failure creates resume record");
    assert_eq!(record["cause"], "rate_limit");
    assert_eq!(record["dueAt"], 1305.0);
}

#[test]
fn overage_does_not_bypass_unknown_scope_rejection_or_spend_controls() {
    for permission in [
        json!({"rateLimitType":"five_hour","overageStatus":"rejected","isUsingOverage":true}),
        json!({"rateLimitType":"unknown","overageStatus":"allowed"}),
        json!({"rateLimitType":"seven_day_opus","overageStatus":"allowed"}),
        json!({"rateLimitType":"five_hour","isUsingOverage":"true"}),
    ] {
        let mut data = data(100.0);
        data["claudeRateLimit"] = permission;
        let decision =
            usage_resume::tick(&snapshot("sonnet"), &observation(data), 1000.0).expect("tick");
        assert_eq!(
            decision.record.expect("tick preserves scheduled record")["status"],
            "scheduled"
        );
    }
    for unknown in [false, true] {
        let mut data = data(100.0);
        data["claudeRateLimit"] = json!({"rateLimitType":"five_hour","overageStatus":"allowed"});
        if unknown {
            data["rateLimitsByLimitId"]["unknown"] =
                json!({"secondary":{"usedPercent":100.0,"resetsAt":20000.0}});
        } else {
            data["rateLimits"]["spendControlReached"] = json!(true);
        }
        let decision =
            usage_resume::tick(&snapshot("sonnet"), &observation(data), 1000.0).expect("tick");
        assert_eq!(
            decision.record.expect("tick preserves scheduled record")["status"],
            "scheduled"
        );
    }
}

#[test]
fn overage_permission_covers_known_limits_but_requires_a_fresh_observation() {
    for permission in [
        json!({"overageStatus":"allowed_warning"}),
        json!({"isUsingOverage":true}),
        json!({"overageInUse":true}),
    ] {
        let mut data = data(100.0);
        data["rateLimitsByLimitId"]["claude-sonnet"]["secondary"]["usedPercent"] = json!(100.0);
        data["claudeRateLimit"] = permission;
        data["claudeRateLimit"]["rateLimitType"] = json!("seven_day_sonnet");
        let decision = usage_resume::tick(&snapshot("sonnet"), &observation(data.clone()), 1000.0)
            .expect("tick");
        assert_eq!(
            decision
                .record
                .expect("fresh observation starts resume record")["status"],
            "started"
        );
        let stale = LimitsObservation::from_record(
            json!({"accountKey":"default","at":900.0,"data":data}),
            None,
        )
        .expect("stale observation");
        let decision = usage_resume::tick(&snapshot("sonnet"), &stale, 1000.0).expect("tick");
        assert_eq!(
            decision
                .record
                .expect("stale observation preserves scheduled record")["status"],
            "scheduled"
        );
    }
}

#[tokio::test]
async fn account_limits_change_updates_each_wait_by_its_model() {
    use studio_runtime_services::continuation_runtime::stage_limits_changed_for_account;
    use studio_runtime_store::RuntimeStore;
    let dir = tempfile::tempdir().expect("isolated store");
    let store = RuntimeStore::open(Some(dir.path())).await.expect("store");
    store.transaction_with(|tx| {
        for model in ["sonnet", "opus", "default"] {
            let record = json!({"id":model,"accountKey":"default","threadId":"thread","epoch":1,"turnId":"turn","cause":"rate_limit","status":"scheduled","failedAt":900.0,"dueAt":9005.0,"taskClaims":[]});
            let agent = json!({"id":model,"rootId":model,"epoch":1,"status":"failed","accountKey":"default","threadId":"thread","lastCompletedTurn":"turn","autoWake":true,"nativeFailureHold":true,"cwd":"/fixture","isLead":true,"provider":"claude","model":model,"usageResume":record});
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", [model, &agent.to_string()]).map_err(studio_runtime_store::StoreError::from)?;
            tx.execute("INSERT INTO runtime_usage_resumes(id,agent,record) VALUES(?1,?1,?2)", [model, &record.to_string()]).map_err(studio_runtime_store::StoreError::from)?;
        }
        let raw = json!({"accountKey":"default","at":1000.0,"data":data(100.0)});
        assert!(stage_limits_changed_for_account(tx, "default", &raw, 1000.0)?);
        Ok::<_, studio_runtime_services::continuation::ContinuationError>(())
    }).await.expect("account telemetry");
    let db = store.test_reader().expect("reader");
    for (model, expected) in [("sonnet", 1305.0), ("opus", 9005.0), ("default", 9005.0)] {
        let due: f64 = db
            .query_row(
                "SELECT json_extract(record,'$.dueAt') FROM runtime_usage_resumes WHERE id=?1",
                [model],
                |row| row.get(0),
            )
            .expect("dueAt");
        assert_eq!(due, expected, "{model}");
    }
}

#[test]
fn missing_observation_reschedules_without_followup() {
    let decision = usage_resume::tick_without_observation(&snapshot("sonnet"), 1000.0)
        .expect("reschedule without telemetry");
    let record = decision.record.expect("wait remains durable");
    assert_eq!(record["status"], "scheduled");
    assert_eq!(
        record["dueAt"],
        1000.0 + studio_runtime_services::continuation::POLL_SECONDS
    );
    assert!(decision.effects.is_empty());
}

#[test]
fn ui_eligibility_matches_resume_decisions_on_shared_fixtures() {
    let fixtures: Value = serde_json::from_str(include_str!("claude_usage_ui_cases.json"))
        .expect("usage fixtures parse");
    for case in fixtures.as_array().expect("usage fixtures are an array") {
        let records = snapshot_records(case["model"].as_str().expect("fixture model is text"));
        let state = Snapshot::from_records(records.clone()).expect("resume snapshot is valid");
        let agent = &records["agent"];
        let raw = &case["observation"];
        let limits =
            LimitsObservation::from_record(raw.clone(), None).expect("limits observation is valid");
        let view = usage_resume::eligibility(raw, agent, 1000.0);
        let decision =
            usage_resume::tick(&state, &limits, 1000.0).expect("resume decision succeeds");
        assert_eq!(
            view["canRun"],
            decision.record.expect("resume decision retains its record")["status"] == "started",
            "{}",
            case["name"]
        );
        assert_eq!(view["allowed"], limits.allowed(agent, 1000.0));
        assert_eq!(view["resetAt"], json!(limits.reset_at(agent)));
        assert_eq!(view["canRun"], case["canRun"], "{}", case["name"]);
        assert_eq!(view["overage"], case["overage"], "{}", case["name"]);
        assert_eq!(
            view["buckets"]
                .as_array()
                .expect("applicable buckets are an array")
                .iter()
                .map(|b| b["id"].clone())
                .collect::<Vec<_>>(),
            case["buckets"]
                .as_array()
                .expect("expected buckets are an array")
                .clone(),
            "{}",
            case["name"]
        );
    }
}

#[test]
fn ui_decision_expires_at_the_first_applicable_window_reset() {
    let records = snapshot_records("sonnet");
    let agent = &records["agent"];
    let mut raw = json!({"accountKey":"default","at":1000.0,"data":data(20.0)});
    raw["data"]["rateLimits"]["primary"]["resetsAt"] = json!(1002.0);
    let view = usage_resume::eligibility(&raw, agent, 1000.0);
    assert_eq!(view["canRun"], true);
    assert_eq!(view["validUntil"], 1002.0);
    assert_eq!(
        usage_resume::eligibility(&raw, agent, 1002.0)["canRun"],
        false
    );
}

/// Set `key` of the wait in both the record and the agent's projection.
fn set_wait(records: &mut Value, key: &str, value: Value) {
    records["record"][key] = value.clone();
    records["agent"]["usageResume"][key] = value;
}

/// A wait whose failure named the provider reset at 1300 (Unix seconds).
fn waiting_for_reset() -> Value {
    let mut records = snapshot_records("sonnet");
    set_wait(&mut records, "resetAt", json!(1300.0));
    set_wait(&mut records, "plannedAt", json!(1300.0));
    set_wait(&mut records, "dueAt", json!(1305.0));
    records
}

fn with_due(mut records: Value, due: f64) -> Snapshot {
    set_wait(&mut records, "dueAt", json!(due));
    Snapshot::from_records(records).expect("snapshot")
}

/// A read that fails or is not current keeps the provider reset that the wait
/// holds, so the wait is still planned for that reset.
#[test]
fn a_limits_read_without_a_current_reset_keeps_the_provider_reset() {
    let failed = LimitsObservation::from_record(
        json!({"accountKey":"default","at":1000.0,"error":"limits unavailable"}),
        None,
    )
    .expect("failed read");
    let decision =
        usage_resume::tick(&with_due(waiting_for_reset(), 1000.0), &failed, 1000.0).expect("tick");
    let record = decision.record.expect("wait remains durable");
    assert_eq!(record["status"], "scheduled");
    assert_eq!(record["resetAt"], 1300.0, "{record}");
    assert_eq!(record["dueAt"], 1305.0, "{record}");

    // Current telemetry without a blocked window has no reset of its own.
    let snapshot = Snapshot::from_records(waiting_for_reset()).expect("snapshot");
    let decision =
        usage_resume::limits_changed(&snapshot, &observation(data(20.0)), 1000.0).expect("limits");
    let record = decision.record.expect("wait remains durable");
    assert_eq!(record["resetAt"], 1300.0, "{record}");
    assert_eq!(record["plannedAt"], 1300.0, "{record}");
}

/// After the provider reset, a failed, erroneous or stale limits read must not
/// keep the chat waiting: the wait resumes once with its fixed follow-up id. A
/// current read that still shows the limit keeps it waiting.
#[test]
fn a_wait_resumes_once_after_the_provider_reset_when_limits_reads_fail() {
    let now = 1306.0;
    let started = |decision: studio_runtime_services::continuation::Decision, case: &str| {
        let record = decision.record.clone().expect("record");
        assert_eq!(record["status"], "started", "{case}: {record}");
        assert_eq!(
            decision.effects,
            vec![studio_runtime_services::continuation::Effect::Followup {
                id: "usage-resume:resume".into(),
                text: usage_resume::CONTINUATION.into(),
            }],
            "{case}"
        );
        let agent = decision.agent.clone().expect("agent");
        assert_eq!(agent["status"], "queued", "{case}");
        assert!(agent.get("nativeFailureHold").is_none(), "{case}");
        // The started wait never sends its follow-up again.
        let mut records = waiting_for_reset();
        records["record"] = record.clone();
        records["agent"] = agent;
        let again = Snapshot::from_records(records).expect("snapshot");
        let repeat = usage_resume::tick_without_observation(&again, now + 600.0).expect("tick");
        assert!(repeat.effects.is_empty(), "{case}");
        assert!(repeat.record.is_none(), "{case}");
    };
    let snapshot = Snapshot::from_records(waiting_for_reset()).expect("snapshot");
    started(
        usage_resume::tick_without_observation(&snapshot, now).expect("tick"),
        "failed read",
    );
    let failed = LimitsObservation::from_record(
        json!({"accountKey":"default","at":now,"error":"limits unavailable"}),
        None,
    )
    .expect("failed read");
    started(
        usage_resume::tick(&snapshot, &failed, now).expect("tick"),
        "erroneous read",
    );
    let stale = LimitsObservation::from_record(
        json!({"accountKey":"default","at":1000.0,"data":data(100.0)}),
        None,
    )
    .expect("stale read");
    started(
        usage_resume::tick(&snapshot, &stale, now).expect("tick"),
        "stale read",
    );

    // A current read that still shows the limit until 5000 keeps the wait.
    let mut blocked = data(100.0);
    blocked["rateLimits"]["primary"]["resetsAt"] = json!(5000.0);
    let current = LimitsObservation::from_record(
        json!({"accountKey":"default","at":now,"data":blocked}),
        None,
    )
    .expect("current read");
    let decision = usage_resume::tick(&snapshot, &current, now).expect("tick");
    let record = decision.record.expect("wait remains durable");
    assert_eq!(record["status"], "scheduled", "{record}");
    assert_eq!(record["resetAt"], 5000.0, "{record}");
    assert!(decision.effects.is_empty());

    // Before the reset a failed read only reschedules for it.
    let early = with_due(waiting_for_reset(), 1200.0);
    let decision = usage_resume::tick_without_observation(&early, 1201.0).expect("tick");
    let record = decision.record.expect("wait remains durable");
    assert_eq!(record["status"], "scheduled", "{record}");
    assert_eq!(record["dueAt"], 1305.0, "{record}");
    assert!(decision.effects.is_empty());

    // A reset that had passed when the turn failed is no evidence of a new one,
    // so a wait for it does not resume without a current read.
    let mut records = waiting_for_reset();
    set_wait(&mut records, "resetAt", json!(800.0));
    let old = Snapshot::from_records(records).expect("snapshot");
    let decision = usage_resume::tick_without_observation(&old, now).expect("tick");
    assert_eq!(decision.record.expect("record")["status"], "scheduled");
    assert!(decision.effects.is_empty());
}
