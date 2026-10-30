//! Liveness detection must never change a chat or its pending input.
use serde_json::json;
use std::sync::Arc;
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, SchedulerRepository};

async fn seeded(now: f64) -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let root = tempfile::tempdir().expect("liveness state");
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            for (id, fields) in [
                ("idle", json!({})),
                ("stopped", json!({"status":"paused","autoWake":false})),
                ("held", json!({"status":"waiting","nativeFailureHold":true})),
                ("unknown", json!({"startOutcomeHold":{"reason":"unknown"}})),
                ("codex", json!({"status":"running","inFlight":true,"turnId":"codex-turn",
                    "created":now - 1000.0,"activity":{"at":now - 700.0}})),
                ("claude", json!({"status":"running","provider":"claude","inFlight":true,
                    "turnId":"claude-turn","created":now - 1000.0,"activity":{"at":now - 700.0}})),
                ("session", json!({"status":"queued","startWait":{"reason":"provider_session","epoch":0}})),
                ("resume", json!({"status":"waiting","usageResume":{"id":"resume"}})),
                ("capacity", json!({"status":"waiting","capacityRetry":{"id":"capacity"}})),
                ("transfer", json!({"accountTransferId":"transfer"})),
            ] {
                let mut record = json!({"id":id,"rootId":id,"epoch":0,"status":"completed",
                    "autoWake":true,"cwd":path.to_string_lossy(),"accountKey":"default",
                    "provider":"codex","name":id});
                for (key, value) in fields.as_object().expect("fields") {
                    record[key] = value.clone();
                }
                AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES (?1,?2,'user','continue','pending',?3,0)",
                    rusqlite::params![format!("{id}-message"), id, now - 180.0],
                )?;
            }
            tx.execute("INSERT INTO runtime_usage_resumes(id,agent,record) VALUES ('resume','resume',?1)",
                [json!({"id":"resume","status":"scheduled","dueAt":now - 1000.0}).to_string()])?;
            tx.execute("INSERT INTO runtime_capacity_retries(id,agent,record) VALUES ('capacity','capacity',?1)",
                [json!({"id":"capacity","status":"scheduled","dueAt":now - 1000.0}).to_string()])?;
            tx.execute("INSERT INTO runtime_account_transfers(id,record) VALUES ('transfer',?1)",
                [json!({"id":"transfer","leadId":"transfer","status":"pending","created":now - 1000.0,
                    "members":{"transfer":{"phase":"blocked","error":"catalog unavailable"}}}).to_string()])?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("seed");
    (root, store)
}

async fn stored_state(store: &RuntimeStore) -> Vec<String> {
    store
        .read_async("tests/liveness-state", |tx| {
            let mut result = Vec::new();
            for table in [
                "runtime_agents",
                "runtime_events",
                "request_results",
                "runtime_usage_resumes",
                "runtime_capacity_retries",
                "runtime_account_transfers",
            ] {
                let mut statement = tx.prepare(&format!("SELECT * FROM {table} ORDER BY id"))?;
                let columns = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        Ok((0..columns)
                            .map(|column| format!("{:?}", row.get_ref(column)))
                            .collect::<Vec<_>>()
                            .join("|"))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                result.push(format!("{table}: {rows:?}"));
            }
            Ok::<_, studio_runtime_store::StoreError>(result)
        })
        .await
        .expect("stored state")
}

#[tokio::test]
async fn detection_never_wakes_an_idle_chat_or_changes_held_input() {
    let now = studio_runtime_services::event_write::unix_time().expect("clock");
    let (_root, store) = seeded(now).await;
    let before = stored_state(&store).await;
    for at in [now, now + 120.0] {
        let list = store
            .read_async("tests/liveness", move |tx| {
                studio_runtime_services::studio_errors::list(tx, at)
            })
            .await
            .expect("Studio errors");
        let rows = list["items"].as_array().expect("error rows");
        for (agent, kind) in [
            ("idle", "input_not_started"),
            ("stopped", "input_not_started"),
            ("held", "input_not_started"),
            ("unknown", "input_not_started"),
            ("codex", "turn_stalled"),
            ("claude", "turn_stalled"),
            ("session", "session_not_started"),
            ("resume", "schedule_stalled"),
            ("capacity", "schedule_stalled"),
            ("transfer", "schedule_stalled"),
        ] {
            let row = rows
                .iter()
                .find(|row| row["agentId"] == agent && row["kind"] == kind)
                .unwrap_or_else(|| panic!("missing {agent}/{kind}: {list}"));
            assert_eq!(row["action"], "check", "{row}");
            for label in ["Account:", "Start:", "Last provider event:"] {
                assert!(
                    row["detail"].as_str().expect("detail").contains(label),
                    "{row}"
                );
            }
        }
        assert_eq!(
            stored_state(&store).await,
            before,
            "detection changes no stored state"
        );
    }
}

