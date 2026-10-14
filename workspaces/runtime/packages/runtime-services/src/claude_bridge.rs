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
/// Existing unsupported recovery records stay visible and cannot enter ordinary batching.
pub fn admission_hold(agent: &Value) -> Option<&'static str> {
    for (field, message) in [
        (
            "claudePreInputRetry",
            "Claude's original input is frozen pending exact retry recovery",
        ),
        (
            "claudeAuthWait",
            "Claude account verification is pending; original input remains held",
        ),
    ] {
        if agent.get(field).is_some_and(|v| !v.is_null()) {
            return Some(message);
        }
    }
    let attempt = agent.get("startAttempt")?;
    (agent.get("provider") == Some(&json!("claude"))
        && attempt.get("executionOutcome") == Some(&json!("not_applied"))
        && attempt.get("claudeInputRequest").is_none_or(Value::is_null))
    .then_some("Claude input rejection has no frozen request; inspect before resuming")
}

/// An incompatible bridge is held until its owner proves it idle and retires the
/// exact retained generation. This decision never detaches or stops active work.
pub fn compatibility_hold(
    initialize: &Value,
    source_build: &str,
    started_options: &Value,
    current_options: &Value,
) -> Option<&'static str> {
    let capabilities = initialize.get("capabilities").unwrap_or(&Value::Null);
    (capabilities.get("claudeVersion")!=Some(&json!(23)) || capabilities.get("claudeBuild").and_then(Value::as_str)!=Some(source_build) || started_options!=current_options).then_some("Claude bridge compatibility changed; preserve the retained process until verified idle retirement")
}
/// Same filename/content digest as the bridge. No provider state is opened.
pub fn bridge_build(directory: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut paths = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    paths.retain(|p| {
        p.extension().is_some_and(|v| v == "mjs")
            && p.file_name()
                .and_then(|v| v.to_str())
                .is_some_and(|v| !v.ends_with(".test.mjs"))
    });
    paths.sort();
    if paths.len() > 256 {
        return Err(std::io::Error::other("bridge source count exceeds bound"));
    }
    let mut body = Vec::new();
    for path in paths {
        body.extend_from_slice(
            path.file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| std::io::Error::other("invalid bridge source filename"))?
                .as_bytes(),
        );
        body.push(b'\n');
        let remaining = (16 * 1024 * 1024_usize).saturating_sub(body.len());
        std::fs::File::open(path)?
            .take(remaining as u64 + 1)
            .read_to_end(&mut body)?;
        if body.len() > 16 * 1024 * 1024 {
            return Err(std::io::Error::other("bridge source exceeds bound"));
        }
        body.push(b'\n');
    }
    Ok(studio_runtime_store::sha256_hex(&body))
}
