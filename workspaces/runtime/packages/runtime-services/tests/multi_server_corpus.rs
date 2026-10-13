#[path = "multi_server/common.rs"]
mod common;
use common::*;
use serde_json::{Value, json};
use studio_runtime_services::multi_server_auth::{AccessSnapshot, OwnerObservation, authorize};
use studio_runtime_services::multi_server_discovery::DiscoverySnapshot;

fn full_matrix() -> bool {
    std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some()
}

fn python_authentication_outputs_match_complete_principal_and_writes() {
    let data = &fixture().data;
    for case in data["auth"].as_array().expect("cases") {
        let result = (|| {
            let request = request(case)?;
            let snapshot = AccessSnapshot::from_records(
                case["clientId"].as_str().expect("id"),
                case["client"].as_str(),
                case["inviteId"].as_str().expect("invite"),
                case["invite"].as_str(),
                true,
            )?;
            let mut whois = data["whois"].clone();
            whois["UserProfile"]["LoginName"] = case["owner"].clone();
            let owner = OwnerObservation::from_json("100.64.0.2", &[], &data["status"], &whois)?;
            authorize(
                &snapshot,
                &request,
                "local-server",
                &owner,
                case.get("probe"),
                data["now"].as_i64().expect("now"),
            )
        })();
        if let Some(status) = case["status"].as_u64() {
            assert_eq!(
                result.err().expect("refused").status(),
                status as u16,
                "{}",
                case["name"]
            );
        } else {
            let actual = result.unwrap_or_else(|e| panic!("{}: {e}", case["name"]));
            assert_eq!(
                json!({"principal":actual.principal.value(),"clientWrite":actual.client_write}),
                case["expected"],
                "{}",
                case["name"]
            );
        }
    }
}
fn malformed_stored_rows_are_located_without_panics() {
    let data = &fixture().data;
    for field in ["id", "clientId", "kind", "publicKey", "status"] {
        for value in [Value::Null, json!(9), json!([]), json!({})] {
            let mut client: Value =
                serde_json::from_str(data["client"].as_str().expect("client")).expect("JSON");
            client[field] = value;
            let error = AccessSnapshot::from_records(
                "peer-server",
                Some(&client.to_string()),
                "",
                None,
                true,
            )
            .err()
            .expect("invalid row refused");
            assert!(
                error
                    .to_string()
                    .contains("runtime_access_clients/peer-server"),
                "{error}"
            );
        }
    }
}
fn owner_observations_reject_tags_wrong_ip_and_spoofed_login() {
    let data = &fixture().data;
    assert!(
        OwnerObservation::from_json("127.0.0.1", &[], &data["status"], &data["whois"]).is_err()
    );
    assert!(
        OwnerObservation::from_json(
            "100.64.0.2",
            &["foreign".into()],
            &data["status"],
            &data["whois"]
        )
        .is_err()
    );
    let mut whois = data["whois"].clone();
    whois["Node"]["Tags"] = json!(["tag:server"]);
    assert!(OwnerObservation::from_json("100.64.0.2", &[], &data["status"], &whois).is_err());
    assert!(
        OwnerObservation::from_json(
            "100.64.0.2",
            &["owner@example.test".into(), "owner@example.test".into()],
            &data["status"],
            &data["whois"]
        )
        .is_err()
    );
}
fn discovery_includes_local_other_port_and_only_online_owner_nodes() {
    let data = &fixture().data;
    let snapshot = DiscoverySnapshot::from_status(
        &data["status"],
        "local-server",
        "https://local.fixture.ts.net",
    )
    .expect("status");
    assert_eq!(serde_json::json!(snapshot.candidates()), data["candidates"]);
    let decision = studio_runtime_services::multi_server_discovery::decide_probe(
        &snapshot,
        "https://peer.fixture.ts.net",
        &data["discoveryProbe"],
        None,
        None,
        true,
        data["now"].as_i64().expect("now"),
    )
    .expect("probe");
    assert_eq!(decision.record(), Some(&data["discovered"]));
    assert!(!decision.auto_pair());
    assert_eq!(
        snapshot.candidates(),
        vec![
            "https://local.fixture.ts.net:8443",
            "https://peer.fixture.ts.net",
            "https://peer.fixture.ts.net:8443"
        ]
    );
}
fn pairing_is_atomic_at_every_write_and_replays_after_restart() {
    let data = &fixture().data;
    let case = &data["auth"][0];
    let now = data["now"].as_i64().expect("now");
    for (table, operation) in [
        ("runtime_access_nonces", "INSERT"),
        ("runtime_access_invites", "UPDATE"),
        ("runtime_access_clients", "INSERT"),
        ("runtime_access_receipts", "INSERT"),
        ("runtime_access_audit", "INSERT"),
    ]
    .into_iter()
    .take(if full_matrix() { 5 } else { 1 })
    {
        let (root, store) = store();
        let before = rows(&store);
        store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER failpoint BEFORE {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture failpoint'); END"))?;Ok(())}).expect("failpoint");
        assert!(
            boundary()
                .handle(
                    &store,
                    &request(case).expect("request"),
                    &owner(),
                    None,
                    now
                )
                .is_err(),
            "{table}"
        );
        assert_eq!(rows(&store), before, "rollback at {table}");
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER failpoint")?;
                Ok(())
            })
            .expect("remove failpoint");
        drop(store);
        let reopened =
            studio_runtime_store::RuntimeStore::open(Some(root.path())).expect("restart");
        let result = boundary()
            .handle(
                &reopened,
                &request(case).expect("request"),
                &owner(),
                None,
                now,
            )
            .expect("first apply after rollback");
        assert_eq!(result["paired"], true);
        let status: String = reopened
            .read()
            .expect("read")
            .query_row(
                "SELECT json_extract(record,'$.status') FROM runtime_access_invites WHERE id=?1",
                [data["invitation"]["inviteId"].as_str().expect("invite ID")],
                |r| r.get(0),
            )
            .expect("invite state");
        assert_eq!(status, "used");
        let fresh = request(&data["freshPair"]).expect("fresh nonce");
        assert_eq!(
            boundary()
                .handle(&reopened, &fresh, &owner(), None, now)
                .expect("recover pairing result"),
            result
        );
        let audit:i64=reopened.read().expect("read").query_row("SELECT count(*) FROM runtime_access_audit WHERE client='new-client' AND action='pair'",[],|r|r.get(0)).expect("audit");
        assert_eq!(audit, 1, "pairing applied once");
        assert!(
            boundary()
                .handle(
                    &reopened,
                    &request(case).expect("request"),
                    &owner(),
                    None,
                    now
                )
                .is_err(),
            "nonce replay"
        );
    }
}

