use serde_json::{Value, json};
use studio_runtime_services::{
    continuation::Snapshot,
    provider_adapter::{ProviderKind, adapter},
    usage_resume::{self, Cause, FailureObservation, LimitsObservation},
};

fn snapshot(model: &str) -> Snapshot {
    let record = json!({"id":"resume","accountKey":"default","threadId":"thread","epoch":1,
        "turnId":"turn","cause":"rate_limit","status":"scheduled","failedAt":900.0,
        "dueAt":990.0,"taskClaims":[]});
    Snapshot::from_records(json!({"agent":{
        "id":"a","rootId":"a","epoch":1,"status":"failed","accountKey":"default",
        "threadId":"thread","lastCompletedTurn":"turn","autoWake":true,"nativeFailureHold":true,
        "cwd":"/fixture","isLead":true,"provider":"claude","model":model,"usageResume":record
    },"record":record}))
    .expect("snapshot")
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
    assert_eq!(recorded.record.unwrap()["dueAt"], 1305.0);
    let changed = usage_resume::limits_changed(&s, &full, 1000.0).expect("limits changed");
    assert_eq!(changed.record.as_ref().unwrap()["dueAt"], 1305.0);
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
    assert_eq!(released.record.unwrap()["status"], "started");
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
    assert_eq!(changed.record.unwrap()["dueAt"], 1000.0);
    let released = usage_resume::tick(&s, &limits, 1000.0).expect("tick");
    assert_eq!(released.record.unwrap()["status"], "started");
}

#[test]
fn unknown_models_and_buckets_keep_the_latest_reset() {
    for model in ["default", "custom-sonnet-proxy", "claude-new-model", ""] {
        let decision =
            usage_resume::tick(&snapshot(model), &observation(data(100.0)), 1000.0).expect("tick");
        let record = decision.record.unwrap();
        assert_eq!(record["status"], "scheduled", "{model}");
        assert_eq!(record["dueAt"], 9005.0, "{model}");
    }
    let mut data = data(20.0);
    data["rateLimitsByLimitId"]["claude-unknown"] =
        json!({"secondary":{"usedPercent":100.0,"resetsAt":10000.0}});
    let decision =
        usage_resume::tick(&snapshot("sonnet"), &observation(data), 1000.0).expect("tick");
    assert_eq!(decision.record.unwrap()["dueAt"], 10005.0);
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
    let record = decision.record.unwrap();
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
        assert_eq!(decision.record.unwrap()["status"], "scheduled");
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
        assert_eq!(decision.record.unwrap()["status"], "scheduled");
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
        assert_eq!(decision.record.unwrap()["status"], "started");
        let stale = LimitsObservation::from_record(
            json!({"accountKey":"default","at":900.0,"data":data}),
            None,
        )
        .expect("stale observation");
        let decision = usage_resume::tick(&snapshot("sonnet"), &stale, 1000.0).expect("tick");
        assert_eq!(decision.record.unwrap()["status"], "scheduled");
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
