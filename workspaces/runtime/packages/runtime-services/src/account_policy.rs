//! Read-only projections of startup-loaded account and usage state.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

/// A public string field retains malformed and null values for historical consumers.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum PublicString {
    #[default]
    Missing,
    String(String),
    Other(Value),
}
impl PublicString {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            Self::Missing | Self::Other(_) => None,
        }
    }
    pub fn equals_json(&self, other: &Value) -> bool {
        match self {
            Self::Missing => other.is_null(),
            Self::String(value) => other.as_str() == Some(value),
            Self::Other(value) => value == other,
        }
    }
    pub fn is_present_non_null(&self) -> bool {
        !matches!(self, Self::Missing | Self::Other(Value::Null))
    }
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}
impl Serialize for PublicString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_unit(),
            Self::String(value) => value.serialize(serializer),
            Self::Other(value) => value.serialize(serializer),
        }
    }
}
impl<'de> Deserialize<'de> for PublicString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::String(value) => Self::String(value),
            value @ (Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::Array(_)
            | Value::Object(_)) => Self::Other(value),
        })
    }
}

/// A public boolean field retains non-boolean values for legacy consumers.
#[derive(Clone, PartialEq, Eq, Default)]
pub enum PublicBoolean {
    #[default]
    Missing,
    Bool(bool),
    Other(Value),
}
impl PublicBoolean {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            Self::Missing | Self::Other(_) => None,
        }
    }
    pub fn truthy(&self) -> bool {
        match self {
            Self::Missing => false,
            Self::Bool(value) => *value,
            Self::Other(value) => crate::event_write::truthy(Some(value)),
        }
    }
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}
impl Serialize for PublicBoolean {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_unit(),
            Self::Bool(value) => value.serialize(serializer),
            Self::Other(value) => value.serialize(serializer),
        }
    }
}
impl<'de> Deserialize<'de> for PublicBoolean {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::Bool(value) => Self::Bool(value),
            value @ (Value::Null
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_)) => Self::Other(value),
        })
    }
}

fn public_string(value: Option<&Value>) -> PublicString {
    match value {
        None => PublicString::Missing,
        Some(Value::String(value)) => PublicString::String(value.clone()),
        Some(value) => PublicString::Other(value.clone()),
    }
}
fn public_boolean(value: Option<&Value>) -> PublicBoolean {
    match value {
        None => PublicBoolean::Missing,
        Some(Value::Bool(value)) => PublicBoolean::Bool(*value),
        Some(value) => PublicBoolean::Other(value.clone()),
    }
}

/// Exact provider options retain extensions and field presence for the public wire.
/// The separate projection serves profile lookup without reparsing the options.
#[derive(Clone, Eq)]
pub struct PublicClaudeOptions {
    raw: Value,
    pub config_dir: PublicString,
    pub binary_path: PublicString,
    pub launch_args: PublicString,
}
impl PartialEq for PublicClaudeOptions {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeConfigView {
    #[serde(default)]
    config_dir: PublicString,
    #[serde(default)]
    binary_path: PublicString,
    #[serde(default)]
    launch_args: PublicString,
}
impl PublicClaudeOptions {
    pub fn wire_object(&self) -> Option<Map<String, Value>> {
        self.raw.as_object().cloned()
    }
}
impl Serialize for PublicClaudeOptions {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.raw.serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for PublicClaudeOptions {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        let view = if raw.is_object() {
            ClaudeConfigView::deserialize(&raw).map_err(serde::de::Error::custom)?
        } else {
            ClaudeConfigView::default()
        };
        Ok(Self {
            raw,
            config_dir: view.config_dir,
            binary_path: view.binary_path,
            launch_args: view.launch_args,
        })
    }
}

/// Partial public throttle observation; extensions and malformed fields remain visible.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicThrottleFields {
    #[serde(default, skip_serializing_if = "PublicNumber::is_missing")]
    pub burst: PublicNumber,
    #[serde(default, skip_serializing_if = "PublicNumber::is_missing")]
    pub pause_seconds: PublicNumber,
    #[serde(default, skip_serializing_if = "PublicNumber::is_missing")]
    pub max_running_turns: PublicNumber,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}