#[test]
fn multi_server_corpus_witness() {
    python_authentication_outputs_match_complete_principal_and_writes();
    malformed_stored_rows_are_located_without_panics();
    owner_observations_reject_tags_wrong_ip_and_spoofed_login();
    discovery_includes_local_other_port_and_only_online_owner_nodes();
    pairing_is_atomic_at_every_write_and_replays_after_restart();
    orchestration_claims_survive_lost_receipts_and_reject_changed_content();
    python_receipt_states_preserve_completed_results_and_uncertainty();
}

fn orchestration_claims_survive_lost_receipts_and_reject_changed_content() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use studio_runtime_services::multi_server_auth::AccessError;
    use studio_runtime_services::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationDriver, OrchestrationEffects, PairedTransport,
    };
    struct Effects(Arc<AtomicUsize>);
    impl OrchestrationEffects for Effects {
        fn supports(&self, action: &str) -> bool {
            action == "fixture"
        }
        fn execute(&self, _: &str, _: &Value) -> Result<ExchangeOutcome, AccessError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ExchangeOutcome::Applied(json!({"applied":true})))
        }
    }
    struct Transport(Arc<AtomicUsize>, bool);
    impl PairedTransport for Transport {
        fn exchange(
            &self,
            _: &str,
            envelope: &Value,
            _: std::time::Duration,
        ) -> Result<Value, AccessError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(if self.1 {
                json!({"requestId":envelope["requestId"],"outcome":"unknown"})
            } else {
                json!({"requestId":envelope["requestId"],"outcome":"applied","value":{"applied":true}})
            })
        }
    }
    let now = fixture().data["now"].as_i64().expect("now");
    for direction in if full_matrix() {
        &["inbox", "outbox"][..]
    } else {
        &["outbox"][..]
    } {
        for stage in ["claim", "finish"] {
            let (root, store) = store();
            let calls = Arc::new(AtomicUsize::new(0));
            let event = if stage == "claim" { "INSERT" } else { "UPDATE" };
            store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER failpoint BEFORE {event} ON runtime_server_{direction} BEGIN SELECT RAISE(ABORT,'fixture failpoint'); END"))?;Ok(())}).expect("failpoint");
            let run = |store: Arc<studio_runtime_store::RuntimeStore>| {
                let driver = OrchestrationDriver::new(
                    store,
                    Arc::new(Transport(calls.clone(), false)),
                    Arc::new(Effects(calls.clone())),
                );
                if *direction == "inbox" {
                    driver.receive("peer-server",&json!({"requestId":"effect-a","action":"fixture","payload":{"actor":"fixture"}}),now)
                } else {
                    driver.exchange(
                        "peer-server",
                        "fixture",
                        &json!({"actor":"fixture"}),
                        "effect-a",
                        now,
                    )
                }
            };
            assert!(run(store.clone()).is_err(), "{direction}/{stage}");
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(stage == "finish"));
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER failpoint")?;
                    Ok(())
                })
                .expect("remove failpoint");
            drop(store);
            let reopened = Arc::new(
                studio_runtime_store::RuntimeStore::open(Some(root.path())).expect("restart"),
            );
            let result = run(reopened.clone()).expect("recovery");
            assert_eq!(
                result,
                if stage == "claim" {
                    ExchangeOutcome::Applied(json!({"applied":true}))
                } else {
                    ExchangeOutcome::Unknown
                }
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "never repeat effect at {direction}/{stage}"
            );
            let driver = OrchestrationDriver::new(
                reopened,
                Arc::new(Transport(calls.clone(), false)),
                Arc::new(Effects(calls.clone())),
            );
            let changed = if *direction == "inbox" {
                driver.receive("peer-server",&json!({"requestId":"effect-a","action":"fixture","payload":{"actor":"different"}}),now)
            } else {
                driver.exchange(
                    "peer-server",
                    "fixture",
                    &json!({"actor":"different"}),
                    "effect-a",
                    now,
                )
            };
            assert_eq!(changed.expect_err("conflict").status(), 409);
        }
    }
    let (_root, store) = store();
    let calls = Arc::new(AtomicUsize::new(0));
    let driver = OrchestrationDriver::new(
        store.clone(),
        Arc::new(Transport(calls.clone(), true)),
        Arc::new(Effects(calls.clone())),
    );
    assert_eq!(
        driver
            .exchange("peer-server", "fixture", &json!({}), "unknown", now)
            .expect("unknown"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(
        driver
            .exchange("peer-server", "fixture", &json!({}), "unknown", now + 100)
            .expect("unknown retained"),
        ExchangeOutcome::Unknown
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let before = rows(&store);
    assert_eq!(
        driver
            .receive(
                "peer-server",
                &json!({"requestId":"missing","action":"absent","payload":{}}),
                now
            )
            .expect_err("named refusal")
            .status(),
        501
    );
    assert_eq!(rows(&store), before);
    let queued = &fixture().data["queued"];
    let envelope: Value =
        serde_json::from_str(queued["body"].as_str().expect("body")).expect("envelope");
    assert_eq!(
        driver
            .exchange(
                "peer-server",
                "browser_callback",
                &envelope["payload"],
                "python-queued",
                now
            )
            .expect("Python-written queue"),
        ExchangeOutcome::Unknown
    );
}

fn python_receipt_states_preserve_completed_results_and_uncertainty() {
    let data = &fixture().data;
    let case = &data["auth"][0];
    let now = data["now"].as_i64().expect("now");
    for state in if full_matrix() {
        &["pending", "complete", "tombstone", "expired", "malformed"][..]
    } else {
        &["pending", "expired"][..]
    } {
        let state = *state;
        let (_root, store) = store();
        let result = boundary()
            .handle(
                &store,
                &request(case).expect("request"),
                &owner(),
                None,
                now,
            )
            .expect("pair");
        store.transaction(|tx| {
            if state == "expired" {
                tx.execute("UPDATE runtime_access_receipts SET record=json_set(record,'$.completed',?1) WHERE client='new-client'", [now-8*86400])?;
            } else if state == "malformed" {
                tx.execute("UPDATE runtime_access_receipts SET record=json_set(record,'$.body',9) WHERE client='new-client'", [])?;
            } else {
                tx.execute("UPDATE runtime_access_receipts SET status=?1 WHERE client='new-client'", [state])?;
            }
            Ok(())
        }).expect("saved Python-compatible state");
        let before = rows(&store);
        let replay = boundary().handle(
            &store,
            &request(&data["freshPair"]).expect("request"),
            &owner(),
            None,
            now,
        );
        if matches!(state, "pending" | "complete") {
            assert_eq!(replay.expect("recovered receipt"), result);
            let saved: String = store
                .read()
                .expect("read")
                .query_row(
                    "SELECT status FROM runtime_access_receipts WHERE client='new-client'",
                    [],
                    |r| r.get(0),
                )
                .expect("saved");
            assert_eq!(saved, "complete");
        } else {
            assert_eq!(
                replay.err().expect("refused").status(),
                if state == "malformed" { 503 } else { 410 }
            );
            assert_eq!(rows(&store), before, "refusal is atomic");
        }
    }
}
