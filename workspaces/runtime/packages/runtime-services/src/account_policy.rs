//! Read-only projections of startup-loaded account and usage state.

use serde::Deserialize;
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::sync::OnceLock;
use thiserror::Error;

const REGISTRY_SCHEMA: &str = include_str!("../../runtime-http/account-registry.schema.json");
const LIMITS_SCHEMA: &str = include_str!("../../runtime-http/workspace-usage-limits.schema.json");
pub const AUTHENTICATION_RECOVERY_MESSAGE: &str =
    "Sign in again to this account. Chats will continue when sign-in works.";

fn compile_validator(schema_text: &str) -> Result<jsonschema::Validator, ()> {
    let schema: Value = serde_json::from_str(schema_text).map_err(|_| ())?;
    jsonschema::validator_for(&schema).map_err(|_| ())
}

fn registry_violation(value: &Value) -> Result<Option<(String, String)>, ()> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, ()>> = OnceLock::new();
    let validator = VALIDATOR
        .get_or_init(|| compile_validator(REGISTRY_SCHEMA))
        .as_ref()
        .map_err(|_| ())?;
    Ok(validator.iter_errors(value).next().map(|error| {
        (
            error.instance_path().to_string(),
            error.kind().keyword().to_owned(),
        )
    }))
}

fn limits_violation(value: &Value) -> Result<Option<(String, String)>, ()> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, ()>> = OnceLock::new();
    let validator = VALIDATOR
        .get_or_init(|| compile_validator(LIMITS_SCHEMA))
        .as_ref()
        .map_err(|_| ())?;
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
    Ok(validator.iter_errors(&wrapped).next().map(|error| {
        (
            error.instance_path().to_string(),
            error.kind().keyword().to_owned(),
        )
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryDocument {
    default_account_key: String,
    accounts: BTreeMap<String, Value>,
    #[serde(default)]
    logins: BTreeMap<String, Value>,
}

#[derive(Clone, Default)]
pub(crate) struct RegistryOrder {
    pub(crate) accounts: Vec<String>,
    pub(crate) logins: Vec<String>,
}

pub(crate) fn decode_registry(bytes: &[u8]) -> Result<(Value, RegistryOrder), (String, String)> {
    if bytes.len() > studio_runtime_store::account_state::MAX_PERSISTED_DOCUMENT_BYTES
        || !studio_runtime_store::account_state::persisted_document_within_bounds(bytes)
    {
        return Err(("/".to_owned(), "resourceLimit".to_owned()));
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| ("/".to_owned(), "invalid JSON".to_owned()))?;
    match registry_violation(&value) {
        Ok(Some((pointer, _keyword))) => {
            return Err((pointer, "schema validation failed".to_owned()));
        }
        Err(()) => return Err(("/".to_owned(), "schema validation failed".to_owned())),
        Ok(None) => {}
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let order = RegistryOrder::deserialize(&mut deserializer)
        .map_err(|_| ("/".to_owned(), "invalid registry order".to_owned()))?;
    Ok((value, order))
}

pub(crate) fn encode_registry(value: &Value, order: &RegistryOrder) -> Result<Vec<u8>, ()> {
    let Some(root) = value.as_object() else {
        return Err(());
    };
    let Some(accounts) = root.get("accounts").and_then(Value::as_object) else {
        return Err(());
    };
    let logins = root.get("logins").and_then(Value::as_object);
    let mut output = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut output);
    let mut map = serializer.serialize_map(Some(root.len())).map_err(|_| ())?;
    for (key, field) in root {
        match key.as_str() {
            "accounts" => map.serialize_entry(
                key,
                &OrderedRegistryMap {
                    values: accounts,
                    keys: &order.accounts,
                },
            ),
            "logins" => map.serialize_entry(
                key,
                &OrderedRegistryMap {
                    values: logins.ok_or(())?,
                    keys: &order.logins,
                },
            ),
            _ => map.serialize_entry(key, field),
        }
        .map_err(|_| ())?;
    }
    map.end().map_err(|_| ())?;
    Ok(output)
}

struct OrderedRegistryMap<'a> {
    values: &'a Map<String, Value>,
    keys: &'a [String],
}

impl Serialize for OrderedRegistryMap<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.values.len()))?;
        let mut emitted = HashSet::new();
        for key in self.keys {
            if let Some(value) = self.values.get(key) {
                map.serialize_entry(key, value)?;
                emitted.insert(key);
            }
        }
        for (key, value) in self.values {
            if !emitted.contains(key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for RegistryOrder {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RootVisitor;
        impl<'de> serde::de::Visitor<'de> for RootVisitor {
            type Value = RegistryOrder;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("account registry object")
            }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut result = RegistryOrder::default();
                let mut seen = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate registry field"));
                    }
                    match key.as_str() {
                        "accounts" => result.accounts = map.next_value_seed(KeysSeed)?,
                        "logins" => result.logins = map.next_value_seed(KeysSeed)?,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(result)
            }
        }
        deserializer.deserialize_map(RootVisitor)
    }
}