#[derive(Clone, PartialEq, Eq, Default)]
pub enum PublicNumber {
    #[default]
    Missing,
    Number(serde_json::Number),
    Other(Value),
}
impl PublicNumber {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Number(value) => value.as_u64(),
            Self::Missing | Self::Other(_) => None,
        }
    }
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}
impl Serialize for PublicNumber {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Missing => serializer.serialize_unit(),
            Self::Number(value) => value.serialize(serializer),
            Self::Other(value) => value.serialize(serializer),
        }
    }
}
impl<'de> Deserialize<'de> for PublicNumber {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Value::deserialize(deserializer)? {
            Value::Number(value) => Self::Number(value),
            value @ (Value::Null
            | Value::Bool(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_)) => Self::Other(value),
        })
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PublicThrottle {
    Fields(studio_runtime_schema::json_object::JsonObject<PublicThrottleFields>),
    Other(Value),
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PublicOrganizationMismatch {
    Checked(
        studio_runtime_schema::json_object::JsonObject<
            studio_runtime_schema::account_registry::OrganizationMismatch,
        >,
    ),
    Other(Value),
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PublicOrganization {
    Checked(studio_runtime_schema::account_registry::Organization),
    Other(Value),
}
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

/// Public account projection. Optional outer values preserve field presence.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicAccount {
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub id: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub home: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub label: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub source: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub status: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub provider: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub account_id: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub email: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub error: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub plan: PublicString,
    #[serde(default, skip_serializing_if = "PublicBoolean::is_missing")]
    pub deleted: PublicBoolean,
    #[serde(default, skip_serializing_if = "PublicBoolean::is_missing")]
    pub disconnected: PublicBoolean,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub duplicate_of: PublicString,
    /// Provider-owned options, including arbitrary extension fields.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub claude_options: Option<PublicClaudeOptions>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub start_throttle: Option<PublicThrottle>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub organization_mismatch: Option<PublicOrganizationMismatch>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub organization: Option<PublicOrganization>,
    #[serde(default, skip_serializing_if = "PublicBoolean::is_missing")]
    pub can_attempt_native_proof: PublicBoolean,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub group: PublicString,
    #[serde(default, skip_serializing_if = "PublicString::is_missing")]
    pub authentication_recovery: PublicString,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

impl PublicAccount {
    pub fn provider(
        &self,
    ) -> Result<ProviderKind, studio_runtime_schema::provider_kind::UnknownProvider> {
        use studio_runtime_schema::provider_kind::UnknownProvider;
        match &self.provider {
            PublicString::String(value) => value.parse(),
            PublicString::Missing => Err(UnknownProvider(None)),
            PublicString::Other(value) => Err(UnknownProvider(Some(value.to_string()))),
        }
    }
    pub fn provider_kind(&self) -> Option<ProviderKind> {
        self.provider().ok()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AccountsSnapshot {
    pub accounts: Vec<PublicAccount>,
    pub archived_accounts: Vec<PublicAccount>,
    pub default_account_key: Value,
    /// The default account order (`defaultAccountGroups`) from the account order
    /// file, see [`crate::account_preference::AccountOrder`].
    pub default_account_groups: Vec<String>,
    pub logins: Vec<studio_runtime_schema::account_registry::LoginReceipt>,
    pub known_account_keys: HashSet<String>,
}

pub fn apply_authentication_recovery(
    snapshot: &mut AccountsSnapshot,
    waiting_account_keys: &[String],
) {
    for account in &mut snapshot.accounts {
        if waiting_account_keys
            .iter()
            .any(|waiting| Some(waiting.as_str()) == account.id.as_str())
        {
            account.authentication_recovery =
                PublicString::String(AUTHENTICATION_RECOVERY_MESSAGE.to_owned());
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
    let (value, order, typed) =
        studio_runtime_store::account_registry::decode_registry_typed(bytes)
            .map_err(|(pointer, _reason)| registry_error(&pointer, "schema validation failed"))?;
    project_registry_typed(&value, &order, &typed)
}

pub(crate) fn project_registry_value(
    value: &Value,
    order: &RegistryOrder,
) -> Result<AccountsSnapshot, AccountPolicyError> {
    let typed: studio_runtime_schema::json_object::JsonObject<
        studio_runtime_schema::account_registry::AccountRegistry,
    > = serde::Deserialize::deserialize(value)
        .map_err(|_| registry_error("/", "schema validation failed"))?;
    project_registry_typed(value, order, &typed.0)
}

pub(crate) fn project_registry_typed(
    value: &Value,
    order: &RegistryOrder,
    typed: &studio_runtime_schema::account_registry::AccountRegistry,
) -> Result<AccountsSnapshot, AccountPolicyError> {
    let accounts_by_key = value
        .get("accounts")
        .and_then(Value::as_object)
        .ok_or_else(|| registry_error("/accounts", "schema validation failed"))?;
    let default_account_key = &typed.default_account_key;
    let mut accounts = Vec::new();
    let mut archived = Vec::new();
    let mut known = HashSet::new();
    for key in &order.accounts {
        let Some(row) = accounts_by_key.get(key).and_then(Value::as_object) else {
            continue;
        };
        let Some(account) = typed.accounts.get(key).map(|account| &account.0) else {
            continue;
        };
        let provider = account.provider;
        known.insert(key.clone());
        let organization = if provider == ProviderKind::Claude {
            crate::account_driver::claude_identity::organization(
                account.credential_identity.as_deref(),
                account.organization_name.as_deref(),
            )
        } else {
            None
        };
        use studio_runtime_schema::account_registry::AccountStatus;
        let status = match account.status {
            AccountStatus::Ready => "ready",
            AccountStatus::SignedOut => "signedOut",
            AccountStatus::Pending => "pending",
            AccountStatus::Error => "error",
            AccountStatus::Changed => "changed",
            AccountStatus::Duplicate => "duplicate",
        };
        let public = PublicAccount {
            id: PublicString::String(account.id.clone()),
            home: PublicString::String(account.home.clone()),
            label: PublicString::String(account.label.clone()),
            source: PublicString::String(account.source.clone()),
            status: PublicString::String(status.to_owned()),
            provider: PublicString::String(provider.key().to_owned()),
            account_id: public_string(row.get("accountId")),
            email: public_string(row.get("email")),
            error: public_string(row.get("error")),
            plan: public_string(row.get("plan")),
            deleted: public_boolean(row.get("deleted")),
            disconnected: public_boolean(row.get("disconnected")),
            duplicate_of: public_string(row.get("duplicateOf")),
            claude_options: row.get("claudeOptions").map(|raw| PublicClaudeOptions {
                raw: raw.clone(),
                launch_args: public_string(raw.get("launchArgs")),
                binary_path: account
                    .claude_options
                    .as_ref()
                    .and_then(|options| options.0.binary_path.as_ref())
                    .map_or(PublicString::Missing, |value| {
                        PublicString::String(value.clone())
                    }),
                config_dir: account
                    .claude_options
                    .as_ref()
                    .and_then(|options| options.0.config_dir.as_ref())
                    .map_or(PublicString::Missing, |value| {
                        PublicString::String(value.clone())
                    }),
            }),
            start_throttle: row
                .get("startThrottle")
                .map(|raw| match &account.start_throttle {
                    Some(value) => PublicThrottle::Fields(
                        studio_runtime_schema::json_object::JsonObject(PublicThrottleFields {
                            burst: PublicNumber::Number(value.0.burst_number().clone()),
                            pause_seconds: PublicNumber::Number(
                                value.0.pause_seconds_number().clone(),
                            ),
                            max_running_turns: PublicNumber::Number(
                                value.0.max_running_turns_number().clone(),
                            ),
                            extensions: Map::new(),
                        }),
                    ),
                    None => PublicThrottle::Other(raw.clone()),
                }),
            organization_mismatch: account
                .organization_mismatch
                .clone()
                .map(PublicOrganizationMismatch::Checked),
            organization: organization.map(PublicOrganization::Checked),
            can_attempt_native_proof: if account.deleted != Some(true)
                && can_attempt_native_proof_typed(account)
            {
                PublicBoolean::Bool(true)
            } else {
                PublicBoolean::Missing
            },
            group: PublicString::Missing,
            authentication_recovery: PublicString::Missing,
            extensions: Map::new(),
        };
        if account.deleted == Some(true) {
            archived.push(public);
        } else if listed_account(key, account) {
            accounts.push(public);
        }
    }
    let logins = project_logins(typed, order)?;
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

pub(crate) fn project_logins(
    typed: &studio_runtime_schema::account_registry::AccountRegistry,
    order: &RegistryOrder,
) -> Result<Vec<studio_runtime_schema::account_registry::LoginReceipt>, AccountPolicyError> {
    let logins = typed
        .logins
        .as_ref()
        .ok_or_else(|| registry_error("/logins", "schema validation failed"))?;
    Ok(order
        .logins
        .iter()
        .filter_map(|key| {
            let mut receipt = logins.get(key)?.0.clone();
            receipt.request_id = key.clone();
            Some(receipt)
        })
        .collect())
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

pub(crate) fn can_attempt_native_proof_typed(
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
        assert_eq!(snapshot.accounts[0].id.as_str(), Some("z"));
        assert_eq!(snapshot.accounts[1].id.as_str(), Some("default"));
        assert_eq!(snapshot.logins[0].request_id, "login-r");
        assert!(snapshot.logins[0].email.is_none());
        assert_eq!(
            snapshot.logins[0]
                .created_at
                .as_ref()
                .and_then(Option::as_ref)
                .and_then(serde_json::Number::as_f64),
            Some(1.0)
        );
        assert!(state.has_account("default"));
        assert!(!state.has_account("missing"));
    }

    #[test]
    fn public_account_projection_preserves_presence_and_nested_numbers() {
        let registry = json!({"version":2,"defaultAccountKey":"default","accounts":{
            "default":{"id":"default","provider":"codex","home":"/home","label":"A","source":"CLI","status":"ready",
                "email":null,"accountId":"id","deleted":false,
                "startThrottle":{"burst":2.0,"pauseSeconds":30.0,"maxRunningTurns":2.0},
                "claudeOptions":{"configDir":null,"vendor":{"n":1.0}},
                "organizationMismatch":{"expected":{"name":null,"idSuffix":"a"},"observed":{"name":"name","idSuffix":"b"}}}
        },"logins":{}});
        let state = AccountPolicyState::from_startup(Some(registry.to_string().as_bytes()), [])
            .expect("valid registry");
        let public = json!(&state.accounts().accounts[0]);
        assert_eq!(public, registry["accounts"]["default"]);
        assert!(public.get("plan").is_none());
        assert_eq!(public["email"], Value::Null);
        let reparsed: super::PublicAccount =
            serde::Deserialize::deserialize(public).expect("public fixture");
        assert!(reparsed == state.accounts().accounts[0]);
    }

    #[test]
    fn public_fixture_ingress_keeps_malformed_and_null_fields() {
        for row in [
            json!({"id":null,"home":5,"status":false,"provider":"future","email":7,"group":null,"extra":{"n":1.0}}),
            json!({"id":"a","provider":null,"startThrottle":{"burst":false},"organizationMismatch":null,"claudeOptions":{"configDir":7,"launchArgs":null,"vendor":[1,2]}}),
            json!({"id":"a","provider":"codex","home":"","deleted":null,"duplicateOf":null}),
        ] {
            let account: super::PublicAccount =
                serde::Deserialize::deserialize(row.clone()).expect("tolerant public row");
            assert_eq!(json!(account), row);
        }
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
