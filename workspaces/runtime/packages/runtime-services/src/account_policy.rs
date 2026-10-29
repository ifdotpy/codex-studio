//! Read-only projections of startup-loaded account and usage state.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use studio_runtime_providers::ProviderKind;
use thiserror::Error;

pub const AUTHENTICATION_RECOVERY_MESSAGE: &str =
    "Sign in again to this account. Chats will continue when sign-in works.";

fn limits_violation(value: &Value) -> Result<Option<(String, String)>, ()> {
    let mut snapshots = Map::new();
    snapshots.insert(
        value
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        value.clone(),
    );
    let wrapped = serde_json::json!({"rateLimitsByAccount": snapshots});
    studio_runtime_schema::first_error(
        "workspace-usage-limits/UsageLimitsWorkspaceEnvelope",
        &wrapped,
    )
    .map_err(|_| ())
}

pub(crate) use studio_runtime_store::account_registry::{
    RegistryOrder, decode_registry, encode_registry,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitsDiagnostic {
    pub workspace_row: String,
    pub account_key: String,
    pub pointer: String,
    pub keyword: String,
}

#[derive(Clone, Eq, PartialEq)]
pub struct AccountsSnapshot {
    pub accounts: Vec<Value>,
    pub archived_accounts: Vec<Value>,
    pub default_account_key: Value,
    /// The default account order (`defaultAccountGroups`) from the account order
    /// file, see [`crate::account_preference::AccountOrder`].
    pub default_account_groups: Vec<String>,
    pub logins: Vec<Value>,
    pub known_account_keys: HashSet<String>,
}

pub fn apply_authentication_recovery(
    snapshot: &mut AccountsSnapshot,
    waiting_account_keys: &[String],
) {
    for account in &mut snapshot.accounts {
        if account
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|key| waiting_account_keys.iter().any(|waiting| waiting == key))
            && let Some(account) = account.as_object_mut()
        {
            account.insert(
                "authenticationRecovery".to_owned(),
                Value::String(AUTHENTICATION_RECOVERY_MESSAGE.to_owned()),
            );
        }
    }
}

impl fmt::Debug for AccountsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountsSnapshot")
            .field("account_count", &self.accounts.len())
            .field("archived_count", &self.archived_accounts.len())
            .field("login_count", &self.logins.len())
            .field("known_account_keys", &self.known_account_keys)
            .finish()
    }
}

/// Immutable, schema-validated startup projection. Bad limit entries are
/// omitted and retained as value-free diagnostics for startup logging.
#[derive(Clone)]
pub struct AccountPolicyState {
    accounts: AccountsSnapshot,
    limits: BTreeMap<String, Value>,
    limits_diagnostics: Vec<LimitsDiagnostic>,
}

impl fmt::Debug for AccountPolicyState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountPolicyState")
            .field("account_count", &self.accounts.accounts.len())
            .field(
                "limit_account_keys",
                &self.limits.keys().collect::<Vec<_>>(),
            )
            .field("limits_diagnostic_count", &self.limits_diagnostics.len())
            .finish()
    }
}

impl AccountPolicyState {
    /// Validate and project startup snapshots once; request handlers only clone
    /// already-validated immutable data.
    pub fn from_startup(
        registry_bytes: Option<&[u8]>,
        limits_snapshots: impl IntoIterator<Item = (String, Value)>,
    ) -> Result<Self, AccountPolicyError> {
        let accounts = project_registry(registry_bytes)?;
        let mut limits = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for (key, candidate) in limits_snapshots {
            if candidate.get("accountKey").and_then(Value::as_str) != Some(key.as_str()) {
                diagnostics.push(limit_diagnostic(&key, "/accountKey", "const"));
                continue;
            }
            match limits_violation(&candidate) {
                Ok(Some((pointer, keyword))) => {
                    diagnostics.push(limit_diagnostic(&key, &pointer, &keyword));
                    continue;
                }
                Err(()) => {
                    diagnostics.push(limit_diagnostic(&key, "/", "schemaCompile"));
                    continue;
                }
                Ok(None) => {}
            }
            if candidate.get("data").is_none_or(Value::is_null) {
                continue;
            }
            limits.insert(key, candidate);
        }
        Ok(Self {
            accounts,
            limits,
            limits_diagnostics: diagnostics,
        })
    }