struct KeysSeed;
impl<'de> serde::de::DeserializeSeed<'de> for KeysSeed {
    type Value = Vec<String>;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct KeysVisitor;
        impl<'de> serde::de::Visitor<'de> for KeysVisitor {
            type Value = Vec<String>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("ordered object")
            }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut keys = Vec::new();
                let mut seen = HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate registry key"));
                    }
                    map.next_value::<serde::de::IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(keys)
            }
        }
        deserializer.deserialize_map(KeysVisitor)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LimitsDiagnostic {
    pub workspace_row: String,
    pub account_key: String,
    pub pointer: String,
    pub keyword: String,
}

#[derive(Clone)]
pub struct AccountsSnapshot {
    pub accounts: Vec<Value>,
    pub archived_accounts: Vec<Value>,
    pub default_account_key: Value,
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
    let registry: RegistryDocument =
        serde_json::from_value(value).map_err(|_| registry_error("/", "typed decoding failed"))?;
    let mut accounts = Vec::new();
    let mut archived = Vec::new();
    let mut known = HashSet::new();
    for key in order.accounts {
        let Some(mut row) = registry
            .accounts
            .get(&key)
            .and_then(Value::as_object)
            .cloned()
        else {
            continue;
        };
        known.insert(key.clone());
        row.retain(|field, _| !field.starts_with('_'));
        if row.get("deleted") == Some(&Value::Bool(true)) {
            archived.push(Value::Object(row));
        } else if !key.starts_with("login-")
            || row.get("status").and_then(Value::as_str) == Some("ready")
        {
            if can_attempt_native_proof(registry.accounts.get(&key).and_then(Value::as_object)) {
                row.insert("canAttemptNativeProof".into(), Value::Bool(true));
            }
            accounts.push(Value::Object(row));
        }
    }
    let mut logins = Vec::new();
    for key in order.logins {
        if let Some(mut receipt) = registry
            .logins
            .get(&key)
            .and_then(Value::as_object)
            .cloned()
        {
            receipt.insert("requestId".into(), Value::String(key));
            logins.push(Value::Object(receipt));
        }
    }
    Ok(AccountsSnapshot {
        accounts,
        archived_accounts: archived,
        default_account_key: Value::String(registry.default_account_key),
        logins,
        known_account_keys: known,
    })
}

pub(crate) fn can_attempt_native_proof(stored: Option<&Map<String, Value>>) -> bool {
    let Some(row) = stored else { return false };
    let email = row.get("email").and_then(Value::as_str).unwrap_or_default();
    row.get("provider").and_then(Value::as_str) == Some("claude")
        && row.get("status").and_then(Value::as_str) == Some("error")
        && row.get("_authErrorKind").and_then(Value::as_str) == Some("keychain")
        && row.get("deleted") != Some(&Value::Bool(true))
        && row.get("disconnected") != Some(&Value::Bool(true))
        && row.get("duplicateOf").is_none()
        && !email.is_empty()
        && row.get("accountId").and_then(Value::as_str) == Some(&format!("claude:{email}"))
        && row.get("_credentialIdentity") == row.get("accountId")
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

    const REGISTRY: &[u8] = br#"{"version":1,"defaultAccountKey":"default","accounts":{"z":{"id":"z","home":"/z","label":"Z","source":"Codex CLI","status":"ready"},"default":{"id":"default","home":"/default","label":"Default","source":"Codex CLI","status":"signedOut"}},"logins":{"login-r":{"requestId":"login-r","accountKey":"z","status":"ready","createdAt":1.0}}}"#;

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
            r#"{{"version":1,"defaultAccountKey":"default","accounts":{{"default":{{"id":"default","unexpected":"{marker}"}}}},"logins":{{}}}}"#
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
            [("alpha".into(), json!({"accountKey":"alpha","data":{"rateLimits":{"primary":{"usedPercent":1,"notAWindowField":true}}}}))],
        ).expect("bad cache does not stop startup");
        assert_eq!(state.limits("alpha")["data"], Value::Null);
        assert_eq!(state.limits_diagnostics().len(), 1);
        assert_eq!(
            state.limits_diagnostics()[0].workspace_row,
            "workspace/current"
        );
    }

    #[test]
    fn storage_extension_rejected_by_python_restore_is_an_uncached_limit() {
        let input = json!({"accountKey":"default","at":1.0,"data":{"rateLimits":{"primary":{"usedPercent":12,"providerWindow":{"x":1}}},"rateLimitResetCredits":{"providerOwned":true}}});
        let state =
            AccountPolicyState::from_startup(Some(REGISTRY), [("default".into(), input.clone())])
                .expect("Python restore rejection does not stop startup");
        assert!(state.limits("default")["data"].is_null());
        assert_eq!(state.limits_diagnostics().len(), 1);
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
        let registry = format!(
            r#"{{"version":1,"defaultAccountKey":"default","accounts":{{"default":{{"id":"default","home":"/tmp","label":"{marker}","source":"Codex CLI","status":"error","error":"{marker}"}}}},"logins":{{}}}}"#
        );
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
