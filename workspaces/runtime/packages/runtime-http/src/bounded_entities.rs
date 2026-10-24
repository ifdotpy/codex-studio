//! Source-driven publication of bounded summaries. Stream reads never rebuild them.

use crate::router::HttpState;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use studio_runtime_services::resource_notify::{self, BoundedSource};
use studio_runtime_store::{ObservedTable, RuntimeStore, StoreError, SyncDocumentsRepository};
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Publication {
    Accounts,
    Costs,
    SessionCosts,
    Desktop,
}

/// One ordered writer owns the durable summaries for this HTTP workspace.
/// Source observers only enqueue; filesystem reads remain outside SQL callbacks.
pub(crate) async fn start(state: &HttpState) -> Result<(), StoreError> {
    let Some(store) = state.store.clone() else {
        return Ok(());
    };
    let runtime = tokio::runtime::Handle::current();
    let (sender, mut receiver) = mpsc::unbounded_channel();
    for table in [ObservedTable::Agents, ObservedTable::UsageResumes] {
        let sender = sender.clone();
        store.add_table_change_observer(table, move || {
            let _queued = sender.send(if table == ObservedTable::UsageResumes {
                Publication::Accounts
            } else {
                Publication::SessionCosts
            });
        });
    }
    let analytics = sender.clone();
    store.add_analytics_commit_observer(move || {
        let _queued = analytics.send(Publication::SessionCosts);
    });
    if let Some(driver) = &state.account_driver {
        let mut changes = driver.change_signal().subscribe();
        let sender = sender.clone();
        runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                for publication in [
                    Publication::Accounts,
                    Publication::Costs,
                    Publication::SessionCosts,
                ] {
                    if sender.send(publication).is_err() {
                        return;
                    }
                }
            }
        });
    }
    let mut auth = studio_runtime_services::account_driver::auth_status_changes();
    let auth_sender = sender.clone();
    runtime.spawn(async move {
        while auth.changed().await.is_ok() {
            if auth_sender.send(Publication::Accounts).is_err() {
                return;
            }
        }
    });
    let mut source = resource_notify::bounded_source_changes();
    let source_sender = sender.clone();
    runtime.spawn(async move {
        loop {
            let publication = match source.recv().await {
                Ok(BoundedSource::Desktop) => Publication::Desktop,
                Ok(BoundedSource::Costs) => {
                    let _queued = source_sender.send(Publication::SessionCosts);
                    Publication::Costs
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for publication in [Publication::Desktop, Publication::Costs] {
                        let _queued = source_sender.send(publication);
                    }
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if source_sender.send(publication).is_err() {
                return;
            }
        }
    });
    // Register the observers before seeding, and finish the initial projection
    // before the server admits requests. Router construction never owns writes.
    publish_accounts(state).await?;
    publish_costs(state).await?;
    publish_sessions(state).await?;
    publish_desktop(state).await?;
    let refresh_sender = sender.clone();
    runtime.spawn(async move {
        let mut ticks = tokio::time::interval(std::time::Duration::from_secs_f64(
            studio_runtime_services::usage::COST_REFRESH_SECONDS,
        ));
        ticks.tick().await;
        loop {
            ticks.tick().await;
            if refresh_sender.send(Publication::Costs).is_err() {
                return;
            }
        }
    });
    let state = state.clone();
    runtime.spawn(async move {
        while let Some(first) = receiver.recv().await {
            let mut pending = std::collections::BTreeSet::from([first]);
            while let Ok(next) = receiver.try_recv() {
                pending.insert(next);
            }
            for publication in pending {
                let result = match publication {
                    Publication::Accounts => publish_accounts(&state).await,
                    Publication::Costs => publish_costs(&state).await,
                    Publication::SessionCosts => publish_sessions(&state).await,
                    Publication::Desktop => publish_desktop(&state).await,
                };
                if let Err(error) = result {
                    eprintln!("Could not publish {publication:?} entities: {error}");
                }
            }
        }
    });
    Ok(())
}

async fn accounts(
    state: &HttpState,
) -> Result<studio_runtime_services::account_policy::AccountsSnapshot, StoreError> {
    let driver = state.account_driver.clone();
    let policy = state.account_policy.clone();
    let mut snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "bounded-entities/accounts",
        move || match driver {
            Some(driver) => driver
                .listing_snapshot()
                .map_err(|_| StoreError::RecordContract("accounts source unavailable")),
            None => policy
                .map(|policy| policy.accounts())
                .ok_or(StoreError::RecordContract("accounts source unavailable")),
        },
    )
    .await
    .map_err(|_| StoreError::ReadTaskFailed)??;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| StoreError::ReadTaskFailed)?
        .as_secs_f64();
    let waiting = state
        .store
        .as_ref()
        .ok_or(StoreError::ReadTaskFailed)?
        .read_async("bounded-entities/auth-recovery", move |tx| {
            studio_runtime_store::account_state::authentication_recovery_account_keys(tx, now)
        })
        .await?;
    studio_runtime_services::account_policy::apply_authentication_recovery(&mut snapshot, &waiting);
    Ok(snapshot)
}

