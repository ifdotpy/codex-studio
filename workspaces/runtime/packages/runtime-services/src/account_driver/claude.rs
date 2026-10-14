//! Verified Claude account writes. Auth metadata is supplied by the owned probe;
//! these methods never invoke a provider or read credential files.
use super::*;
use serde_json::{Map, json};

#[derive(Debug, thiserror::Error)]
pub enum ClaudeAccountError {
    #[error(transparent)]
    Account(#[from] AccountDriverError),
    #[error("Select a Claude account")]
    NotClaude,
    #[error("Restore this profile's original Claude login")]
    IdentityChanged,
    #[error("Account settings changed during sign-in")]
    SettingsChanged,
    #[error("Invalid Claude profile label or configuration")]
    InvalidProfile,
}

/// Output of `claude auth status --json`; retains metadata only.
pub fn auth_metadata_from_output(stdout: &[u8], succeeded: bool) -> AuthMetadata {
    parse_claude_status(stdout, succeeded)
}

impl<R: ProviderAuthReader> AccountDriver<R> {
    /// Saved identity only; the sign-in path must not block on a native probe.
    pub fn claude_profile(&self, key: &str) -> Result<Value, ClaudeAccountError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let row = registry_account(&state.value, key)?;
        if row.get("provider") != Some(&json!("claude")) {
            return Err(ClaudeAccountError::NotClaude);
        }
        Ok(row.clone())
    }

    pub fn find_claude_config(&self, config: &str) -> Result<Option<Value>, ClaudeAccountError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        Ok(state
            .order
            .accounts
            .iter()
            .filter_map(|key| state.value.get("accounts")?.get(key))
            .find(|row| {
                row.get("provider") == Some(&json!("claude"))
                    && row
                        .pointer("/claudeOptions/configDir")
                        .and_then(Value::as_str)
                        == Some(config)
            })
            .cloned())
    }

    /// The caller owns executable/path resolution and a per-config login lease.
    /// A durable registration is also the restart receipt for Add's config path.
    pub fn register_claude_verified(
        &self,
        mut options: Map<String, Value>,
        label: &str,
        default_home: &Path,
        metadata: &AuthMetadata,
    ) -> Result<String, ClaudeAccountError> {
        if metadata.status != AuthStatus::Ready
            || metadata.email.as_deref().is_none_or(str::is_empty)
            || metadata.account_id
                != metadata
                    .email
                    .as_ref()
                    .map(|email| format!("claude:{email}"))
        {
            return Err(AccountDriverError::SignInRequired.into());
        }
        if label.trim().is_empty()
            || label.chars().count() > 200
            || !studio_runtime_schema::validate(
                "account-registry/ClaudeOptionsRecord",
                &Value::Object(options.clone()),
            )
            .map_err(|_| ClaudeAccountError::InvalidProfile)?
        {
            return Err(ClaudeAccountError::InvalidProfile);
        }
        options.entry("customModels").or_insert_with(|| json!([]));
        let identity = json!({"binaryPath":options.get("binaryPath").cloned().unwrap_or(json!("")),
            "configDir":options.get("configDir").cloned().unwrap_or(json!(""))});
        let digest = crate::event_write::request_signature(&identity);
        let key = format!("claude-profile-{}", &digest[..20]);
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let mut order = state.order.clone();
        if let Some(existing) = updated.get("accounts").and_then(|a| a.get(&key)) {
            if existing.get("status") != Some(&json!("ready"))
                || [
                    ("accountId", metadata.account_id.as_ref()),
                    ("_credentialIdentity", metadata.credential_identity.as_ref()),
                ]
                .iter()
                .any(|(field, expected)| {
                    existing.get(*field).is_some_and(|v| {
                        python_truthy(v) && v.as_str() != expected.map(String::as_str)
                    })
                })
            {
                return Err(ClaudeAccountError::IdentityChanged);
            }
            if existing.get("claudeOptions") != Some(&Value::Object(options)) {
                return Err(ClaudeAccountError::SettingsChanged);
            }
            registry_account_mut(&mut updated, &key)?
                .as_object_mut()
                .ok_or(AccountDriverError::InvalidRegistry)?
                .remove("deleted");
            if updated != state.value {
                self.persist(&mut state, updated)?;
            }
            return Ok(key);
        }
        if let Some(local) = updated
            .get_mut("accounts")
            .and_then(|a| a.get_mut("claude-local"))
            .and_then(Value::as_object_mut)
        {
            let local_options = local.get("claudeOptions").and_then(Value::as_object);
            let same_paths = ["binaryPath", "configDir"].iter().all(|k| {
                local_options.and_then(|o| o.get(*k)).unwrap_or(&json!(""))
                    == options.get(*k).unwrap_or(&json!(""))
            });
            let same_login = local
                .get("accountId")
                .is_none_or(|v| !python_truthy(v) || v.as_str() == metadata.account_id.as_deref());
            if local.get("provider") == Some(&json!("claude")) && same_paths && same_login {
                if local_options.is_some_and(|o| !o.is_empty() && o != &options) {
                    return Err(ClaudeAccountError::SettingsChanged);
                }
                if !options.is_empty() {
                    local.insert("claudeOptions".into(), Value::Object(options));
                }
                apply_registration_metadata(local, metadata);
                local.insert("label".into(), json!(label.trim()));
                local.remove("deleted");
                self.persist(&mut state, updated)?;
                return Ok("claude-local".into());
            }
        }
        let home = options
            .get("configDir")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| default_home.to_string_lossy().into_owned());
        let mut row = Map::from_iter([
            ("id".into(), json!(key)),
            ("provider".into(), json!("claude")),
            ("claudeOptions".into(), Value::Object(options)),
            ("home".into(), json!(home)),
            ("label".into(), json!(label.trim())),
            ("source".into(), json!("Claude Code")),
        ]);
        apply_registration_metadata(&mut row, metadata);
        updated
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .ok_or(AccountDriverError::InvalidRegistry)?
            .insert(key.clone(), Value::Object(row));
        order.accounts.push(key.clone());
        persist_registry(&self.path, &mut state, updated, &order)?;
        Ok(key)
    }

    /// Compare identity/options under the same lock as the registry replacement.
    pub fn refresh_claude_verified(
        &self,
        key: &str,
        expected: &Value,
        metadata: &AuthMetadata,
        reconnect: bool,
    ) -> Result<Value, ClaudeAccountError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let row = registry_account_mut(&mut updated, key)?
            .as_object_mut()
            .ok_or(AccountDriverError::InvalidRegistry)?;
        if row.get("provider") != Some(&json!("claude")) {
            return Err(ClaudeAccountError::NotClaude);
        }
        if ["accountId", "claudeOptions"]
            .iter()
            .any(|k| row.get(*k) != expected.get(*k))
        {
            return Err(ClaudeAccountError::SettingsChanged);
        }
        apply_metadata(row, metadata);
        if reconnect && row.get("status") == Some(&json!("ready")) {
            row.remove("disconnected");
        }
        let result = Value::Object(row.clone());
        if updated != state.value {
            self.persist(&mut state, updated)?;
        }
        Ok(result)
    }
}