#[tokio::test]
async fn an_exact_manual_delivery_hold_keeps_stall_detection_without_waking_or_stopping() {
    let now = studio_runtime_services::event_write::unix_time().expect("clock");
    let root = tempfile::tempdir().expect("manual hold state");
    let store = RuntimeStore::open(Some(root.path())).await.expect("store");
    let cwd = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            for (id, changed_field) in [
                ("exact", None),
                ("stopped", None),
                ("plain-pause", None),
                ("old-epoch", Some("epoch")),
                ("old-attempt", Some("attemptId")),
                ("old-account", Some("accountKey")),
                ("old-thread", Some("threadId")),
                ("old-turn", Some("turnId")),
                ("old-generation", Some("generation")),
            ] {
                let mut agent = AgentRecord::from_value(json!({
                    "id":id,"rootId":id,"epoch":3,"status":"running","autoWake":true,
                    "inFlight":true,"turnEpoch":3,"threadId":"thread","turnId":"turn",
                    "accountKey":"account","provider":"codex","cwd":cwd,
                    "created":now - 1000.0,"activity":{"at":now - 700.0},
                    "startAttempt":{"id":format!("attempt-{id}"),"epoch":3,"accountKey":"account","events":[],
                        "threadId":"thread","turnId":"turn","submitted":true,
                        "requestTicket":"ticket","connectionGeneration":7}
                }))?;
                SchedulerRepository::hold_recovered_delivery(&mut agent)?;
                if id == "plain-pause" {
                    agent.extra.remove("manualRecoveryDeliveryHold");
                }
                if let Some(field) = changed_field {
                    agent
                        .extra
                        .get_mut("manualRecoveryDeliveryHold")
                        .expect("hold")[field] = if matches!(field, "epoch" | "generation") {
                        json!(999)
                    } else {
                        json!("stale")
                    };
                }
                studio_runtime_store::ExecutionHistoryRepository::record_current(tx, &mut agent)?;
                AgentRepository::put(tx, &agent)?;
                if id == "stopped" {
                    assert!(SchedulerRepository::stop_agent(
                        tx,
                        id,
                        "Explicit user Stop"
                    )?);
                }
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("manual recovery holds");
    let before = stored_state(&store).await;
    let list = store
        .read_async("tests/manual-hold-liveness", move |tx| {
            studio_runtime_services::studio_errors::list(tx, now)
        })
        .await
        .expect("Studio errors");
    let stalled = list["items"]
        .as_array()
        .expect("error rows")
        .iter()
        .filter(|row| row["kind"] == "turn_stalled")
        .collect::<Vec<_>>();
    assert_eq!(
        stalled.len(),
        1,
        "only the exact current hold has an active native turn: {list}"
    );
    assert_eq!(stalled[0]["agentId"], "exact");
    assert_eq!(stalled[0]["action"], "check");
    for label in ["Account:", "Start:", "Last provider event:"] {
        assert!(
            stalled[0]["detail"]
                .as_str()
                .expect("detail")
                .contains(label)
        );
    }
    assert_eq!(
        stored_state(&store).await,
        before,
        "detection changes no stored state"
    );
}

#[tokio::test]
async fn partial_schedule_observations_and_session_dedup_keep_legacy_behavior() {
    let now = 10000.0;
    let (_root, store) = seeded(now).await;
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_usage_resumes SET record=?1 WHERE id='resume'", [r#"{"id":"ignored","id":"payload","status":"scheduled","dueAt":8000,"reason":4,"reason":"latest"}"#])?;
        tx.execute("UPDATE runtime_capacity_retries SET record=?1 WHERE id='capacity'", [r#"{"id":null,"status":"scheduled","dueAt":8000.5,"attempt":3.0,"maxAttempts":7,"reason":null}"#])?;
        tx.execute("UPDATE runtime_account_transfers SET record=?1 WHERE id='transfer'", [r#"{"id":"foreign","leadId":"transfer","status":"pending","created":8000,"updated":null,"targetAccountKey":null,"members":{"z":{"phase":"unknown","name":9},"a":{"phase":"blocked","error":"first","error":"last"},"ignored":3}}"#])?;
        studio_runtime_services::liveness::record_session_start_failure(tx, "default", "unchanged", 1.0)?;
        studio_runtime_services::liveness::record_session_start_failure(tx, "default", "unchanged", 9.0)?;
        let failure: String = tx.query_row("SELECT start_failure FROM runtime_account_state WHERE account='default'", [], |r| r.get(0))?;
        assert_eq!(serde_json::from_str::<serde_json::Value>(&failure).unwrap()["at"], 1.0);
        Ok::<_, studio_runtime_store::StoreError>(())
    }).await.unwrap();
    let before = stored_state(&store).await;
    let findings = store
        .read_async("test/partial-liveness", move |tx| {
            studio_runtime_services::liveness::findings(tx, now)
        })
        .await
        .unwrap();
    let find = |id: &str| findings.iter().find(|f| f.id == id).unwrap();
    assert!(
        find("liveness:schedule_stalled:usage:payload")
            .detail
            .contains("Last reason: latest")
    );
    assert_eq!(
        find("liveness:schedule_stalled:capacity:capacity").subject,
        "Capacity retry, attempt 0 of 7"
    );
    let transfer = find("liveness:schedule_stalled:transfer:transfer:8000");
    assert_eq!(transfer.subject, "Account transfer to another account");
    assert!(transfer.detail.contains("a (blocked): last; z (unknown)"));
    let rows = store
        .read_async("test/partial-errors", move |tx| {
            studio_runtime_services::studio_errors::list(tx, now)
        })
        .await
        .unwrap();
    assert!(
        rows["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["id"] == transfer.id)
    );
    assert_eq!(stored_state(&store).await, before);
}