async fn put(
    store: &Arc<RuntimeStore>,
    collection: &'static str,
    rows: Vec<(String, Value)>,
) -> Result<(), StoreError> {
    store
        .transaction_with(move |tx| {
            let ids = rows
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<std::collections::BTreeSet<_>>();
            for (id, value) in rows {
                let payload = json!({"collection":collection,"id":id,"value":value});
                if !studio_runtime_schema::validate_sync_entity_payload_strict(
                    collection, &id, false, &payload,
                )
                .map_err(StoreError::SchemaValidation)?
                {
                    return Err(StoreError::InvalidSyncObject {
                        scope: collection.into(),
                        id,
                    });
                }
                SyncDocumentsRepository::put_entity(tx, collection, &id, value, false)?;
            }
            let mut statement =
                tx.prepare("SELECT id FROM sync_entities WHERE collection=?1 AND deleted=0")?;
            let previous = statement
                .query_map([collection], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for id in previous {
                if !ids.contains(&id) {
                    SyncDocumentsRepository::put_entity(tx, collection, &id, Value::Null, true)?;
                }
            }
            Ok(())
        })
        .await
}

async fn publish_accounts(state: &HttpState) -> Result<(), StoreError> {
    let snapshot = accounts(state).await?;
    // Only this allowlisted DTO reaches sync_entities/RxDB. Login receipts,
    // URLs, device codes and credentials stay in the transient auth owner.
    let value = accounts_summary(snapshot);
    put(
        state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?,
        "accounts",
        vec![("current".into(), value)],
    )
    .await
}

fn accounts_summary(snapshot: studio_runtime_services::account_policy::AccountsSnapshot) -> Value {
    json!({"accounts":snapshot.accounts,"archivedAccounts":snapshot.archived_accounts,"defaultAccountKey":snapshot.default_account_key,"defaultAccountGroups":snapshot.default_account_groups,"supportsDelete":true,"supportsDisconnect":true})
}

async fn publish_costs(state: &HttpState) -> Result<(), StoreError> {
    let snapshot = accounts(state).await?;
    let root = std::path::PathBuf::from(&state.state_dir);
    let (rows, refreshes) = studio_runtime_process::blocking_pool::spawn_blocking(
        "bounded-entities/costs",
        move || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |time| time.as_secs_f64());
            let mut rows = Vec::new();
            let mut refreshes = Vec::new();
            for account in snapshot.accounts {
                let Some(key) = account.get("id").and_then(Value::as_str).map(str::to_owned) else {continue};
                let mut value = studio_runtime_services::usage::account_cost_snapshot(&root, &key, &account, now)
                    .unwrap_or_else(|error| json!({"accountKey":key,"at":null,"error":error.to_string(),"data":null,"refreshing":false,"stale":true}));
                let due = value.get("_refreshDue").and_then(Value::as_bool) == Some(true);
                if due && studio_runtime_services::usage::claim_cost_refresh_attempt(&key) {
                    refreshes.push((key.clone(), account));
                    value["refreshing"] = json!(true);
                }
                if let Some(fields) = value.as_object_mut() {fields.remove("_refreshDue");}
                rows.push((key, value));
            }
            (rows, refreshes)
        },
    ).await.map_err(|_| StoreError::ReadTaskFailed)?;
    put(
        state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?,
        "accountCost",
        rows,
    )
    .await?;
    for (key, account) in refreshes {
        let state_dir = std::path::PathBuf::from(&state.state_dir);
        let cancelled = state.cost_scan_cancellation_handle();
        let worker = state.track_cost_refresh_worker();
        tokio::spawn(async move {
            let _worker = worker;
            let result = studio_runtime_process::blocking_pool::spawn_blocking(
                "bounded-entities/cost-scan",
                move || {
                    studio_runtime_services::usage::refresh_account_costs_report(
                        &state_dir, &key, &account, &cancelled,
                    )
                },
            )
            .await;
            match result {
                Ok((_, Ok(())))
                | Ok((_, Err(studio_runtime_services::usage::UsageError::ScannerCancelled))) => {}
                Ok((_, Err(error))) => eprintln!("Local cost refresh failed: {error}"),
                Err(error) => eprintln!("Local cost refresh worker failed: {error}"),
            }
        });
    }
    Ok(())
}