pub(super) const SUBSCRIPTION_ENV_REMOVALS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
    "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

pub fn subscription_environment(
    mut environment: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> BTreeMap<std::ffi::OsString, std::ffi::OsString> {
    for key in SUBSCRIPTION_ENV_REMOVALS {
        environment.remove(std::ffi::OsStr::new(key));
    }
    environment
}

/// Resolve using the same profile/override/PATH policy as account status probes.
pub fn login_binary(
    environment: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> Option<PathBuf> {
    resolve_login_binary(None, environment)
}
pub(crate) fn resolve_login_binary(
    configured: Option<&str>,
    environment: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
) -> Option<PathBuf> {
    let value = |key: &str| {
        environment
            .get(std::ffi::OsStr::new(key))
            .map(std::ffi::OsString::as_os_str)
    };
    super::resolve_claude_binary(
        configured,
        value(super::CLAUDE_BIN_OVERRIDE).and_then(std::ffi::OsStr::to_str),
        value("PATH"),
        value("HOME"),
    )
    .map(Into::into)
}

impl<R: ProviderAuthReader> AccountDriver<R> {
    /// Restart-only proof for a ready login whose chat repair has not committed.
    /// Read outside the registry lock, then compare the pinned profile before publication.
    pub(crate) fn revalidate_claude(&self, key: &str) -> Result<Value, ClaudeAccountError> {
        let expected = self.claude_profile(key)?;
        let metadata = self.reader.read(&AuthProfile {
            provider: "claude".into(),
            home: expected
                .get("home")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            claude_options: expected.get("claudeOptions").cloned(),
        });
        self.refresh_claude_verified(key, &expected, &metadata, false)
    }
}
