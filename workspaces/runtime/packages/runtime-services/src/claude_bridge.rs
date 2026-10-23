//! Claude admission and launch decisions for the existing provider-session transport.
//! Native credentials stay with the CLI; all account evidence is injected.
use crate::account_driver::{AuthMetadata, AuthStatus};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};
use studio_runtime_process::ProviderLaunchSpec;
#[derive(Debug, thiserror::Error)]
pub enum BridgeHold {
    #[error("Cannot verify the pinned Claude account")]
    Account,
    #[error("Install Node.js and Claude Code before starting Claude work")]
    Executable,
    #[error("Claude bridge support is missing")]
    Support,
    #[error("Claude bridge settings are invalid")]
    Options,
    #[error("Claude native account proof is pending; original input remains held")]
    AuthWait,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    provider: String,
    account_id: Option<String>,
    email: Option<String>,
    status: String,
    #[serde(default = "empty_options")]
    claude_options: Value,
}
fn empty_options() -> Value {
    json!({})
}
pub struct BridgeSnapshot {
    profile: Profile,
    raw: Value,
}
impl BridgeSnapshot {
    pub fn from_record(record: Value) -> Result<Self, BridgeHold> {
        let profile: Profile =
            serde_json::from_value(record.clone()).map_err(|_| BridgeHold::Account)?;
        if profile.provider != "claude" {
            return Err(BridgeHold::Account);
        }
        if !studio_runtime_schema::validate(
            "account-registry/ClaudeOptionsRecord",
            &profile.claude_options,
        )
        .map_err(|_| BridgeHold::Options)?
        {
            return Err(BridgeHold::Options);
        }
        Ok(Self {
            profile,
            raw: record,
        })
    }
    pub fn launch(
        &self,
        paths: ResolvedBridge,
        metadata: &AuthMetadata,
        environment: BTreeMap<String, String>,
    ) -> Result<ProviderLaunchSpec, BridgeHold> {
        let identity = if self.raw.get("_nativeAuthPending") == Some(&json!(true)) {
            let email = self
                .profile
                .email
                .as_ref()
                .filter(|v| !v.is_empty())
                .ok_or(BridgeHold::Account)?;
            if self.profile.account_id.as_ref() != Some(&format!("claude:{email}"))
                || self.profile.status != "error"
            {
                return Err(BridgeHold::Account);
            }
            email.clone()
        } else {
            if metadata.status != AuthStatus::Ready {
                return Err(BridgeHold::AuthWait);
            }
            if self.profile.account_id.is_some() && self.profile.account_id != metadata.account_id {
                return Err(BridgeHold::Account);
            }
            metadata
                .email
                .clone()
                .filter(|v| !v.is_empty())
                .ok_or(BridgeHold::Account)?
        };
        let credential = self
            .raw
            .get("_credentialIdentity")
            .and_then(Value::as_str)
            .ok_or(BridgeHold::Account)?;
        if !crate::account_driver::claude_identity::belongs_to(credential, &identity) {
            return Err(BridgeHold::Account);
        }
        if self.raw.get("_nativeAuthPending") != Some(&json!(true))
            && !metadata
                .credential_identity
                .as_deref()
                .is_some_and(|observed| {
                    crate::account_driver::claude_identity::accepts(credential, observed)
                })
        {
            return Err(BridgeHold::Account);
        }
        if !paths.sdk_present {
            return Err(BridgeHold::Support);
        }
        if !paths.node.is_absolute()
            || !paths.executable.is_absolute()
            || !paths.bridge.is_absolute()
            || !paths.root.is_absolute()
        {
            return Err(BridgeHold::Executable);
        }
        let mut env: BTreeMap<String, String> =
            crate::account_driver::claude::subscription_environment(
                environment
                    .into_iter()
                    .map(|(k, v)| (k.into(), v.into()))
                    .collect(),
            )
            .into_iter()
            .map(|(k, v)| {
                Ok((
                    k.into_string().map_err(|_| BridgeHold::Options)?,
                    v.into_string().map_err(|_| BridgeHold::Options)?,
                ))
            })
            .collect::<Result<_, BridgeHold>>()?;
        if let Some(config) = self
            .profile
            .claude_options
            .get("configDir")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            env.insert("CLAUDE_CONFIG_DIR".into(), config.into());
        }
        env.insert(
            "STUDIO_CLAUDE_OPTIONS".into(),
            serde_json::to_string(&bridge_options(&self.profile.claude_options)?)
                .map_err(|_| BridgeHold::Options)?,
        );
        env.insert("STUDIO_CLAUDE_ACCOUNT".into(), identity);
        env.insert(
            "STUDIO_CLAUDE_CREDENTIAL_IDENTITY".into(),
            metadata
                .credential_identity
                .clone()
                .filter(|observed| {
                    crate::account_driver::claude_identity::accepts(credential, observed)
                })
                .unwrap_or_else(|| credential.to_owned()),
        );
        env.insert(
            "STUDIO_CLAUDE_BIN".into(),
            paths.executable.to_string_lossy().into_owned(),
        );
        env.insert(
            "PATH".into(),
            format!(
                "{}:{}",
                paths.node.parent().ok_or(BridgeHold::Executable)?.display(),
                env.get("PATH").map(String::as_str).unwrap_or("")
            ),
        );
        Ok(ProviderLaunchSpec::new(
            vec![
                paths.node.to_string_lossy().into_owned(),
                paths.bridge.to_string_lossy().into_owned(),
                paths.root.to_string_lossy().into_owned(),
            ],
            env,
            paths.root,
        ))
    }
}
/// Path discovery performs no native invocation. SDK presence is checked by the owner.
pub struct ResolvedBridge {
    pub node: PathBuf,
    pub executable: PathBuf,
    pub bridge: PathBuf,
    pub root: PathBuf,
    pub sdk_present: bool,
}
pub fn bridge_options(value: &Value) -> Result<Value, BridgeHold> {
    let mut options = value.as_object().cloned().ok_or(BridgeHold::Options)?;
    let args = options.remove("launchArgs").unwrap_or(json!(""));
    let args = args.as_str().ok_or(BridgeHold::Options)?;
    if args.len() > 4096 {
        return Err(BridgeHold::Options);
    }
    let tokens = shlex::split(args).ok_or(BridgeHold::Options)?;
    let mut tokens = tokens.iter();
    let mut extra = serde_json::Map::new();
    while let Some(token) = tokens.next() {
        let name = token.strip_prefix("--").ok_or(BridgeHold::Options)?;
        let value = match name {
            "chrome" | "no-chrome" | "debug" | "verbose" | "disable-slash-commands" => Value::Null,
            "add-dir" | "betas" => json!(
                tokens
                    .next()
                    .filter(|v| !v.starts_with('-'))
                    .ok_or(BridgeHold::Options)?
            ),
            _ => return Err(BridgeHold::Options),
        };
        if extra.insert(name.into(), value).is_some() {
            return Err(BridgeHold::Options);
        }
    }
    options.entry("customModels").or_insert(json!([]));
    options.insert("extraArgs".into(), Value::Object(extra));
    Ok(Value::Object(options))
}
