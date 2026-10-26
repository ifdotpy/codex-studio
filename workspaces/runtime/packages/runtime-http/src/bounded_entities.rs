//! Source-driven publication of bounded summaries. Stream reads never rebuild them.

use crate::router::HttpState;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use studio_runtime_services::resource_notify::{self, BoundedSource};
use studio_runtime_services::usage::PricingRefreshOutcome;
use studio_runtime_store::{ObservedTable, RuntimeStore, StoreError, SyncDocumentsRepository};
use tokio::sync::mpsc;

// Serialize cost snapshot publication and scan admission, never the scan itself.
// A late interest cannot overwrite a scan's completed entity with older data.
static COST_PUBLICATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Publication {
    Accounts,
    Costs,
    SessionCosts(Option<String>),
    AgentRows(Vec<i64>),
    Desktop,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PricingReadiness {
    Loading,
    Settled,
}

// One task per publisher may check the durable daily admission and fetch pricing.
// Keeping its permit across the blocking job prevents a burst of source commits
// from queuing more pricing jobs while a fetch is pending.
struct PricingRefresh<F> {
    state: HttpState,
    fetch: F,
    publications: mpsc::UnboundedSender<Publication>,
    single_flight: Arc<tokio::sync::Semaphore>,
    readiness: tokio::sync::watch::Sender<PricingReadiness>,
}

impl<F> PricingRefresh<F>
where
    F: Fn() -> Option<Value> + Clone + Send + Sync + 'static,
{
    fn new(state: HttpState, fetch: F, publications: mpsc::UnboundedSender<Publication>) -> Self {
        Self {
            state,
            fetch,
            publications,
            single_flight: Arc::new(tokio::sync::Semaphore::new(1)),
            readiness: tokio::sync::watch::channel(PricingReadiness::Loading).0,
        }
    }

    fn readiness(&self) -> PricingReadiness {
        *self.readiness.borrow()
    }

    fn request(&self) -> Option<tokio::task::JoinHandle<()>> {
        let permit = self.single_flight.clone().try_acquire_owned().ok()?;
        let state = self.state.clone();
        let fetch = self.fetch.clone();
        let publications = self.publications.clone();
        let worker = state.try_track_cost_refresh_worker()?;
        let readiness = self.readiness.clone();
        Some(tokio::spawn(async move {
            let _worker = worker;
            let _permit = permit;
            let outcome = refresh_session_pricing(&state, fetch).await;
            let first_check =
                readiness.send_replace(PricingReadiness::Settled) == PricingReadiness::Loading;
            let republish = match outcome {
                Ok(PricingRefreshOutcome::Changed) => true,
                // The initial missing-catalog seed must leave loading even after
                // a failed fetch or a check suppressed by the daily attempt TTL.
                Ok(PricingRefreshOutcome::Unavailable) => first_check,
                Ok(PricingRefreshOutcome::Unchanged) => false,
                Err(error) => {
                    eprintln!("Could not refresh session pricing: {error}");
                    first_check
                }
            };
            if republish {
                let _sent = publications.send(Publication::SessionCosts(None));
            }
        }))
    }
}

/// One ordered writer owns the durable summaries for this HTTP workspace.
/// Source observers only enqueue; filesystem reads remain outside SQL callbacks.
pub(crate) async fn start(state: &HttpState) -> Result<(), StoreError> {
    start_with_pricing_fetch(
        state,
        studio_runtime_services::usage::fetch_models_dev_catalog,
    )
    .await
    .map(|_| ())
}

async fn start_with_pricing_fetch<F>(
    state: &HttpState,
    fetch: F,
) -> Result<mpsc::UnboundedSender<Publication>, StoreError>
where
    F: Fn() -> Option<Value> + Clone + Send + Sync + 'static,
{
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let Some(store) = state.store.clone() else {
        return Ok(sender);
    };
    let runtime = tokio::runtime::Handle::current();
    let usage_sender = sender.clone();
    store.add_table_change_observer(ObservedTable::UsageResumes, move || {
        let _queued = usage_sender.send(Publication::Accounts);
    });
    let agents = sender.clone();
    store.add_table_row_change_observer(ObservedTable::Agents, move |change| {
        let _queued = agents.send(match change {
            studio_runtime_store::TableChange::Rows(rows) => Publication::AgentRows(rows.to_vec()),
            studio_runtime_store::TableChange::Full => Publication::SessionCosts(None),
        });
    });
    let analytics = sender.clone();
    store.add_analytics_root_change_observer(move |change| match change {
        studio_runtime_store::ChangedRoots::Roots(roots) => {
            for root in roots {
                let _queued = analytics.send(Publication::SessionCosts(Some(root)));
            }
        }
        studio_runtime_store::ChangedRoots::Full => {
            let _queued = analytics.send(Publication::SessionCosts(None));
        }
    });
    if let Some(driver) = &state.account_driver {
        let mut changes = driver.change_signal().subscribe();
        let sender = sender.clone();
        runtime.spawn(async move {
            while changes.changed().await.is_ok() {
                for publication in [
                    Publication::Accounts,
                    Publication::Costs,
                    Publication::SessionCosts(None),
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
                    let _queued = source_sender.send(Publication::SessionCosts(None));
                    Publication::Costs
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    for publication in [
                        Publication::Desktop,
                        Publication::Costs,
                        Publication::SessionCosts(None),
                    ] {
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
    // Register observers before seeding the saved summaries. Session estimates
    // can scan analytics and local journals, so only their placeholders belong
    // to HTTP startup. Router construction never owns writes.
    let pricing = PricingRefresh::new(state.clone(), fetch, sender.clone());
    let _refresh = pricing.request();
    let mut agent_roots = read_agent_roots(&store).await?;
    publish_accounts(state).await?;
    publish_costs(state).await?;
    seed_session_roots(state).await?;
    publish_desktop(state).await?;
    // Use the existing ordered owner for the first estimate as well as updates.
    // A pricing completion queued during seeding coalesces with this request.
    let _queued = sender.send(Publication::SessionCosts(None));
    let state = state.clone();
    runtime.spawn(async move {
        while let Some(first) = receiver.recv().await {
            let mut pending = std::collections::BTreeSet::from([first]);
            while let Ok(next) = receiver.try_recv() {
                pending.insert(next);
            }
            let mut roots = BTreeSet::new();
            let mut full = pending.contains(&Publication::SessionCosts(None));
            for publication in pending {
                let result = match publication {
                    Publication::Accounts => publish_accounts(&state).await,
                    Publication::Costs => publish_costs(&state).await,
                    Publication::SessionCosts(Some(root)) => {
                        roots.insert(root);
                        Ok(())
                    }
                    Publication::SessionCosts(None) => Ok(()),
                    Publication::AgentRows(rows) => {
                        match changed_agent_roots(&store, &mut agent_roots, rows).await {
                            Ok(changed) => {
                                roots.extend(changed);
                            }
                            Err(error) => {
                                full = true;
                                eprintln!("Could not resolve changed cost roots: {error}");
                            }
                        }
                        Ok(())
                    }
                    Publication::Desktop => publish_desktop(&state).await,
                };
                if let Err(error) = result {
                    eprintln!("Could not publish bounded entities: {error}");
                }
            }
            if full || !roots.is_empty() {
                let _refresh = pricing.request();
            }
            let result = if full {
                match read_agent_roots(&store).await {
                    Ok(current) => agent_roots = current,
                    Err(error) => eprintln!("Could not rebuild cost root identities: {error}"),
                }
                publish_session_roots(&state, None, pricing.readiness()).await
            } else if !roots.is_empty() {
                publish_session_roots(&state, Some(roots), pricing.readiness()).await
            } else {
                Ok(())
            };
            if let Err(error) = result {
                eprintln!("Could not publish session costs: {error}");
            }
        }
    });
    Ok(sender)
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

async fn write_entities(
    store: &Arc<RuntimeStore>,
    collection: &'static str,
    rows: Vec<(String, Value)>,
    deletions: Option<BTreeSet<String>>,
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
            let deleted = match deletions {
                Some(ids) => ids,
                None => {
                    let mut statement = tx.prepare(
                        "SELECT id FROM sync_entities WHERE collection=?1 AND deleted=0",
                    )?;
                    statement
                        .query_map([collection], |row| row.get::<_, String>(0))?
                        .collect::<rusqlite::Result<BTreeSet<_>>>()?
                        .difference(&ids)
                        .cloned()
                        .collect()
                }
            };
            for id in deleted {
                SyncDocumentsRepository::put_entity(tx, collection, &id, Value::Null, true)?;
            }
            Ok(())
        })
        .await
}

async fn put(
    store: &Arc<RuntimeStore>,
    collection: &'static str,
    rows: Vec<(String, Value)>,
) -> Result<(), StoreError> {
    write_entities(store, collection, rows, Some(BTreeSet::new())).await
}

async fn delete(
    store: &Arc<RuntimeStore>,
    collection: &'static str,
    ids: BTreeSet<String>,
) -> Result<(), StoreError> {
    write_entities(store, collection, Vec::new(), Some(ids)).await
}

async fn replace(
    store: &Arc<RuntimeStore>,
    collection: &'static str,
    rows: Vec<(String, Value)>,
) -> Result<(), StoreError> {
    write_entities(store, collection, rows, None).await
}

async fn read_agent_roots(store: &Arc<RuntimeStore>) -> Result<BTreeMap<i64, String>, StoreError> {
    store
        .read_async("bounded-entities/agent-roots", |tx| {
            let mut statement = tx.prepare(
                "SELECT rowid, COALESCE(json_extract(record,'$.rootId'),id) FROM runtime_agents",
            )?;
            Ok(statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
}

async fn changed_agent_roots(
    store: &Arc<RuntimeStore>,
    previous: &mut BTreeMap<i64, String>,
    rows: Vec<i64>,
) -> Result<BTreeSet<String>, StoreError> {
    let current = store.read_async("bounded-entities/changed-agent-roots", move |tx| {
        let mut statement = tx.prepare("SELECT COALESCE(json_extract(record,'$.rootId'),id) FROM runtime_agents WHERE rowid=?1")?;
        rows.into_iter().map(|rowid| Ok((rowid, statement.query_row([rowid], |row| row.get::<_, String>(0)).optional()?))).collect::<Result<Vec<_>, StoreError>>()
    }).await?;
    let mut roots = BTreeSet::new();
    for (rowid, root) in current {
        if let Some(old) = previous.remove(&rowid) {
            roots.insert(old);
        }
        if let Some(root) = root {
            roots.insert(root.clone());
            previous.insert(rowid, root);
        }
    }
    Ok(roots)
}

async fn publish_accounts(state: &HttpState) -> Result<(), StoreError> {
    let snapshot = accounts(state).await?;
    // Only this allowlisted DTO reaches sync_entities/RxDB. Login receipts,
    // URLs, device codes and credentials stay in the transient auth owner.
    let value = accounts_summary(snapshot);
    replace(
        state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?,
        "accounts",
        vec![("current".into(), value)],
    )
    .await
}

fn accounts_summary(snapshot: studio_runtime_services::account_policy::AccountsSnapshot) -> Value {
    json!({"accounts":snapshot.accounts,"archivedAccounts":snapshot.archived_accounts,"defaultAccountKey":snapshot.default_account_key,"defaultAccountGroups":snapshot.default_account_groups,"supportsDelete":true,"supportsDisconnect":true})
}

fn cost_summary(value: Value) -> Result<Value, StoreError> {
    // The scanner cache carries private bookkeeping. Its generated public DTO
    // owns the field allowlist, as it does for the explicit HTTP cost read.
    let summary: crate::generated::AccountCostEntityDto = serde_json::from_value(value)?;
    Ok(serde_json::to_value(summary)?)
}

fn saved_cost(state_dir: &std::path::Path, key: &str, account: &Value) -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |time| time.as_secs_f64());
    studio_runtime_services::usage::account_cost_snapshot(state_dir, key, account, now)
        .unwrap_or_else(|error| json!({"accountKey":key,"at":null,"error":error.to_string(),"data":null,"refreshing":false,"stale":true}))
}

async fn publish_costs(state: &HttpState) -> Result<(), StoreError> {
    let _publication = COST_PUBLICATION.lock().await;
    let snapshot = accounts(state).await?;
    let root = std::path::PathBuf::from(&state.state_dir);
    let rows = studio_runtime_process::blocking_pool::spawn_blocking(
        "bounded-entities/costs",
        move || {
            snapshot
                .accounts
                .into_iter()
                .filter_map(|account| {
                    let key = account.get("id")?.as_str()?.to_owned();
                    Some(cost_summary(saved_cost(&root, &key, &account)).map(|value| (key, value)))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        },
    )
    .await
    .map_err(|_| StoreError::ReadTaskFailed)??;
    replace(
        state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?,
        "accountCost",
        rows,
    )
    .await
}

/// A window-open interest admits work; publication and stream reads never do.
/// An existing reservation joins its scan and receives the same entity result.
pub(crate) async fn request_cost_refresh(state: &HttpState, key: &str) -> Result<(), StoreError> {
    let worker = state
        .try_track_cost_refresh_worker()
        .ok_or(StoreError::ReadTaskFailed)?;
    let _publication = COST_PUBLICATION.lock().await;
    let account = accounts(state)
        .await?
        .accounts
        .into_iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(key))
        .ok_or(StoreError::RecordContract("unknown cost account"))?;
    let root = std::path::PathBuf::from(&state.state_dir);
    let key = key.to_owned();
    let (account, key, value, reservation) = studio_runtime_process::blocking_pool::spawn_blocking(
        "bounded-entities/cost-interest",
        move || {
            let mut value = saved_cost(&root, &key, &account);
            let due = value.get("_refreshDue").and_then(Value::as_bool) == Some(true);
            let reservation = if due {
                studio_runtime_services::usage::reserve_account_cost_refresh(&root, &key, &account)
            } else {
                None
            };
            if reservation.is_some() {
                value["refreshing"] = json!(true);
            }
            Ok::<_, StoreError>((account, key, cost_summary(value)?, reservation))
        },
    )
    .await
    .map_err(|_| StoreError::ReadTaskFailed)??;
    let store = state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?;
    put(store, "accountCost", vec![(key.clone(), value)]).await?;
    let Some(reservation) = reservation else {
        return Ok(());
    };
    let state = state.clone();
    let cancelled = state.cost_scan_cancellation_handle();
    tokio::spawn(async move {
        let _worker = worker;
        let root = std::path::PathBuf::from(&state.state_dir);
        let scan_key = key.clone();
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "bounded-entities/cost-scan",
            move || {
                let (_, outcome) =
                    studio_runtime_services::usage::refresh_reserved_account_costs_report(
                        &root,
                        &scan_key,
                        &account,
                        &cancelled,
                        reservation,
                    );
                (root, scan_key, account, outcome)
            },
        )
        .await;
        let _publication = COST_PUBLICATION.lock().await;
        let result = match result {
            Ok((root, scan_key, account, outcome)) => {
                let current = match accounts(&state).await {
                    Ok(snapshot) => snapshot
                        .accounts
                        .into_iter()
                        .find(|value| value.get("id").and_then(Value::as_str) == Some(&scan_key)),
                    Err(error) => {
                        eprintln!("Could not resolve refreshed account: {error}");
                        if let Err(error) = settle_cost_worker_failure(&state, &key).await {
                            eprintln!("Could not settle refreshed account: {error}");
                        }
                        return;
                    }
                };
                let Some(current) = current else {
                    if let Some(store) = &state.store
                        && let Err(error) =
                            delete(store, "accountCost", BTreeSet::from([key])).await
                    {
                        eprintln!("Could not retire removed cost account: {error}");
                    }
                    return;
                };
                let outcome = if current.get("home") == account.get("home")
                    && current.get("accountId") == account.get("accountId")
                {
                    outcome
                } else {
                    Ok(())
                };
                let account = current;
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "bounded-entities/cost-result",
                    move || {
                        let mut value = saved_cost(&root, &scan_key, &account);
                        if let Err(error) = outcome {
                            value["error"] = json!(error.to_string());
                            value["stale"] = json!(true);
                        }
                        value["refreshing"] = json!(false);
                        cost_summary(value)
                    },
                )
                .await
            }
            Err(error) => Err(error),
        };
        let value = match result {
            Ok(Ok(value)) => value,
            error => {
                eprintln!("Local cost refresh worker failed: {error:?}");
                // Keep the durable saved data and settle its refresh state.
                if let Err(error) = settle_cost_worker_failure(&state, &key).await {
                    eprintln!("Could not settle local cost refresh: {error}");
                }
                return;
            }
        };
        if let Some(store) = &state.store
            && let Err(error) = put(store, "accountCost", vec![(key, value)]).await
        {
            eprintln!("Could not publish local cost refresh: {error}");
        }
    });
    Ok(())
}

async fn settle_cost_worker_failure(state: &HttpState, key: &str) -> Result<(), StoreError> {
    let key = key.to_owned();
    state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?.transaction_with(move |tx| {
        let raw: String = tx.query_row("SELECT payload FROM sync_entities WHERE collection='accountCost' AND id=?1 AND deleted=0", [&key], |row| row.get(0))?;
        let mut value: Value = serde_json::from_str::<Value>(&raw)?["value"].clone();
        value["refreshing"] = json!(false);
        value["stale"] = json!(true);
        value["error"] = json!("Local cost refresh worker failed");
        SyncDocumentsRepository::put_entity(tx, "accountCost", &key, value, false)?;
        Ok(())
    }).await
}

// Fetching is bounded by the catalog owner's request timeout and daily attempt TTL.
// Both startup (even without teams) and source-driven session publication use this path.
async fn refresh_session_pricing<F>(
    state: &HttpState,
    fetch: F,
) -> Result<PricingRefreshOutcome, StoreError>
where
    F: FnOnce() -> Option<Value> + Send + 'static,
{
    let root = std::path::PathBuf::from(&state.state_dir);
    studio_runtime_process::blocking_pool::spawn_blocking(
        "bounded-entities/pricing-refresh",
        move || {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |time| time.as_secs_f64());
            studio_runtime_services::usage::refresh_pricing_with_fetch(&root, now, fetch)
        },
    )
    .await
    .map_err(|_| StoreError::ReadTaskFailed)
}

#[cfg(test)]
async fn publish_sessions(state: &HttpState) -> Result<(), StoreError> {
    publish_session_roots(state, None, PricingReadiness::Settled).await
}

#[cfg(test)]
async fn publish_session_roots_with_fetch<F>(
    state: &HttpState,
    roots: Option<BTreeSet<String>>,
    fetch: F,
) -> Result<(), StoreError>
where
    F: FnOnce() -> Option<Value> + Send + 'static,
{
    let _changed = refresh_session_pricing(state, fetch).await?;
    publish_session_roots(state, roots, PricingReadiness::Settled).await
}

/// Preserve saved estimates and seed absent roots without opening analytics.
/// The ordered publisher replaces pending values after HTTP service can start.
async fn seed_session_roots(state: &HttpState) -> Result<(), StoreError> {
    let store = state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?;
    let rows = store
        .read_async("bounded-entities/session-cost-seed", |tx| {
            let mut statement = tx.prepare(
                "SELECT a.id,s.payload FROM runtime_agents a
             LEFT JOIN sync_entities s ON s.collection='sessionCost' AND s.id=a.id AND s.deleted=0
             WHERE a.deleted_at IS NULL AND json_extract(a.record,'$.isLead')=1",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })?
                .map(|row| {
                    let (id, saved) = row?;
                    let value = match saved {
                        Some(saved) => serde_json::from_str::<Value>(&saved)?["value"].clone(),
                        None => json!({"rootId":id,"totalUSD":null,"pricedSamples":0,
                        "breakdown":{"models":{},"providers":{}},"unknownModels":[],
                        "estimated":true,"pricingState":"loading","cacheAgeSeconds":0,
                        "refreshing":false,"method":"Session cost estimate is pending"}),
                    };
                    Ok((id, value))
                })
                .collect::<Result<Vec<_>, StoreError>>()
        })
        .await?;
    replace(store, "sessionCost", rows).await
}

async fn publish_session_roots(
    state: &HttpState,
    roots: Option<BTreeSet<String>>,
    readiness: PricingReadiness,
) -> Result<(), StoreError> {
    let store = state
        .store
        .as_ref()
        .ok_or(StoreError::ReadTaskFailed)?
        .clone();
    let selected = roots.clone();
    let ids = store.read_async("bounded-entities/session-roots", move |tx| {
        if let Some(roots) = selected {
            let mut statement = tx.prepare("SELECT id FROM runtime_agents WHERE id=?1 AND deleted_at IS NULL AND json_extract(record,'$.isLead')=1")?;
            let mut ids = Vec::new();
            for root in roots {
                if let Some(id) = statement.query_row([root], |row| row.get::<_, String>(0)).optional()? { ids.push(id); }
            }
            Ok::<_, StoreError>(ids)
        } else {
            let mut statement = tx.prepare("SELECT id FROM runtime_agents WHERE deleted_at IS NULL AND json_extract(record,'$.isLead')=1")?;
            Ok(statement.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
        }
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
    let rows=studio_runtime_process::blocking_pool::spawn_blocking("bounded-entities/session-costs",move || {
        #[cfg(test)]
        tests::wait_for_session_cost_gate(source.root());
        ids.into_iter().map(|id| {
        let value=studio_runtime_services::costs::session_cost_with_account_profiles(&source,&id,&profiles).unwrap_or_else(|error| {
            // A completed pricing attempt with no usable catalog is an unavailable
            // estimate (ready + null total), rather than work still loading.
            let pricing_state = match error {
                studio_runtime_services::costs::CostsError::Dependency(_)
                | studio_runtime_services::costs::CostsError::InvalidPricing => match readiness {
                    PricingReadiness::Loading => "loading",
                    PricingReadiness::Settled => "ready",
                },
                _ => "loading",
            };
            json!({"rootId":id,"totalUSD":null,"pricedSamples":0,"breakdown":{"models":{},"providers":{}},"unknownModels":[],"estimated":true,"pricingState":pricing_state,"cacheAgeSeconds":0,"refreshing":false,"method":error.to_string()})});
        (id,value)
    }).collect()}).await.map_err(|_|StoreError::ReadTaskFailed)?;
    match roots {
        None => replace(&store, "sessionCost", rows).await,
        Some(roots) => {
            let present = rows
                .iter()
                .map(|(id, _)| id.clone())
                .collect::<BTreeSet<_>>();
            put(&store, "sessionCost", rows).await?;
            delete(
                &store,
                "sessionCost",
                roots.difference(&present).cloned().collect(),
            )
            .await
        }
    }
}

async fn publish_desktop(state: &HttpState) -> Result<(), StoreError> {
    let mut value = crate::routes::desktop::snapshot(state.clone())
        .await
        .map_err(|_| StoreError::ReadTaskFailed)?;
    if let Some(fields) = value.as_object_mut() {
        fields.remove("backendKind");
    }
    replace(
        state.store.as_ref().ok_or(StoreError::ReadTaskFailed)?,
        "desktop",
        vec![("current".into(), value)],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    type SessionCostGate = (Arc<tokio::sync::Notify>, std::sync::mpsc::Receiver<()>);
    static SESSION_COST_GATES: std::sync::LazyLock<
        std::sync::Mutex<BTreeMap<std::path::PathBuf, SessionCostGate>>,
    > = std::sync::LazyLock::new(Default::default);

    pub(super) fn wait_for_session_cost_gate(root: &std::path::Path) {
        let gate = SESSION_COST_GATES.lock().expect("cost gates").remove(root);
        if let Some((started, release)) = gate {
            started.notify_one();
            release
                .recv_timeout(std::time::Duration::from_secs(15))
                .expect("release session cost computation");
        }
    }

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
        let pricing = root.path().join("model-pricing");
        tokio::fs::create_dir_all(&pricing)
            .await
            .expect("pricing directory");
        tokio::fs::write(pricing.join("refresh-state.json"),
            json!({"lastAttemptAt":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("time").as_secs_f64()}).to_string()
        ).await.expect("fixture suppresses upstream pricing requests");
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

    fn pricing_providers() -> Value {
        json!({
            "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":2}}}},
            "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":2}}}}
        })
    }

    async fn pricing_team(state: &HttpState) {
        tokio::fs::remove_file(
            std::path::Path::new(&state.state_dir).join("model-pricing/refresh-state.json"),
        )
        .await
        .expect("enable injected refresh");
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('pricing-team',?1)",
                    [
                        json!({"id":"pricing-team","rootId":"pricing-team","isLead":true})
                            .to_string(),
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("team");
    }

    #[tokio::test]
    async fn startup_pricing_refresh_makes_session_ready_without_account_interest() {
        let (root, state) = state().await;
        pricing_team(&state).await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetched = calls.clone();
        let mut changes = commits(&state);
        start_with_pricing_fetch(&state, move || {
            fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(pricing_providers())
        })
        .await
        .expect("startup");
        wait_for_entity(
            &state,
            &mut changes,
            "sessionCost",
            "pricing-team",
            |value| value["pricingState"] == "ready",
        )
        .await
        .expect("startup pricing completion");
        assert_eq!(
            entity(&state, "sessionCost", "pricing-team")["pricingState"],
            "ready"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(
            !tokio::fs::try_exists(root.path().join("account-costs"))
                .await
                .expect("account cache metadata"),
            "no account scan"
        );
        state
            .shutdown_services()
            .await
            .expect("fixture refresh tasks drained");
    }

    #[tokio::test]
    async fn session_pricing_refresh_reuses_fresh_artifact_and_refreshes_expired_artifact() {
        let (root, state) = state().await;
        pricing_team(&state).await;
        let directory = root.path().join("model-pricing");
        tokio::fs::create_dir_all(&directory)
            .await
            .expect("pricing directory");
        let artifact = directory.join("models-dev-v1.json");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_secs_f64();
        let mut value =
            json!({"version":1,"fetchedAtEpoch":now,"catalog":{"providers":pricing_providers()}});
        tokio::fs::write(&artifact, value.to_string())
            .await
            .expect("fresh artifact");
        publish_session_roots_with_fetch(&state, None, || panic!("fresh catalog must not fetch"))
            .await
            .expect("fresh publication");
        assert_eq!(
            entity(&state, "sessionCost", "pricing-team")["pricingState"],
            "ready"
        );
        value["fetchedAtEpoch"] = json!(1);
        tokio::fs::write(&artifact, value.to_string())
            .await
            .expect("expired artifact");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetched = calls.clone();
        publish_session_roots_with_fetch(&state, None, move || {
            fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(pricing_providers())
        })
        .await
        .expect("expired publication");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        publish_session_roots_with_fetch(&state, None, || {
            panic!("refreshed catalog must not fetch")
        })
        .await
        .expect("repeated publication");
    }

    #[tokio::test]
    async fn failed_session_pricing_refresh_respects_attempt_ttl() {
        let (root, state) = state().await;
        pricing_team(&state).await;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetched = calls.clone();
        publish_session_roots_with_fetch(&state, None, move || {
            fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        })
        .await
        .expect("failed fetch publication");
        assert_eq!(
            entity(&state, "sessionCost", "pricing-team")["pricingState"],
            "ready"
        );
        assert!(entity(&state, "sessionCost", "pricing-team")["totalUSD"].is_null());
        publish_session_roots_with_fetch(&state, None, || {
            panic!("failed attempt must keep its TTL")
        })
        .await
        .expect("attempt TTL publication");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(
            tokio::fs::try_exists(root.path().join("model-pricing/refresh-state.json"))
                .await
                .expect("attempt metadata")
        );
    }

    fn commits(state: &HttpState) -> mpsc::UnboundedReceiver<()> {
        let (sender, receiver) = mpsc::unbounded_channel();
        state
            .store
            .as_ref()
            .expect("store")
            .add_commit_observer(move || {
                let _sent = sender.send(());
            });
        receiver
    }

    async fn wait_for_entity(
        state: &HttpState,
        changes: &mut mpsc::UnboundedReceiver<()>,
        collection: &str,
        id: &str,
        matches: impl Fn(&Value) -> bool,
    ) -> Result<Value, tokio::time::error::Elapsed> {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let value = entity(state, collection, id);
                if matches(&value) {
                    return value;
                }
                changes.recv().await.expect("commit observer");
            }
        })
        .await
    }

    async fn pricing_roots(state: &HttpState) {
        state
            .store
            .as_ref()
            .expect("store")
            .transaction(|tx| {
                for id in ["a", "b"] {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        [id, &json!({"id":id,"rootId":id,"isLead":true}).to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("pricing roots");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_pricing_refresh_seeds_while_fetch_is_pending_then_republishes() {
        for response in [Some(pricing_providers()), None] {
            let (root, state) = state().await;
            pricing_roots(&state).await;
            tokio::fs::remove_file(root.path().join("model-pricing/refresh-state.json"))
                .await
                .expect("missing pricing at startup");
            let (release, blocked) = std::sync::mpsc::channel();
            let blocked = Arc::new(std::sync::Mutex::new(blocked));
            let mut changes = commits(&state);
            let seeded = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                start_with_pricing_fetch(&state, move || {
                    blocked
                        .lock()
                        .expect("release receiver")
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .expect("release startup fetch");
                    response.clone()
                }),
            )
            .await;
            if seeded.is_ok() {
                assert_eq!(
                    entity(&state, "sessionCost", "b")["pricingState"],
                    "loading"
                );
                assert_eq!(
                    entity(&state, "desktop", "current")["application"],
                    "codex-agents"
                );
            }
            release.send(()).expect("finish startup fetch");
            seeded
                .expect("startup must finish seeding while the pricing fetch is gated")
                .expect("seeded source publisher");
            wait_for_entity(&state, &mut changes, "sessionCost", "b", |value| {
                value["pricingState"] == "ready"
            })
            .await
            .expect("refresh completion reprices the seeded roots");
            state
                .shutdown_services()
                .await
                .expect("startup refresh settled");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pricing_refresh_pending_does_not_block_unrelated_publication() {
        let (root, state) = state().await;
        pricing_roots(&state).await;
        let started = Arc::new(tokio::sync::Notify::new());
        let fetch_started = started.clone();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Arc::new(std::sync::Mutex::new(blocked));
        let sender = start_with_pricing_fetch(&state, move || {
            fetch_started.notify_one();
            blocked
                .lock()
                .expect("release receiver")
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("release fetch");
            Some(pricing_providers())
        })
        .await
        .expect("startup skips recent attempt");
        tokio::fs::remove_file(root.path().join("model-pricing/refresh-state.json"))
            .await
            .expect("expire attempt");
        let mut changes = commits(&state);
        sender
            .send(Publication::SessionCosts(Some("a".into())))
            .expect("request session");
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .expect("fetch started");
        let mut desktop = entity(&state, "desktop", "current");
        desktop["application"] = json!("pending-refresh-witness");
        put(
            state.store.as_ref().expect("store"),
            "desktop",
            vec![("current".into(), desktop)],
        )
        .await
        .expect("desktop marker");
        sender
            .send(Publication::Desktop)
            .expect("unrelated desktop publication");
        let published = wait_for_entity(&state, &mut changes, "desktop", "current", |value| {
            value["application"] == "codex-agents"
        })
        .await;
        release.send(()).expect("finish fetch");
        state
            .shutdown_services()
            .await
            .expect("pricing task settled");
        assert!(
            published.is_ok(),
            "the ordered publisher must continue while pricing fetch is blocked"
        );
    }

    #[tokio::test]
    async fn pricing_refresh_triggered_by_one_root_republishes_other_roots() {
        let (root, state) = state().await;
        pricing_roots(&state).await;
        let sender = start_with_pricing_fetch(&state, || Some(pricing_providers()))
            .await
            .expect("startup skips recent attempt");
        let unavailable = entity(&state, "sessionCost", "b")["method"].clone();
        tokio::fs::remove_file(root.path().join("model-pricing/refresh-state.json"))
            .await
            .expect("expire attempt");
        let mut changes = commits(&state);
        sender
            .send(Publication::SessionCosts(Some("a".into())))
            .expect("only a changes");
        wait_for_entity(&state, &mut changes, "sessionCost", "b", |value| {
            value["method"] != unavailable
        })
        .await
        .expect("new catalog republishes inactive root b");
        state
            .shutdown_services()
            .await
            .expect("fixture refresh tasks drained");
    }

    #[tokio::test]
    async fn pricing_refresh_unchanged_catalog_does_not_request_full_republication() {
        let (root, state) = state().await;
        pricing_team(&state).await;
        assert_eq!(
            refresh_session_pricing(&state, || Some(pricing_providers()))
                .await
                .expect("initial catalog"),
            PricingRefreshOutcome::Changed
        );
        let artifact = root.path().join("model-pricing/models-dev-v1.json");
        let mut saved: Value =
            serde_json::from_slice(&tokio::fs::read(&artifact).await.expect("artifact"))
                .expect("artifact JSON");
        saved["lastAttemptAt"] = json!(1);
        saved["fetchedAtEpoch"] = json!(1);
        tokio::fs::write(&artifact, saved.to_string())
            .await
            .expect("stale artifact");
        let (sender, mut publications) = mpsc::unbounded_channel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetched = calls.clone();
        let pricing = PricingRefresh::new(
            state.clone(),
            move || {
                fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(pricing_providers())
            },
            sender,
        );
        pricing
            .request()
            .expect("admitted refresh")
            .await
            .expect("completed refresh");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(
            matches!(
                publications.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "unchanged pricing must not invalidate any session roots"
        );
        pricing
            .request()
            .expect("TTL check")
            .await
            .expect("completed TTL check");
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(matches!(
            publications.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pricing_refresh_shutdown_closes_admission_before_draining() {
        let (_root, state) = state().await;
        pricing_team(&state).await;
        let started = Arc::new(tokio::sync::Notify::new());
        let fetch_started = started.clone();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocked = Arc::new(std::sync::Mutex::new(blocked));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetched = calls.clone();
        let (sender, _publications) = mpsc::unbounded_channel();
        let pricing = PricingRefresh::new(
            state.clone(),
            move || {
                fetched.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                fetch_started.notify_one();
                blocked
                    .lock()
                    .expect("release receiver")
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("release shutdown fetch");
                Some(pricing_providers())
            },
            sender.clone(),
        );
        let active = pricing.request().expect("pre-shutdown admission");
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .expect("fetch gated");
        let mut shutdown = Box::pin(state.shutdown_services());
        assert!(
            futures_util::poll!(shutdown.as_mut()).is_pending(),
            "shutdown waits for the gated refresh"
        );
        // A second publisher has a free single-flight permit. It still cannot
        // admit work once shutdown has closed the shared worker owner.
        let late = PricingRefresh::new(state.clone(), || Some(pricing_providers()), sender);
        let admitted_after_close = late.request();
        let rejected = admitted_after_close.is_none();
        release.send(()).expect("finish in-flight fetch");
        active.await.expect("in-flight task finished");
        if let Some(task) = admitted_after_close {
            task.await.expect("drain old admission behavior");
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), shutdown)
            .await
            .expect("shutdown bounded")
            .expect("shutdown drained pricing tasks");
        assert!(
            rejected,
            "a request arriving after shutdown begins must not start a task"
        );
        assert!(
            pricing.request().is_none(),
            "completion events cannot reopen admission after the drain"
        );
        assert!(
            late.request().is_none(),
            "a free permit cannot reopen closed worker admission"
        );
        assert_eq!(
            pricing.single_flight.available_permits(),
            1,
            "no pricing task remains alive"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(
            request_cost_refresh(&state, "default").await.is_err(),
            "account interest shares closed worker admission"
        );
    }

    #[tokio::test]
    async fn costs_publication_does_not_admit_a_scan_without_window_interest() {
        let (root, mut state) = state().await;
        let home = root.path().join("claude");
        state.account_policy = Some(Arc::new(
            studio_runtime_services::account_policy::AccountPolicyState::from_startup(
                Some(json!({"version":1,"defaultAccountKey":"on-demand","accounts":{"on-demand":{"id":"on-demand","home":home,"label":"On demand","source":"fixture","provider":"claude","status":"ready"}},"logins":{}}).to_string().as_bytes()), [],
            ).expect("ready account"),
        ));
        state
            .cost_scan_cancellation_handle()
            .store(true, std::sync::atomic::Ordering::Release);
        publish_costs(&state).await.expect("saved cost publication");
        assert_eq!(
            entity(&state, "accountCost", "on-demand")["refreshing"],
            false
        );
    }

    #[tokio::test]
    async fn partial_upsert_keeps_other_teams_until_explicit_deletion() {
        let (_root, state) = state().await;
        let store = state.store.as_ref().expect("store");
        let row = |id: &str| {
            (
                id.to_owned(),
                json!({"accountKey":id,"at":null,"error":null,"data":null,"refreshing":false,"stale":true}),
            )
        };
        put(store, "accountCost", vec![row("a"), row("b")])
            .await
            .expect("initial rows");
        put(store, "accountCost", vec![row("a")])
            .await
            .expect("partial update");
        assert_eq!(entity(&state, "accountCost", "b")["accountKey"], "b");
    }

    #[tokio::test]
    async fn analytics_cost_roots_notify_after_commit_but_never_after_rollback() {
        use studio_runtime_store::ChangedRoots;
        use studio_runtime_store::analytics_capture::AnalyticsCaptureRepository as Repo;
        let (_root, state) = state().await;
        let store = state.store.as_ref().expect("store");
        let changes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = changes.clone();
        store.add_analytics_root_change_observer(move |roots| {
            recorded.lock().expect("events").push(roots)
        });
        store
            .analytics_transaction(|tx, _| Repo::generation(tx, &["a".into(), "b".into()]))
            .await
            .expect("commit");
        assert_eq!(
            *changes.lock().expect("events"),
            vec![ChangedRoots::Roots(BTreeSet::from([
                "a".into(),
                "b".into()
            ]))]
        );
        changes.lock().expect("events").clear();
        let failed = store
            .analytics_projection_transaction(|tx| {
                Repo::generation(tx, &["rolled-back".into()])?;
                Err::<(), _>(StoreError::RecordContract("rollback witness"))
            })
            .await;
        assert!(failed.is_err());
        assert!(changes.lock().expect("events").is_empty());
        store
            .analytics_projection_transaction(|tx| Repo::generation(tx, &["a".into()]))
            .await
            .expect("projection commit");
        assert_eq!(
            *changes.lock().expect("events"),
            vec![ChangedRoots::Roots(BTreeSet::from(["a".into()]))]
        );
        changes.lock().expect("events").clear();
        store
            .analytics_projection_transaction(|tx| {
                tx.execute("DELETE FROM analytics_usage_roots WHERE root='a'", [])?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("deleted root commits");
        assert_eq!(*changes.lock().expect("events"), vec![ChangedRoots::Full]);
    }

    #[tokio::test]
    async fn mixed_analytics_metadata_batch_keeps_unknown_cost_scope_full() {
        use studio_runtime_store::ChangedRoots;
        use studio_runtime_store::analytics_capture::AnalyticsCaptureRepository as Repo;
        let (_root, state) = state().await;
        let store = state.store.as_ref().expect("store");
        let changes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = changes.clone();
        store.add_analytics_root_change_observer(move |scope| {
            recorded.lock().expect("events").push(scope)
        });
        store.analytics_projection_transaction(|tx| {
            Repo::generation(tx, &["a".into()])?;
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('b',?1)", [json!({"id":"b","rootId":"b","provider":"claude","accountKey":"changed-profile"}).to_string()])?;
            Ok::<_, StoreError>(())
        }).await.expect("mixed batch commits");
        assert_eq!(*changes.lock().expect("events"), vec![ChangedRoots::Full]);
    }

    #[tokio::test]
    async fn cost_interest_http_dispatches_concurrent_interests_and_streams_real_completion() {
        use axum::{
            body::Body,
            extract::ConnectInfo,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        let (root, mut state) = state().await;
        let home = root.path().join("profile");
        let account = json!({"id":"default","home":home,"label":"Fixture","source":"fixture","provider":"claude","status":"ready"});
        state.account_policy = Some(Arc::new(studio_runtime_services::account_policy::AccountPolicyState::from_startup(
            Some(json!({"version":1,"defaultAccountKey":"default","accounts":{"default":account},"logins":{}}).to_string().as_bytes()), []
        ).expect("ready account")));
        let cache =
            root.path()
                .join("account-costs/claude")
                .join(studio_runtime_store::sha256_hex(
                    format!("default{}", home.display()).as_bytes(),
                ));
        tokio::fs::create_dir_all(&cache)
            .await
            .expect("cache directory");
        tokio::fs::write(
            cache.join("claude-costs.json"),
            json!({"config":home,"checked":1,"error":null,"data":{"todayUSD":2,"last30DaysUSD":3}})
                .to_string(),
        )
        .await
        .expect("saved estimate");
        use futures_util::StreamExt;
        publish_costs(&state).await.expect("saved baseline");
        let app = crate::router::router(state.clone()).expect("router");
        let request = |method: &str, uri: &str, body: Body| {
            Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:4321")
                .header("content-type", "application/json")
                .header("x-canvas-token", "token")
                .header(
                    crate::schema::API_SCHEMA_HASH_HEADER,
                    crate::schema::api_schema_hash(),
                )
                .extension(ConnectInfo(
                    "127.0.0.1:12345"
                        .parse::<std::net::SocketAddr>()
                        .expect("peer"),
                ))
                .body(body)
                .expect("request")
        };
        let response = app.clone().oneshot(request("GET", &format!("/api/sync/stream?protocol={}&resources=%5B%7B%22kind%22%3A%22costs%22%7D%5D&tokenRates=false", studio_runtime_schema::SYNC_PROTOCOL_VERSION_TEXT), Body::empty())).await.expect("cost stream");
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.into_body().into_data_stream();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.expect("initial stream bytes");
            if String::from_utf8_lossy(&bytes).contains("event: resources") {
                break;
            }
        }
        let interest = || {
            request(
                "POST",
                "/api/costs/interest",
                Body::from(r#"{"account_key":"default"}"#),
            )
        };
        let (first, second) = tokio::join!(
            app.clone().oneshot(interest()),
            app.clone().oneshot(interest())
        );
        assert_eq!(first.expect("first interest").status(), StatusCode::OK);
        assert_eq!(
            second.expect("concurrent joining interest").status(),
            StatusCode::OK
        );
        let saved = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(chunk) = stream.next().await {
                let bytes = chunk.expect("live stream bytes");
                let text = String::from_utf8_lossy(&bytes);
                if !text.contains("event: resources") {
                    continue;
                }
                let data = text
                    .lines()
                    .find_map(|line| line.strip_prefix("data: "))
                    .expect("event data");
                let event: Value = serde_json::from_str(data).expect("resource event");
                for entry in event["resourceVersions"]
                    .as_array()
                    .expect("resource entries")
                {
                    if let Some(documents) = entry["entityChanges"]["documents"].as_array() {
                        for document in documents {
                            let value = &document["value"];
                            if document["collection"] == "accountCost"
                                && document["id"] == "default"
                                && value["refreshing"] == false
                                && value["checkedAt"].as_f64().is_some_and(|at| at > 1.0)
                            {
                                return value.clone();
                            }
                        }
                    }
                }
            }
            panic!("cost stream closed before worker completion");
        })
        .await
        .expect("first unreserved interest dispatches and publishes completion over SSE");
        let cache_value: Value = serde_json::from_slice(
            &tokio::fs::read(cache.join("claude-costs.json"))
                .await
                .expect("actual scanner saved cache"),
        )
        .expect("saved scanner result");
        assert_eq!(saved["checkedAt"], cache_value["checked"]);
        assert!(
            saved["error"].as_str().is_some(),
            "the actual missing-profile scan records its failure"
        );
        state
            .shutdown_services()
            .await
            .expect("all dispatched scans settle");
        assert_eq!(saved["data"]["last30DaysUSD"], 3);
        assert_eq!(saved["refreshing"], false);
        assert_eq!(saved["stale"], true);
        assert_eq!(saved["at"], 1);
    }

    #[tokio::test]
    async fn scoped_session_cost_update_and_delete_preserve_an_unrelated_team() {
        let (_root, state) = state().await;
        let store = state.store.as_ref().expect("store");
        store
            .transaction(|tx| {
                for id in ["a", "b"] {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        [id, &json!({"id":id,"rootId":id,"isLead":true}).to_string()],
                    )?;
                }
                Ok(())
            })
            .await
            .expect("teams");
        publish_sessions(&state).await.expect("full baseline");
        let mut untouched = entity(&state, "sessionCost", "b");
        untouched["method"] = json!("untouched team witness");
        put(store, "sessionCost", vec![("b".into(), untouched.clone())])
            .await
            .expect("saved unrelated estimate");
        publish_session_roots(
            &state,
            Some(BTreeSet::from(["a".into()])),
            PricingReadiness::Settled,
        )
        .await
        .expect("scoped update");
        assert_eq!(entity(&state, "sessionCost", "b"), untouched);
        let mut roots = read_agent_roots(store).await.expect("root identities");
        let rowid = *roots
            .iter()
            .find(|(_, root)| *root == "a")
            .expect("a rowid")
            .0;
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_agents WHERE id='a'", [])?;
                Ok(())
            })
            .await
            .expect("delete team");
        let changed = changed_agent_roots(store, &mut roots, vec![rowid, rowid])
            .await
            .expect("old root retained after delete");
        assert_eq!(changed, BTreeSet::from(["a".into()]));
        publish_session_roots(&state, Some(changed), PricingReadiness::Settled)
            .await
            .expect("explicit scoped delete");
        assert_eq!(entity(&state, "sessionCost", "b"), untouched);
        assert!(
            store
                .test_read_transaction(|tx| Ok(tx.query_row(
                    "SELECT deleted FROM sync_entities WHERE collection='sessionCost' AND id='a'",
                    [],
                    |row| row.get::<_, bool>(0)
                )?))
                .expect("a tombstone")
        );
    }

    #[tokio::test]
    async fn failed_claude_refresh_preserves_the_saved_result_age() {
        let root = tempfile::tempdir().expect("isolated cost state");
        let home = root.path().join("missing-profile");
        let account = json!({"home":home,"provider":"claude","status":"ready"});
        let cache =
            root.path()
                .join("account-costs/claude")
                .join(studio_runtime_store::sha256_hex(
                    format!("age-witness{}", home.display()).as_bytes(),
                ));
        tokio::fs::create_dir_all(&cache)
            .await
            .expect("cache directory");
        tokio::fs::write(
            cache.join("claude-costs.json"),
            json!({"config":home,"checked":1,"error":null,"data":{"last30DaysUSD":3}}).to_string(),
        )
        .await
        .expect("saved estimate");
        let (saved, result) = studio_runtime_services::usage::refresh_account_costs_report(
            root.path(),
            "age-witness",
            &account,
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(saved);
        result.expect("failure recorded in cache");
        let value = saved_cost(root.path(), "age-witness", &account);
        assert_eq!(value["data"]["last30DaysUSD"], 3);
        assert_eq!(value["stale"], true);
        assert_eq!(
            value["at"], 1,
            "a failed check must not make old data look fresh"
        );
    }

    // A fixture gate pauses the actual cost worker before its analytics read.
    // The witness does not depend on machine speed or create contention load.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_serves_http_while_large_session_cost_publication_is_pending() {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::{Request, StatusCode};
        use futures_util::StreamExt;
        use std::net::SocketAddr;
        use std::time::Duration;
        use tower::ServiceExt;

        let (root, state) = state().await;
        let store = state.store.as_ref().expect("store");
        store.transaction(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('large-team',?1)",
                [json!({"id":"large-team","rootId":"large-team","isLead":true,"provider":"codex"}).to_string()])?;
            Ok(())
        }).await.expect("lead");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            tx.execute_batch("WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<32768)
                INSERT INTO analytics_cost_rows(id,root,agent,seq,at,model,account_key,input,output,uncached,counted,counted_model,final_evidence)
                SELECT 'usage-'||i,'large-team','large-team',i,i,'gpt-test','default',100,10,1,1,'gpt-test',1 FROM n;
                INSERT INTO analytics_cost_groups(root,model,account,input,output,uncached,count)
                VALUES('large-team','gpt-test','default',100,10,1,32768);")?;
            Ok(())
        }).await.expect("large synthetic cost table and its aggregate");
        tokio::fs::write(
            root.path().join("model-pricing/models-dev-v1.json"),
            json!({"version":1,"catalog":{"providers":pricing_providers()}}).to_string(),
        )
        .await
        .expect("local pricing");
        let started = Arc::new(tokio::sync::Notify::new());
        let (release, blocked) = std::sync::mpsc::channel();
        SESSION_COST_GATES
            .lock()
            .expect("cost gates")
            .insert(store.root().to_owned(), (started.clone(), blocked));
        let mut changes = commits(&state);
        let startup = state.clone();
        let mut request = tokio::spawn(async move {
            // Match runtime-server: summary initialization precedes HTTP service.
            startup
                .initialize_bounded_entities()
                .await
                .expect("initialize summaries");
            let app = crate::router::router(startup).expect("router");
            app.oneshot(
                Request::builder()
                    .uri("/api/desktop")
                    .header("host", "127.0.0.1:4321")
                    .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("HTTP response")
        });
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("session cost publication reaches the analytics gate");
        let response = tokio::time::timeout(Duration::from_secs(2), &mut request).await;
        let stream = if response.is_ok() {
            Some(
                tokio::time::timeout(Duration::from_secs(2), async {
                    let query = form_urlencoded::Serializer::new(String::new())
                        .append_pair(
                            "protocol",
                            studio_runtime_schema::SYNC_PROTOCOL_VERSION_TEXT,
                        )
                        .append_pair("tokenRates", "false")
                        .append_pair(
                            "resources",
                            &json!([{"kind":"session-cost","agentId":"large-team"}]).to_string(),
                        )
                        .finish();
                    let app = crate::router::router(state.clone()).expect("stream router");
                    let response = app
                        .oneshot(
                            Request::builder()
                                .uri(format!("/api/sync/stream?{query}"))
                                .header("host", "127.0.0.1:4321")
                                .header("x-canvas-token", "token")
                                .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
                                .body(Body::empty())
                                .expect("stream request"),
                        )
                        .await
                        .expect("stream response");
                    assert_eq!(response.status(), StatusCode::OK);
                    let mut stream = response.into_body().into_data_stream();
                    let session = stream
                        .next()
                        .await
                        .expect("session event")
                        .expect("session bytes");
                    assert!(String::from_utf8_lossy(&session).contains("event: session"));
                    let initial = stream
                        .next()
                        .await
                        .expect("initial costs")
                        .expect("initial bytes");
                    assert!(
                        String::from_utf8_lossy(&initial).contains(r#""pricingState":"loading""#)
                    );
                    stream
                })
                .await,
            )
        } else {
            None
        };
        // Always release the worker before a timeout assertion can unwind the runtime.
        release.send(()).expect("release analytics read gate");
        if response.is_err() {
            request
                .await
                .expect("request finishes after analytics release");
        }
        let response = response
            .expect("HTTP must serve before analytics cost publication finishes")
            .expect("request task");
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = stream
            .expect("opened stream")
            .expect("SSE initial costs must not wait for analytics publication");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let frame = stream
                    .next()
                    .await
                    .expect("cost update event")
                    .expect("cost update bytes");
                if String::from_utf8_lossy(&frame).contains(r#""pricedSamples":32768"#) {
                    break;
                }
            }
        })
        .await
        .expect("SSE receives costs after publication finishes");
        let cost = wait_for_entity(&state, &mut changes, "sessionCost", "large-team", |value| {
            value["pricedSamples"] == 32768
        })
        .await
        .expect("costs fill in after the read gate opens");
        assert_eq!(cost["pricingState"], "ready");
        assert!((cost["totalUSD"].as_f64().expect("estimate") - 3.93216).abs() < 1e-9);
        state
            .shutdown_services()
            .await
            .expect("cost workers settle");
    }

    #[tokio::test]
    async fn startup_cost_seed_preserves_saved_estimates_and_retires_removed_roots() {
        let (_root, state) = state().await;
        pricing_roots(&state).await;
        // Both leads start pending. A saved result then survives the next seed.
        seed_session_roots(&state).await.expect("initial cost seed");
        let mut saved = entity(&state, "sessionCost", "a");
        saved["pricingState"] = json!("ready");
        saved["totalUSD"] = json!(7);
        saved["pricedSamples"] = json!(3);
        let store = state.store.as_ref().expect("store");
        put(store, "sessionCost", vec![("a".into(), saved.clone())])
            .await
            .expect("saved estimate");
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_agents WHERE id='b'", [])?;
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('c',?1)",
                    [json!({"id":"c","rootId":"c","isLead":true}).to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("replace a lead");
        seed_session_roots(&state)
            .await
            .expect("seed after restart");
        assert_eq!(entity(&state, "sessionCost", "a"), saved);
        assert_eq!(
            entity(&state, "sessionCost", "c")["pricingState"],
            "loading"
        );
        assert!(entity(&state, "sessionCost", "c")["totalUSD"].is_null());
        assert!(
            store
                .test_read_transaction(|tx| Ok(tx.query_row(
                    "SELECT deleted FROM sync_entities WHERE collection='sessionCost' AND id='b'",
                    [],
                    |row| row.get::<_, bool>(0)
                )?))
                .expect("retired cost")
        );
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
        state
            .shutdown_services()
            .await
            .expect("fixture refresh tasks drained");
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
    async fn cost_summary_excludes_scanner_bookkeeping_from_published_cache() {
        let (_root, state) = state().await;
        let summary = cost_summary(json!({"accountKey":"default","at":1,"error":null,"data":null,"refreshing":false,"stale":false,"sourceFingerprint":"scanner-only","_refreshDue":true})).expect("public DTO");
        put(
            state.store.as_ref().expect("store"),
            "accountCost",
            vec![("default".into(), summary)],
        )
        .await
        .expect("cache publication");
        let value = entity(&state, "accountCost", "default");
        assert_eq!(value["at"], 1);
        assert!(value.get("sourceFingerprint").is_none());
        assert!(value.get("_refreshDue").is_none());
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
