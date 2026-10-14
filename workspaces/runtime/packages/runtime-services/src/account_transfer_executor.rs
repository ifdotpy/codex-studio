//! Retained-session executor for lazy account moves.
//!
//! Ports Python `codex_account_transfer.py:destination_settings`, `move_lazy`,
//! `read_transfer_source`, and `codex_portable_history.py:_native_records`.
//! Provider writes are guarded by the transfer driver's durable intent and exact
//! SessionReceipt; an Unknown outcome is never resent.

use crate::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ProviderRequest, ProviderResponse,
        RequestCertainty, SessionReceipt,
    },
    account_transfer::TransferError,
    account_transfer_driver::{
        self as driver, PreparedTransfer, TransferDispatch, TransferExecutor, TransferMethod,
        TransferPreparation, TransferTransport,
    },
    model_catalog::ModelCatalogService,
    portable_history::{self, ExportInput, NativeHistory},
    submit_gates::{
        self as gates, CatalogIdentity, CatalogObservation, ExecutionSettings, ModelCapabilities,
        StoredField,
    },
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use studio_runtime_providers::Method;
use studio_runtime_store::RuntimeStore;

const READ_DEADLINE: Duration = Duration::from_secs(120);
const MUTATION_DEADLINE: Duration = Duration::from_secs(60);
static NATIVE_IMPORT_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn copy_native_history(
    source_home: &str,
    target_home: &str,
    native_path: &str,
) -> Result<String, TransferError> {
    let source_root = fs::canonicalize(source_home)
        .map_err(|_| TransferError::Refused("Source native history is unavailable"))?;
    let source = fs::canonicalize(native_path)
        .map_err(|_| TransferError::Refused("Source native history is unavailable"))?;
    let relative = source.strip_prefix(&source_root).map_err(|_| {
        TransferError::Refused("Source native history path escaped its account home")
    })?;
    if !matches!(
        relative
            .components()
            .next()
            .and_then(|part| part.as_os_str().to_str()),
        Some("sessions" | "archived_sessions")
    ) || source.extension().and_then(|value| value.to_str()) != Some("jsonl")
    {
        return Err(TransferError::Refused(
            "Unsupported native history format; original context is preserved",
        ));
    }
    let bytes = fs::read(&source)
        .map_err(|_| TransferError::Refused("Source native history is not fully persisted"))?;
    let first_end = bytes
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or(TransferError::Refused(
            "Native history has no complete session metadata",
        ))?;
    let metadata: Value = serde_json::from_slice(&bytes[..first_end])
        .map_err(|_| TransferError::Refused("Native history metadata is invalid"))?;
    if metadata["type"] != "session_meta" || metadata.pointer("/payload/history_base").is_some() {
        return Err(TransferError::Refused(
            "Native history ancestry requires the portable-history importer",
        ));
    }
    if !bytes.ends_with(b"\n") {
        return Err(TransferError::Refused(
            "Source native history is not fully persisted",
        ));
    }
    let target_root = PathBuf::from(target_home);
    let destination = target_root.join("sessions").join(".studio-imports").join(
        source
            .file_name()
            .ok_or(TransferError::Refused("Invalid native history filename"))?,
    );
    fs::create_dir_all(
        destination
            .parent()
            .ok_or(TransferError::Refused("Invalid native history destination"))?,
    )
    .map_err(|_| TransferError::Refused("Could not create native history destination"))?;
    if destination.exists() {
        if fs::read(&destination)
            .map_err(|_| TransferError::Refused("Native history destination is unreadable"))?
            != bytes
        {
            return Err(TransferError::Refused(
                "The destination has different native history. No file was overwritten.",
            ));
        }
    } else {
        let temporary = destination.with_extension(format!(
            "studio-transfer-{}-{}.tmp",
            std::process::id(),
            NATIVE_IMPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&temporary, &bytes)
            .map_err(|_| TransferError::Refused("Could not copy native history"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
                .map_err(|_| TransferError::Refused("Could not secure native history copy"))?;
        }
        fs::rename(&temporary, &destination)
            .map_err(|_| TransferError::Refused("Could not publish native history copy"))?;
    }
    let manifest_path = target_root.join(".studio-account-imports.json");
    let mut manifest: serde_json::Map<String, Value> = if manifest_path.exists() {
        serde_json::from_slice(
            &fs::read(&manifest_path)
                .map_err(|_| TransferError::Refused("Native import manifest is unreadable"))?,
        )
        .map_err(|_| TransferError::Refused("Native import manifest is invalid"))?
    } else {
        serde_json::Map::new()
    };
    let relative_destination = destination.strip_prefix(&target_root).map_err(|_| {
        TransferError::Refused("Native history destination escaped its account home")
    })?;
    let relative_key = relative_destination.to_string_lossy().replace('\\', "/");
    manifest.insert(
        relative_key,
        json!({
            "threadId":metadata.pointer("/payload/id").cloned().unwrap_or(Value::Null),
            "sha256":studio_runtime_store::sha256_hex(&bytes),
            "sourceHome":source_root,
        }),
    );
    let temporary_manifest = manifest_path.with_extension("tmp");
    fs::write(
        &temporary_manifest,
        serde_json::to_vec(&Value::Object(manifest))
            .map_err(|_| TransferError::Refused("Could not encode native import manifest"))?,
    )
    .map_err(|_| TransferError::Refused("Could not publish native import manifest"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary_manifest, fs::Permissions::from_mode(0o600))
            .map_err(|_| TransferError::Refused("Could not secure native import manifest"))?;
    }
    fs::rename(&temporary_manifest, &manifest_path)
        .map_err(|_| TransferError::Refused("Could not publish native import manifest"))?;
    Ok(destination.to_string_lossy().into_owned())
}

pub struct AccountTransferExecutor<'a> {
    pub store: &'a RuntimeStore,
    pub session: Arc<dyn AccountSessionPort>,
    pub catalog: &'a ModelCatalogService,
    pub preparation: &'a crate::account_worker::preparation::PreparationResources,
    pub now: f64,
}

fn session_reply(
    store: &RuntimeStore,
    session: &dyn AccountSessionPort,
    transfer: &str,
    agent: &str,
    account: &str,
    actor: Option<crate::account_worker::retained::RequestActor>,
    operation: String,
    method: Method,
    params: Value,
    now: f64,
    deadline: Duration,
) -> Result<(SessionReceipt, Value), TransferError> {
    let scope = session
        .current_scope(account)
        .ok_or(TransferError::Requires("provider-session/account-worker"))?;
    if scope.account != account {
        return Err(TransferError::Refused(
            "Provider read account scope changed",
        ));
    }
    let method_name = method.as_str();
    let replay = driver::provider_read_intent(
        store,
        transfer,
        agent,
        &operation,
        account,
        serde_json::to_value(&scope)
            .map_err(|_| TransferError::Refused("Invalid provider scope"))?,
        method_name,
        params.clone(),
        now,
    )?;
    if let Some(saved) = replay {
        let value = match saved["result"]["kind"].as_str() {
            Some("reply") => saved["result"].get("value").cloned().unwrap_or(Value::Null),
            _ => {
                return Err(TransferError::Refused(
                    "The provider rejected a transfer read",
                ));
            }
        };
        let ticket = studio_runtime_providers::RequestTicket {
            generation: saved["receipt"]["ticket"]["generation"]
                .as_u64()
                .unwrap_or_default(),
            id: serde_json::from_value(saved["receipt"]["ticket"]["id"].clone())
                .map_err(|_| TransferError::Refused("Invalid saved provider ticket"))?,
            method: Method::from_wire(
                saved["receipt"]["ticket"]["method"]
                    .as_str()
                    .unwrap_or_default(),
            )
            .ok_or(TransferError::Refused("Invalid saved provider ticket"))?,
        };
        return Ok((
            SessionReceipt {
                scope: serde_json::from_value(saved["receipt"]["scope"].clone())
                    .map_err(|_| TransferError::Refused("Invalid saved provider scope"))?,
                operation: saved["receipt"]["operation"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                ticket: Some(ticket),
                certainty: RequestCertainty::Written,
                result: Some(ProviderResponse::Reply(value.clone())),
                not_sent_reason: None,
            },
            value,
        ));
    }
    let mut last_receipt = None;
    for attempt in 0..2 {
        let receipt = session
            .provider_request(ProviderRequest {
                account: account.into(),
                operation: operation.clone(),
                actor: actor.clone(),
                expected_scope: Some(scope.clone()),
                method,
                params: params.clone(),
                deadline: Instant::now() + deadline,
            })
            .map_err(|error| match error {
                AccountSessionError::Requires(name) => TransferError::Requires(name),
                _ => TransferError::Refused(
                    "The retained provider request failed; inspect its durable intent",
                ),
            })?;
        let certainty = match receipt.certainty {
            RequestCertainty::Written => "written",
            RequestCertainty::NotSent => "not_sent",
            RequestCertainty::Unknown => "unknown",
        };
        let ticket = receipt.ticket.as_ref().map(|ticket| {
            json!({
                "generation":ticket.generation,
                "id":ticket.id,
                "method":ticket.method.as_str(),
            })
        });
        let result = receipt.result.as_ref().map(|value| match value {
            ProviderResponse::Reply(value) => json!({"kind":"reply","value":value}),
            ProviderResponse::Rejected(value) => json!({"kind":"rejected","value":value}),
        });
        let exact = json!({"operation":receipt.operation,"scope":receipt.scope,
            "certainty":certainty,"ticket":ticket,
            "notSentReason":receipt.not_sent_reason.map(|reason| format!("{reason:?}"))});
        driver::provider_read_receipt(
            store,
            transfer,
            agent,
            &operation,
            exact,
            result.clone(),
            certainty,
            now,
        )?;
        if receipt.certainty == RequestCertainty::NotSent && attempt == 0 {
            last_receipt = Some(receipt);
            continue;
        }
        if receipt.operation != operation
            || receipt.scope != scope
            || receipt.certainty != RequestCertainty::Written
            || receipt.ticket.is_none()
        {
            return Err(TransferError::Refused(
                "The retained read receipt is not Written on the expected connection",
            ));
        }
        let response = match receipt.result.clone() {
            Some(ProviderResponse::Reply(value)) => value,
            Some(ProviderResponse::Rejected(_)) | None => {
                return Err(TransferError::Refused(
                    "The provider rejected a transfer read",
                ));
            }
        };
        return Ok((receipt, response));
    }
    let receipt = last_receipt.ok_or(TransferError::Refused("Provider read was not sent"))?;
    if receipt.certainty == RequestCertainty::NotSent {
        return Err(TransferError::Refused(
            "Provider read was not sent after retry",
        ));
    }
    Err(TransferError::Refused(
        "The provider read outcome is unknown",
    ))
}

fn read_catalog(
    store: &RuntimeStore,
    session: &dyn AccountSessionPort,
    transfer: &str,
    account: &str,
    provider: &str,
    home: &str,
    agent: &str,
    transfer_id: &str,
    now: f64,
) -> Result<CatalogObservation, TransferError> {
    let mut cursor: Option<String> = None;
    let mut seen = std::collections::HashSet::new();
    let mut rows = Vec::new();
    for page in 0..16 {
        let mut params =
            json!({"limit":studio_runtime_schema::shared_values::MODEL_CATALOG_ITEMS_PER_PAGE});
        if let Some(value) = &cursor {
            params["cursor"] = json!(value);
        }
        let operation = format!("account-transfer:{transfer_id}:{agent}:model-list:{page}");
        let (_, response) = session_reply(
            store,
            session,
            transfer,
            agent,
            account,
            None,
            operation,
            Method::ModelList,
            params,
            now,
            READ_DEADLINE,
        )?;
        let data = response["data"]
            .as_array()
            .ok_or(TransferError::Refused("Invalid destination model catalog"))?;
        rows.extend(data.iter().cloned());
        match response.get("nextCursor") {
            None | Some(Value::Null) => {
                let models = rows
                    .iter()
                    .map(|row| {
                        Ok(ModelCapabilities {
                            model: row["model"]
                                .as_str()
                                .ok_or(TransferError::Refused("Invalid catalog model"))?
                                .into(),
                            resolved_model: row
                                .get("resolvedModel")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            is_default: row
                                .get("isDefault")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            hidden: row.get("hidden").and_then(Value::as_bool).unwrap_or(false),
                            access_programs: row
                                .pointer("/availableAccessPrograms/cyber")
                                .and_then(Value::as_array)
                                .map(|values| {
                                    values
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .map(str::to_owned)
                                        .collect()
                                }),
                            supported_efforts: row["supportedReasoningEfforts"]
                                .as_array()
                                .map(|efforts| {
                                    efforts
                                        .iter()
                                        .filter_map(|v| {
                                            v["reasoningEffort"].as_str().map(str::to_owned)
                                        })
                                        .collect()
                                })
                                .unwrap_or_default(),
                            default_effort: row
                                .get("defaultReasoningEffort")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            priority: row["serviceTiers"]
                                .as_array()
                                .is_some_and(|tiers| tiers.iter().any(|v| v["id"] == "priority")),
                        })
                    })
                    .collect::<Result<Vec<_>, TransferError>>()?;
                return Ok(CatalogObservation {
                    identity: CatalogIdentity {
                        account_key: account.into(),
                        home: home.into(),
                        provider: provider.into(),
                        generation: 0,
                    },
                    expires_at: Instant::now() + Duration::from_secs(30),
                    models,
                });
            }
            Some(Value::String(value)) if !value.is_empty() && seen.insert(value.clone()) => {
                cursor = Some(value.clone())
            }
            _ => return Err(TransferError::Refused("Invalid destination model cursor")),
        }
    }
    Err(TransferError::Refused(
        "Destination catalog pagination limit exceeded",
    ))
}

fn destination_settings(
    agent: &Value,
    source_provider: &str,
    target_provider: &str,
    catalog: &CatalogObservation,
) -> Result<Value, TransferError> {
    let changed = source_provider != target_provider;
    let visible = catalog
        .models
        .iter()
        .filter(|model| !model.hidden)
        .collect::<Vec<_>>();
    let current = agent["model"].as_str().and_then(|name| {
        visible
            .iter()
            .find(|model| model.model == name || model.resolved_model.as_deref() == Some(name))
    });
    let chosen = if changed || current.is_none() {
        visible.iter().find(|model| model.is_default).or_else(|| {
            let fallback = if target_provider == "claude" {
                "default"
            } else {
                crate::agents_create::DEFAULT_LEAD_MODEL
            };
            visible.iter().find(|model| model.model == fallback)
        })
    } else {
        current
    };
    let model = chosen.ok_or(TransferError::Refused(
        "The destination account has no available default model",
    ))?;
    let mut selected_model = model.model.clone();
    if !changed && model.resolved_model.as_deref() == agent["model"].as_str() {
        selected_model = agent["model"].as_str().unwrap_or_default().into();
    }
    let requested_fast = !changed && agent["fastMode"] == true;
    let settings = ExecutionSettings {
        model: StoredField::Value(selected_model.clone()),
        effort: if changed {
            None
        } else {
            agent
                .get("effort")
                .and_then(Value::as_str)
                .map(str::to_owned)
        },
        native_effort: StoredField::Missing,
        fast_mode: Some(requested_fast && model.priority),
        daybreak_enabled: Some(!changed && agent["daybreakEnabled"] == true),
    };
    let (effort, native_effort) =
        gates::validate_execution(&settings, catalog, changed || settings.effort.is_none())
            .map_err(|_| TransferError::Requires("account-transfer/destination-settings"))?;
    let mut worker_defaults = agent
        .get("workerDefaults")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    worker_defaults.insert("accountKey".into(), json!(catalog.identity.account_key));
    if changed {
        worker_defaults.insert("model".into(), Value::Null);
        worker_defaults.insert("effort".into(), json!("medium"));
        worker_defaults.insert("fastMode".into(), json!(false));
        worker_defaults.insert("daybreakEnabled".into(), json!(false));
        worker_defaults.insert("cyberAccessProgram".into(), json!("standard"));
    }
    let mut result = json!({"provider":target_provider,"model":selected_model,"effort":effort,
        "nativeEffort":native_effort,"fastMode":settings.fast_mode.unwrap_or(false),
        "daybreakEnabled":settings.daybreak_enabled.unwrap_or(false),"workerDefaults":worker_defaults});
    let daybreak = !changed && agent["daybreakEnabled"] == true;
    let cyber_program = if target_provider == "claude" {
        if daybreak {
            return Err(TransferError::Requires(
                "account-transfer/destination-settings",
            ));
        }
        "standard".to_owned()
    } else {
        let grants = catalog
            .models
            .iter()
            .find(|row| row.model == selected_model && !row.hidden)
            .and_then(|row| row.access_programs.as_ref());
        if daybreak {
            grants
                .and_then(|programs| {
                    ["daybreakBlue", "daybreakRed"]
                        .into_iter()
                        .find(|p| programs.iter().any(|v| v == p))
                })
                .ok_or(TransferError::Requires(
                    "account-transfer/destination-settings",
                ))?
                .to_owned()
        } else if grants.is_some_and(|programs| !programs.iter().any(|value| value == "standard")) {
            return Err(TransferError::Requires(
                "account-transfer/destination-settings",
            ));
        } else {
            "standard".to_owned()
        }
    };
    result["cyberAccessProgram"] = json!(cyber_program);
    if changed {
        result["pendingSettings"] = Value::Null;
        result["pendingSettingsAccountKey"] = Value::Null;
    } else if let Some(pending) = agent
        .get("pendingSettings")
        .filter(|value| value.is_object())
    {
        if agent
            .get("pendingSettingsAccountKey")
            .and_then(Value::as_str)
            .is_some_and(|key| key != catalog.identity.account_key)
        {
            return Err(TransferError::Refused(
                "Queued settings belong to another account. Save them again before transfer",
            ));
        }
        let pending_model = pending
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let pending_fast = pending
            .get("fastMode")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let pending_exec = ExecutionSettings {
            model: StoredField::Value(pending_model.into()),
            effort: pending
                .get("effort")
                .and_then(Value::as_str)
                .map(str::to_owned),
            native_effort: StoredField::Missing,
            fast_mode: Some(pending_fast),
            daybreak_enabled: Some(
                pending
                    .get("daybreakEnabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            ),
        };
        let (pending_effort, pending_native) =
            gates::validate_execution(&pending_exec, catalog, false)
                .map_err(|_| TransferError::Requires("account-transfer/destination-settings"))?;
        let mut normalized = pending.clone();
        normalized["effort"] = pending_effort.clone().map_or(Value::Null, Value::String);
        normalized["nativeEffort"] = pending_native.clone().map_or(Value::Null, Value::String);
        normalized["cyberAccessProgram"] = pending
            .get("cyberAccessProgram")
            .cloned()
            .unwrap_or(json!("standard"));
        result["pendingSettings"] = normalized;
        result["pendingSettingsAccountKey"] = json!(catalog.identity.account_key);
    }
    Ok(result)
}

impl TransferExecutor for AccountTransferExecutor<'_> {
    fn available(&self) -> Result<(), TransferError> {
        Ok(())
    }

    fn prepare(&self, input: &TransferPreparation) -> Result<PreparedTransfer, TransferError> {
        let operation = input.operation();
        let agent = input.agent();
        let member = &operation["members"][agent["id"].as_str().unwrap_or_default()];
        let agent_id = agent["id"]
            .as_str()
            .ok_or(TransferError::Refused("Invalid transfer agent"))?;
        let epoch = agent["epoch"]
            .as_i64()
            .ok_or(TransferError::Refused("Invalid transfer epoch"))?;
        let transfer_id = operation["id"]
            .as_str()
            .ok_or(TransferError::Refused("Invalid transfer id"))?;
        let source_account = member["sourceAccountKey"].as_str().unwrap_or("default");
        let target_account = operation["targetAccountKey"]
            .as_str()
            .ok_or(TransferError::Refused("Invalid transfer target"))?;
        let source_provider = member["provider"].as_str().unwrap_or("codex");
        let target_provider = operation["targetProvider"].as_str().unwrap_or("codex");
        let (home, registered_provider) =
            self.catalog
                .account_profile(target_account)
                .ok_or(TransferError::Requires(
                    "account-transfer/destination-settings",
                ))?;
        if registered_provider != target_provider {
            return Err(TransferError::Refused(
                "Destination provider changed during transfer",
            ));
        }
        let catalog = read_catalog(
            self.store,
            &*self.session,
            transfer_id,
            target_account,
            target_provider,
            &home,
            agent_id,
            transfer_id,
            self.now,
        )?;
        let settings = destination_settings(agent, source_provider, target_provider, &catalog)?;

        let mut portable = None;
        let mut history_text = None;
        let mut copied_history_path = None;
        let source_thread = member["sourceThreadId"].as_str();
        if let Some(thread_id) = source_thread {
            let operation_id = format!("account-transfer:{transfer_id}:{agent_id}:thread-read");
            let (_, read) = session_reply(
                self.store,
                &*self.session,
                transfer_id,
                agent_id,
                source_account,
                None,
                operation_id,
                Method::ThreadRead,
                json!({"threadId":thread_id,"includeTurns":false}),
                self.now,
                READ_DEADLINE,
            )?;
            let thread = read
                .get("thread")
                .filter(|thread| thread["id"] == thread_id)
                .ok_or(TransferError::Refused("Source thread identity changed"))?;
            if !matches!(
                thread["status"]["type"].as_str(),
                Some("idle" | "notLoaded" | "systemError")
            ) {
                return Err(TransferError::Refused("Source thread is not idle"));
            }
            let portable_required = source_provider != target_provider
                || source_provider == "claude"
                || target_provider == "claude";
            if !portable_required {
                let native_path =
                    thread
                        .get("path")
                        .and_then(Value::as_str)
                        .ok_or(TransferError::Refused(
                            "Codex returned no saved context path",
                        ))?;
                let source_home = self
                    .catalog
                    .account_profile(source_account)
                    .map(|(home, _)| home)
                    .ok_or(TransferError::Requires(
                        "account-transfer/native-history-copy",
                    ))?;
                copied_history_path = Some(copy_native_history(&source_home, &home, native_path)?);
            }
            let mut records = vec![json!({"kind":"native_thread","thread":thread})];
            let mut cursor: Option<String> = None;
            let mut cursors = std::collections::HashSet::new();
            for page in 0..1024 {
                let mut params = json!({"threadId":thread_id,"limit":20,"sortDirection":"desc","itemsView":"full"});
                if let Some(value) = &cursor {
                    params["cursor"] = json!(value);
                }
                let (_, page_value) = session_reply(
                    self.store,
                    &*self.session,
                    transfer_id,
                    agent_id,
                    source_account,
                    None,
                    format!("account-transfer:{transfer_id}:{agent_id}:turns:{page}"),
                    Method::ThreadTurnsList,
                    params.clone(),
                    self.now,
                    READ_DEADLINE,
                )?;
                if !page_value["data"].is_array() {
                    return Err(TransferError::Refused("Invalid source history page"));
                }
                records.push(json!({"kind":"native_turn_page","request":params,"page":page_value}));
                match page_value.get("nextCursor") {
                    None | Some(Value::Null) => break,
                    Some(Value::String(value))
                        if !value.is_empty() && cursors.insert(value.clone()) =>
                    {
                        cursor = Some(value.clone())
                    }
                    _ => {
                        return Err(TransferError::Refused(
                            "Repeated or invalid source history cursor",
                        ));
                    }
                }
            }
            if portable_required {
                let mut archive_agent = agent.clone();
                if let Some(source_state) = agent
                    .get("lazyAccountTransfer")
                    .and_then(|lazy| lazy.get("sourceState"))
                    .and_then(Value::as_object)
                {
                    for (key, value) in source_state {
                        archive_agent[key] = value.clone();
                    }
                }
                archive_agent["accountKey"] = json!(source_account);
                archive_agent["threadId"] = json!(thread_id);
                let native = NativeHistory {
                    account_key: source_account.into(),
                    thread_id: thread_id.into(),
                    epoch,
                    settings: portable_history::source_settings_snapshot(&archive_agent),
                    connection_generation: self
                        .session
                        .current_scope(source_account)
                        .ok_or(TransferError::Requires("provider-session/account-worker"))?
                        .generation,
                    records: Box::new(records.into_iter().map(Ok)),
                    checked_empty_proof: None,
                };
                let descriptor = portable_history::export_history(
                    self.store,
                    ExportInput {
                        agent_id: agent_id.into(),
                        account_key: source_account.into(),
                        epoch,
                        thread_id: Some(thread_id.into()),
                        source_settings: native.settings.clone(),
                        source_connection_generation: Some(native.connection_generation),
                        transfer_id: transfer_id.into(),
                        native_history: Some(native),
                    },
                )
                .map_err(|_| TransferError::Refused("Source portable history export failed"))?;
                history_text = Some(
                    portable_history::history_context(self.store.root(), &descriptor).map_err(
                        |_| TransferError::Refused("Source portable history validation failed"),
                    )?,
                );
                portable =
                    Some(descriptor.to_value().map_err(|_| {
                        TransferError::Refused("Invalid portable archive descriptor")
                    })?);
            }
        }
        let scope = self
            .session
            .current_scope(target_account)
            .ok_or(TransferError::Requires("provider-session/account-worker"))?;
        let prepared = crate::account_worker::preparation::prepare_native_thread(
            self.store,
            self.preparation,
            agent_id,
            &format!("account-transfer:{transfer_id}:{agent_id}"),
            scope.generation,
        )
        .map_err(|_| TransferError::Refused("Destination thread preparation failed"))?;
        let mut params = prepared.params;
        params["model"] = settings["model"].clone();
        if let Some(config) = params.get_mut("config").and_then(Value::as_object_mut) {
            if settings.get("nativeEffort").is_some_and(Value::is_string) {
                config.insert(
                    "model_reasoning_effort".into(),
                    settings["nativeEffort"].clone(),
                );
            }
            config.insert(
                "features.fast_mode".into(),
                json!(settings["fastMode"] == true),
            );
        }
        params["serviceTier"] = json!(if settings["fastMode"] == true {
            "priority"
        } else {
            "default"
        });
        if let Some(text) = history_text {
            params["developerInstructions"] = json!(text);
        }
        if let Some(path) = copied_history_path {
            params["path"] = json!(path);
        }
        if let Some(history) = &portable {
            params["portableHistory"] = history.clone();
        }
        let method = if source_thread.is_some() && portable.is_none() {
            TransferMethod::Fork
        } else {
            TransferMethod::Start
        };
        if method == TransferMethod::Fork {
            if let Some(object) = params.as_object_mut() {
                drop(object.remove("dynamicTools"));
            }
            params["threadId"] = json!(source_thread);
            params["excludeTurns"] = json!(true);
            params["deferGoalContinuation"] = json!(true);
        }
        driver::prepared_member(
            self.store,
            transfer_id,
            agent_id,
            settings,
            portable,
            self.now,
        )?;
        let scope = self
            .session
            .current_scope(target_account)
            .ok_or(TransferError::Requires("provider-session/account-worker"))?;
        let operation_id = format!(
            "account-transfer:{transfer_id}:{agent_id}:{}",
            method.wire()
        );
        Ok(PreparedTransfer {
            ticket: crate::account_transfer::Ticket {
                operation: transfer_id.into(),
                agent: agent_id.into(),
                epoch,
                account: target_account.into(),
                connection: scope.connection.clone(),
                request: operation_id,
            },
            scope,
            method,
            params,
        })
    }
}

impl TransferTransport for AccountTransferExecutor<'_> {
    fn send(
        &self,
        ticket: &crate::account_transfer::Ticket,
        scope: &crate::account_session_runtime::ConnectionScope,
        method: &str,
        params: &Value,
    ) -> TransferDispatch {
        let parsed = Method::from_wire(method);
        let Some(method) = parsed else {
            return TransferDispatch {
                receipt: json!({"operation":ticket.request,"scope":scope,"certainty":"not_sent","ticket":null}),
                result: None,
                certainty: "not_sent",
            };
        };
        let request = ProviderRequest {
            account: ticket.account.clone(),
            operation: ticket.request.clone(),
            actor: Some(crate::account_worker::retained::RequestActor {
                agent: ticket.agent.clone(),
                epoch: ticket.epoch,
            }),
            expected_scope: Some(scope.clone()),
            method,
            params: params.clone(),
            deadline: Instant::now() + MUTATION_DEADLINE,
        };
        let session_receipt = self.session.provider_request(request);
        match session_receipt {
            Ok(receipt) => {
                let certainty = match receipt.certainty {
                    RequestCertainty::Written => "written",
                    RequestCertainty::NotSent => "not_sent",
                    RequestCertainty::Unknown => "unknown",
                };
                let result = match receipt.result {
                    Some(ProviderResponse::Reply(value)) => Some(value),
                    Some(ProviderResponse::Rejected(value)) => Some(json!({"rejected":value})),
                    None => None,
                };
                let exact = json!({"operation":receipt.operation,"scope":receipt.scope,"certainty":certainty,
                    "ticket":receipt.ticket.map(|ticket| json!({
                        "generation": ticket.generation,
                        "id": ticket.id,
                        "method": ticket.method.as_str(),
                    })),
                    "notSentReason":receipt.not_sent_reason.map(|reason| format!("{reason:?}")),
                    "result":result});
                TransferDispatch {
                    receipt: exact,
                    result,
                    certainty,
                }
            }
            Err(_) => TransferDispatch {
                receipt: json!({"operation":ticket.request,"scope":scope,"certainty":"not_sent","ticket":null}),
                result: None,
                certainty: "not_sent",
            },
        }
    }
}

pub fn before_start(
    store: &RuntimeStore,
    session: Option<Arc<dyn AccountSessionPort>>,
    catalog: &ModelCatalogService,
    preparation: &crate::account_worker::preparation::PreparationResources,
    agent_id: &str,
    now: f64,
) -> Result<Value, TransferError> {
    let agent = studio_runtime_store::AgentRepository::get(
        &store.read()?,
        &studio_runtime_store::AgentId::from(agent_id),
    )?
    .ok_or(TransferError::Refused("Unknown transfer member"))?
    .value();
    let lazy = agent
        .get("lazyAccountTransfer")
        .filter(|value| !value.is_null());
    if let Some(lazy) = lazy {
        let operation = lazy["id"]
            .as_str()
            .ok_or(TransferError::Refused("Invalid lazy transfer identity"))?;
        let session = session.ok_or(TransferError::Requires("provider-session/account-worker"))?;
        let executor = AccountTransferExecutor {
            store,
            session,
            catalog,
            preparation,
            now,
        };
        let result = driver::execute_member(store, &executor, operation, agent_id, now)?;
        if result == driver::DispatchOutcome::ReceiptSaved {
            driver::adopt_receipt(store, operation, agent_id, now)?;
        }
    } else {
        crate::account_transfer::before_start(&agent)?;
    }
    Ok(studio_runtime_store::AgentRepository::get(
        &store.read()?,
        &studio_runtime_store::AgentId::from(agent_id),
    )?
    .ok_or(TransferError::Refused("Unknown transfer member"))?
    .value())
}

#[cfg(test)]
mod destination_settings_tests {
    use super::*;

    #[test]
    fn missing_default_and_fallback_model_refuses_instead_of_selecting_first() {
        let catalog = CatalogObservation {
            identity: CatalogIdentity {
                account_key: "target".into(),
                home: "/tmp/target".into(),
                provider: "codex".into(),
                generation: 1,
            },
            expires_at: Instant::now() + Duration::from_secs(30),
            models: vec![ModelCapabilities {
                model: "visible-but-not-default".into(),
                resolved_model: None,
                is_default: false,
                hidden: false,
                access_programs: None,
                supported_efforts: vec!["medium".into()],
                default_effort: Some("medium".into()),
                priority: false,
            }],
        };
        let result = destination_settings(
            &json!({"model":"source-model"}),
            "claude",
            "codex",
            &catalog,
        );
        assert!(matches!(
            result,
            Err(TransferError::Refused(
                "The destination account has no available default model"
            ))
        ));
    }
}