async fn publish_sessions(state: &HttpState) -> Result<(), StoreError> {
    let store = state
        .store
        .as_ref()
        .ok_or(StoreError::ReadTaskFailed)?
        .clone();
    let ids=store.read_async("bounded-entities/session-roots",|tx| {
        let mut statement=tx.prepare("SELECT id FROM runtime_agents WHERE deleted_at IS NULL AND json_extract(record,'$.isLead')=1")?;
        Ok::<_,StoreError>(statement.query_map([],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
    }).await?;
    let profiles = accounts(state)
        .await
        .map(|snapshot| {
            snapshot
                .accounts
                .into_iter()
                .chain(snapshot.archived_accounts)
                .filter_map(|account| Some((account.get("id")?.as_str()?.to_owned(), account)))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let source = store.clone();
    let rows=studio_runtime_process::blocking_pool::spawn_blocking("bounded-entities/session-costs",move || ids.into_iter().map(|id| {
        let value=studio_runtime_services::costs::session_cost_with_account_profiles(&source,&id,&profiles).unwrap_or_else(|error|json!({"rootId":id,"totalUSD":null,"pricedSamples":0,"breakdown":{"models":{},"providers":{}},"unknownModels":[],"estimated":true,"pricingState":"loading","cacheAgeSeconds":0,"refreshing":false,"method":error.to_string()}));
        (id,value)
    }).collect()).await.map_err(|_|StoreError::ReadTaskFailed)?;
    put(&store, "sessionCost", rows).await
}

async fn publish_desktop(state: &HttpState) -> Result<(), StoreError> {
    let mut value = crate::routes::desktop::snapshot(state.clone())
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("backendKind");
    }
    put(
        state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?,
        "desktop",
        vec![("current".into(), value)],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state() -> (tempfile::TempDir, HttpState) {
        let root = tempfile::tempdir().expect("isolated source state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let policy=studio_runtime_services::account_policy::AccountPolicyState::from_startup(Some(br#"{"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":"/fixture","label":"Source account","source":"fixture","provider":"codex","status":"signedOut"}},"logins":{}}"#),[]).expect("policy");
        let state = HttpState::new(
            "token".into(),
            "workspace".into(),
            root.path().display().to_string(),
            4321,
            None,
            Some(store),
            Some(Arc::new(policy)),
        );
        (root, state)
    }

    fn entity(state: &HttpState, collection: &str, id: &str) -> Value {
        let collection = collection.to_owned();
        let id = id.to_owned();
        state
            .store
            .as_ref()
            .expect("store")
            .test_read_transaction(move |tx| {
                let raw: String = tx.query_row(
                    "SELECT payload FROM sync_entities WHERE collection=?1 AND id=?2 AND deleted=0",
                    [collection, id],
                    |row| row.get(0),
                )?;
                Ok(serde_json::from_str::<Value>(&raw)?["value"].clone())
            })
            .expect("published source entity")
    }

    #[tokio::test]
    async fn awaited_initialization_publishes_summaries_before_serving() {
        let (_root, state) = state().await;
        state
            .initialize_bounded_entities()
            .await
            .expect("source owners initialized");
        assert_eq!(
            entity(&state, "accounts", "current")["accounts"][0]["label"],
            "Source account"
        );
        assert_eq!(
            entity(&state, "desktop", "current")["application"],
            "codex-agents"
        );
    }

    #[tokio::test]
    async fn accounts_source_publishes_summary_without_private_login_material() {
        let (_root, state) = state().await;
        let mut snapshot = state.account_policy.as_ref().expect("policy").accounts();
        snapshot
            .logins
            .push(json!({"url":"https://login.invalid/private-only","deviceCode":"private-code"}));
        put(
            state.store.as_ref().expect("store"),
            "accounts",
            vec![("current".into(), accounts_summary(snapshot))],
        )
        .await
        .expect("accounts published");
        let value = entity(&state, "accounts", "current");
        assert_eq!(value["accounts"][0]["label"], "Source account");
        assert!(value.get("logins").is_none());
        assert!(!value.to_string().contains("private-only"));
    }

    #[tokio::test]
    async fn costs_source_publishes_per_account_unavailability_and_retires_removed_accounts() {
        let (_root, mut state) = state().await;
        publish_costs(&state).await.expect("costs published");
        let value = entity(&state, "accountCost", "default");
        assert_eq!(value["accountKey"], "default");
        assert!(value["error"].as_str().is_some());
        state.account_policy = Some(Arc::new(
            studio_runtime_services::account_policy::AccountPolicyState::from_startup(
                Some(br#"{"version":1,"defaultAccountKey":"default","accounts":{},"logins":{}}"#),
                [],
            )
            .expect("empty accounts"),
        ));
        publish_costs(&state)
            .await
            .expect("account removal published");
        assert!(state.store.as_ref().expect("store").test_read_transaction(|tx|Ok(tx.query_row("SELECT deleted FROM sync_entities WHERE collection='accountCost' AND id='default'",[],|row|row.get::<_,bool>(0))?)).expect("tombstone"));
    }

    #[tokio::test]
    async fn session_cost_source_publishes_ready_pricing_and_agent_removal() {
        let (root, state) = state().await;
        tokio::fs::create_dir_all(root.path().join("model-pricing"))
            .await
            .expect("pricing directory");
        tokio::fs::write(root.path().join("model-pricing/models-dev-v1.json"),json!({"version":1,"catalog":{"providers":{"openai":{"models":{"gpt-test":{"cost":{"input":1,"output":1}}}},"anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":1}}}}}}}).to_string()).await.expect("pricing source");
        let store = state.store.as_ref().expect("store");
        store.transaction(|tx|{tx.execute("INSERT INTO runtime_agents(id,record) VALUES('lead',?1)",[json!({"id":"lead","rootId":"lead","epoch":1,"isLead":true,"status":"idle","provider":"codex","model":"gpt-test","cwd":"/fixture","accountKey":"default"}).to_string()])?;Ok(())}).await.expect("chat source");
        publish_sessions(&state)
            .await
            .expect("session costs published");
        let value = entity(&state, "sessionCost", "lead");
        assert_eq!(value["rootId"], "lead");
        assert_eq!(value["pricingState"], "ready");
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_agents WHERE id='lead'", [])?;
                Ok(())
            })
            .await
            .expect("chat removed");
        publish_sessions(&state)
            .await
            .expect("session removal published");
        assert!(store.test_read_transaction(|tx|Ok(tx.query_row("SELECT deleted FROM sync_entities WHERE collection='sessionCost' AND id='lead'",[],|row|row.get::<_,bool>(0))?)).expect("tombstone"));
    }

    #[tokio::test]
    async fn desktop_source_publishes_a_structured_summary() {
        let (_root, state) = state().await;
        publish_desktop(&state).await.expect("desktop published");
        let value = entity(&state, "desktop", "current");
        assert_eq!(value["application"], "codex-agents");
        assert!(value["diskSpace"].is_object());
        assert!(value["publicOrigin"].is_null());
    }
}