    pub fn from_store(
        registry_bytes: Option<&[u8]>,
        read: studio_runtime_store::account_state::LimitsRead,
    ) -> Result<Self, AccountPolicyError> {
        let mut state = Self::from_startup(registry_bytes, read.snapshots)?;
        state
            .limits_diagnostics
            .extend(read.diagnostics.into_iter().map(|item| LimitsDiagnostic {
                workspace_row: item.workspace_row.to_owned(),
                account_key: item.account_key,
                pointer: item.pointer,
                keyword: item.keyword,
            }));
        Ok(state)
    }

    pub fn accounts(&self) -> AccountsSnapshot {
        self.accounts.clone()
    }

    pub fn has_account(&self, account_key: &str) -> bool {
        self.accounts.known_account_keys.contains(account_key)
    }

    pub fn limits(&self, account_key: &str) -> Value {
        self.limits.get(account_key).cloned().unwrap_or_else(
            || serde_json::json!({"accountKey":account_key,"data":null,"at":null,"error":null}),
        )
    }

    pub fn limits_diagnostics(&self) -> &[LimitsDiagnostic] {
        &self.limits_diagnostics
    }
}

fn limit_diagnostic(key: &str, pointer: &str, keyword: &str) -> LimitsDiagnostic {
    LimitsDiagnostic {
        workspace_row: "workspace/current".to_owned(),
        account_key: key.to_owned(),
        pointer: pointer.to_owned(),
        keyword: keyword.to_owned(),
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum AccountPolicyError {
    #[error("Cannot read state/accounts/registry.json at {pointer}: {reason}")]
    InvalidRegistry {
        pointer: String,
        reason: &'static str,
    },
}

pub(crate) fn project_registry(
    bytes: Option<&[u8]>,
) -> Result<AccountsSnapshot, AccountPolicyError> {
    let Some(bytes) = bytes else {
        return Err(registry_error("/", "registry is absent"));
    };
    let (value, order) = decode_registry(bytes)
        .map_err(|(pointer, _reason)| registry_error(&pointer, "schema validation failed"))?;
    project_registry_value(&value, &order)
}

pub(crate) fn project_registry_value(
    value: &Value,
    order: &RegistryOrder,
) -> Result<AccountsSnapshot, AccountPolicyError> {
    let typed: studio_runtime_schema::json_object::JsonObject<
        studio_runtime_schema::account_registry::AccountRegistry,
    > = serde::Deserialize::deserialize(value)
        .map_err(|_| registry_error("/", "schema validation failed"))?;
    let typed = typed.0;
    let accounts_by_key = value
        .get("accounts")
        .and_then(Value::as_object)
        .ok_or_else(|| registry_error("/accounts", "schema validation failed"))?;
    let logins_by_key = value
        .get("logins")
        .and_then(Value::as_object)
        .ok_or_else(|| registry_error("/logins", "schema validation failed"))?;
    let default_account_key = value
        .get("defaultAccountKey")
        .and_then(Value::as_str)
        .ok_or_else(|| registry_error("/defaultAccountKey", "schema validation failed"))?;
    let mut accounts = Vec::new();
    let mut archived = Vec::new();
    let mut known = HashSet::new();
    for key in &order.accounts {
        let Some(mut row) = accounts_by_key.get(key).and_then(Value::as_object).cloned() else {
            continue;
        };
        let Some(account) = typed.accounts.get(key).map(|account| &account.0) else {
            continue;
        };
        let provider = account.provider;
        row.insert("provider".into(), Value::String(provider.key().into()));
        known.insert(key.clone());
        if provider == ProviderKind::Claude
            && let Some(organization) = crate::account_driver::claude_identity::organization(
                account.credential_identity.as_deref(),
                account.organization_name.as_deref(),
            )
        {
            row.insert("organization".into(), organization);
        }
        row.retain(|field, _| !field.starts_with('_'));
        if account.deleted == Some(true) {
            archived.push(Value::Object(row));
        } else if listed_account(key, account) {
            if can_attempt_native_proof_typed(account) {
                row.insert("canAttemptNativeProof".into(), Value::Bool(true));
            }
            accounts.push(Value::Object(row));
        }
    }
    let mut logins = Vec::new();
    for key in &order.logins {
        if let Some(mut receipt) = logins_by_key.get(key).and_then(Value::as_object).cloned() {
            receipt.insert("requestId".into(), Value::String(key.clone()));
            logins.push(Value::Object(receipt));
        }
    }
    Ok(AccountsSnapshot {
        accounts,
        archived_accounts: archived,
        default_account_key: Value::String(default_account_key.to_owned()),
        // The account order file supplies the default order (`AccountOrder::apply`).
        default_account_groups: Vec::new(),
        logins,
        known_account_keys: known,
    })
}

/// Whether a non-archived registry row is in the account list. A row made by
/// a Studio sign-in (`login-` key) is listed once it is ready, and also later
/// when it has a provider identity, so a signed-out account keeps its
/// sign-in action. A sign-in row without an identity (in progress, failed or
/// cancelled) stays hidden, and so does a non-ready duplicate row.
fn listed_account(key: &str, row: &studio_runtime_schema::account_registry::AccountRecord) -> bool {
    !key.starts_with("login-")
        || row.status == studio_runtime_schema::account_registry::AccountStatus::Ready
        || (row.account_id.as_deref().is_some_and(|id| !id.is_empty())
            && row
                .duplicate_of
                .as_ref()
                .and_then(Option::as_deref)
                .is_none_or(str::is_empty))
}

pub(crate) fn can_attempt_native_proof(stored: Option<&Map<String, Value>>) -> bool {
    stored
        .and_then(|row| {
            <studio_runtime_schema::json_object::JsonObject<
                studio_runtime_schema::account_registry::AccountRecord,
            > as serde::Deserialize>::deserialize(Value::Object(row.clone()))
            .ok()
        })
        .is_some_and(|row| can_attempt_native_proof_typed(&row.0))
}

fn can_attempt_native_proof_typed(
    row: &studio_runtime_schema::account_registry::AccountRecord,
) -> bool {
    let email = row.email.as_deref().unwrap_or_default();
    row.provider == ProviderKind::Claude
        && row.status == studio_runtime_schema::account_registry::AccountStatus::Error
        && row.auth_error_kind.as_deref() == Some("keychain")
        && row.deleted != Some(true)
        && row.disconnected != Some(true)
        && row.duplicate_of.is_none()
        && !email.is_empty()
        && row.account_id.as_deref() == Some(&format!("claude:{email}"))
        && row.credential_identity.as_deref().is_some_and(|identity| {
            crate::account_driver::claude_identity::belongs_to(identity, email)
        })
}

fn registry_error(pointer: &str, reason: &'static str) -> AccountPolicyError {
    AccountPolicyError::InvalidRegistry {
        pointer: pointer.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountPolicyError, AccountPolicyState};
    use serde_json::{Value, json};

    const REGISTRY: &[u8] = br#"{"version":2,"defaultAccountKey":"default","accounts":{"z":{"id":"z","provider":"codex","home":"/z","label":"Z","source":"Codex CLI","status":"ready"},"default":{"id":"default","provider":"codex","home":"/default","label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{"login-r":{"requestId":"login-r","accountKey":"z","status":"ready","createdAt":1.0}}}"#;

    #[test]
    fn startup_projects_registry_in_file_order_and_never_reads_auth() {
        let state = AccountPolicyState::from_startup(Some(REGISTRY), []).expect("valid registry");
        let snapshot = state.accounts();
        assert_eq!(snapshot.accounts[0]["id"], "z");
        assert_eq!(snapshot.accounts[1]["id"], "default");
        assert_eq!(snapshot.logins[0]["requestId"], "login-r");
        assert!(state.has_account("default"));
        assert!(!state.has_account("missing"));
    }

    #[test]
    fn invalid_registry_fails_closed_with_location_and_no_input_values() {
        let marker = "DO_NOT_ECHO";
        let bytes = format!(
            r#"{{"version":2,"defaultAccountKey":"default","accounts":{{"default":{{"id":"default","unexpected":"{marker}"}}}},"logins":{{}}}}"#
        );
        let error = AccountPolicyState::from_startup(Some(bytes.as_bytes()), [])
            .expect_err("unknown fields rejected");
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(marker));
        assert!(rendered.contains("/accounts/default"));
    }

    #[test]
    fn invalid_limit_is_diagnostic_and_absent_cache_uses_default_shape() {
        let state = AccountPolicyState::from_startup(
            Some(REGISTRY),
            [("alpha".into(), json!({"accountKey":"alpha","data":{"rateLimits":{"primary":{"usedPercent":"1","providerWindow":{"x":1}}}}}))],
        ).expect("bad cache does not stop startup");
        assert_eq!(state.limits("alpha")["data"], Value::Null);
        assert_eq!(state.limits_diagnostics().len(), 1);
        assert_eq!(
            state.limits_diagnostics()[0].workspace_row,
            "workspace/current"
        );
        assert_eq!(
            state.limits_diagnostics()[0].pointer,
            "/rateLimitsByAccount/alpha/data"
        );
    }

    #[test]
    fn startup_preserves_vendor_window_and_reset_credit_extensions() {
        let input = json!({"accountKey":"default","at":1.0,"data":{"rateLimits":{"primary":{"usedPercent":12,"providerWindow":{"x":1}}},"rateLimitResetCredits":{"providerOwned":true}}});
        let state =
            AccountPolicyState::from_startup(Some(REGISTRY), [("default".into(), input.clone())])
                .expect("provider extensions survive startup");
        assert_eq!(state.limits("default"), input);
        assert!(state.limits_diagnostics().is_empty());
    }

    #[test]
    fn missing_registry_fails_closed_instead_of_synthesizing_account_data() {
        let error = AccountPolicyState::from_startup(None, []).expect_err("missing registry");
        assert!(error.to_string().contains("registry is absent"));
        let error =
            AccountPolicyState::from_startup(Some(b"{}"), []).expect_err("invalid registry");
        assert!(matches!(error, AccountPolicyError::InvalidRegistry { .. }));
    }

    #[test]
    fn debug_summaries_do_not_print_registry_or_limit_values_or_numeric_bytes() {
        let marker = "PRIVATE_ACCOUNT_MARKER_49371";
        let registry = json!({
            "version": studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,
            "defaultAccountKey": "default",
            "accounts": {"default": {
                "id": "default", "provider": "codex", "home": std::env::temp_dir(),
                "label": marker, "source": "Codex CLI", "status": "error", "error": marker
            }},
            "logins": {}
        })
        .to_string();
        let limits = json!({
            "accountKey":"default", "at": 1.0, "data":{"extra":marker}, "error":marker
        });
        let state = AccountPolicyState::from_startup(
            Some(registry.as_bytes()),
            [("default".into(), limits)],
        )
        .expect("valid data");
        let rendered = format!("{state:?} {:?}", state.accounts());
        let bytes = marker
            .as_bytes()
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        assert!(!rendered.contains(marker));
        assert!(!rendered.contains(&bytes));
    }
}
