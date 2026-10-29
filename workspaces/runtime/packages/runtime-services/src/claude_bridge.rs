//! Claude admission and launch decisions for the existing provider-session transport.
//! Native credentials stay with the CLI; all account evidence is injected.
use crate::account_driver::{AuthMetadata, AuthStatus};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};
use studio_runtime_process::ProviderLaunchSpec;
/// Launch variable that binds a bridge to its credential identity.
pub const CREDENTIAL_IDENTITY_ENVIRONMENT: &str = "STUDIO_CLAUDE_CREDENTIAL_IDENTITY";
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::claude_options::ClaudeOptionsRecord;
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
}
pub struct BridgeSnapshot {
    profile: Profile,
    raw: Value,
    options: ClaudeOptionsRecord,
}
impl BridgeSnapshot {
    pub fn from_record(record: Value) -> Result<Self, BridgeHold> {
        let profile: Profile =
            Deserialize::deserialize(&record).map_err(|_| BridgeHold::Account)?;
        if profile.provider != ProviderKind::Claude.key() {
            return Err(BridgeHold::Account);
        }
        let empty = json!({});
        let options =
            serde::Deserialize::deserialize(record.get("claudeOptions").unwrap_or(&empty))
                .map_err(|_| BridgeHold::Options)?;
        Ok(Self {
            profile,
            raw: record,
            options,
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
        if let Some(config) = self.options.config_dir.as_deref().filter(|v| !v.is_empty()) {
            env.insert("CLAUDE_CONFIG_DIR".into(), config.into());
        }
        env.insert(
            "STUDIO_CLAUDE_OPTIONS".into(),
            serde_json::to_string(&bridge_options_checked(
                self.raw.get("claudeOptions"),
                &self.options,
            )?)
            .map_err(|_| BridgeHold::Options)?,
        );
        env.insert("STUDIO_CLAUDE_ACCOUNT".into(), identity);
        let credential_identity = metadata
            .credential_identity
            .clone()
            .filter(|observed| {
                crate::account_driver::claude_identity::accepts(credential, observed)
            })
            .unwrap_or_else(|| credential.to_owned());
        // The pin learns organization and account UUIDs after a bridge starts.
        // A bridge launched with an earlier form holds the same credential.
        let earlier_identities =
            crate::account_driver::claude_identity::earlier_forms(&credential_identity);
        env.insert(CREDENTIAL_IDENTITY_ENVIRONMENT.into(), credential_identity);
        env.insert(
            crate::account_driver::CLAUDE_BIN_OVERRIDE.into(),
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
        let mut spec = ProviderLaunchSpec::new(
            vec![
                paths.node.to_string_lossy().into_owned(),
                paths.bridge.to_string_lossy().into_owned(),
                paths.root.to_string_lossy().into_owned(),
            ],
            env,
            paths.root,
        );
        if !earlier_identities.is_empty() {
            spec.retained
                .earlier
                .insert(CREDENTIAL_IDENTITY_ENVIRONMENT.into(), earlier_identities);
        }
        // Node and the Claude CLI run by canonical paths, which a package
        // manager upgrade or a CLI self-update replaces while a bridge runs.
        spec.retained.executable_program = true;
        spec.retained
            .executable_variables
            .insert(crate::account_driver::CLAUDE_BIN_OVERRIDE.into());
        Ok(spec)
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
    finish_bridge_options(options, args)
}

pub fn bridge_options_public(
    options: Option<&crate::account_policy::PublicClaudeOptions>,
) -> Result<Value, BridgeHold> {
    use crate::account_policy::PublicString;
    let mut output = match options {
        Some(options) => options.wire_object().ok_or(BridgeHold::Options)?,
        None => serde_json::Map::new(),
    };
    output.remove("launchArgs");
    let args = match options.map(|options| &options.launch_args) {
        None | Some(PublicString::Missing) => "",
        Some(PublicString::String(value)) => value.as_str(),
        Some(PublicString::Other(_)) => return Err(BridgeHold::Options),
    };
    finish_bridge_options(output, args)
}

fn bridge_options_checked(
    value: Option<&Value>,
    typed: &ClaudeOptionsRecord,
) -> Result<Value, BridgeHold> {
    let mut options = match value {
        Some(value) => value.as_object().cloned().ok_or(BridgeHold::Options)?,
        None => serde_json::Map::new(),
    };
    let present = options.remove("launchArgs");
    let args = match (present, typed.launch_args.as_deref()) {
        (None, _) => "",
        (Some(Value::String(_)), Some(args)) => args,
        _ => return Err(BridgeHold::Options),
    };
    finish_bridge_options(options, args)
}

fn finish_bridge_options(
    mut options: serde_json::Map<String, Value>,
    args: &str,
) -> Result<Value, BridgeHold> {
    let extra = bridge_launch_args(args)?;
    options.entry("customModels").or_insert(json!([]));
    options.insert("extraArgs".into(), Value::Object(extra));
    Ok(Value::Object(options))
}

pub(crate) fn bridge_launch_args(args: &str) -> Result<serde_json::Map<String, Value>, BridgeHold> {
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
    Ok(extra)
}

#[cfg(test)]
mod options_tests {
    use super::{BridgeHold, BridgeSnapshot, bridge_options, bridge_options_checked};
    use serde_json::json;

    #[test]
    fn snapshot_checks_options_shape_and_bridge_preserves_extensions() {
        let profile =
            |options| json!({"provider":"claude","status":"ready","claudeOptions":options});
        assert!(BridgeSnapshot::from_record(profile(json!({"customModels":[{"id":"m","label":"Model","extra":true}],"extension":{"key":1}}))).is_ok());
        assert!(matches!(
            BridgeSnapshot::from_record(profile(json!({"customModels":[["m","Model"]]}))),
            Err(BridgeHold::Options)
        ));
        assert!(matches!(
            BridgeSnapshot::from_record(profile(json!(null))),
            Err(BridgeHold::Options)
        ));
        assert_eq!(
            bridge_options(&json!({"launchArgs":"--verbose","extension":{"key":1}})).unwrap(),
            json!({"customModels":[],"extension":{"key":1},"extraArgs":{"verbose":null}})
        );
        assert!(bridge_options(&json!({"launchArgs":null})).is_err());
    }

    #[test]
    fn public_options_match_legacy_bridge_projection() {
        for raw in [
            json!({}),
            json!({"launchArgs":"--verbose","vendor":{"n":1}}),
            json!({"launchArgs":null}),
            json!({"launchArgs":7}),
            json!(null),
            json!([]),
        ] {
            let profile: crate::account_policy::PublicAccount =
                <crate::account_policy::PublicAccount as serde::Deserialize>::deserialize(
                    json!({"claudeOptions": raw}),
                )
                .expect("public fixture");
            let old = super::bridge_options(&raw);
            let actual = super::bridge_options_public(profile.claude_options.as_ref());
            assert_eq!(actual.is_ok(), old.is_ok(), "{raw}");
            if let (Ok(actual), Ok(old)) = (actual, old) {
                assert_eq!(actual, old);
            }
        }
        assert_eq!(
            super::bridge_options_public(None).expect("default"),
            super::bridge_options(&json!({})).expect("default")
        );
    }

    #[test]
    fn checked_options_keep_missing_null_and_extensions_at_launch() {
        use serde::Deserialize;
        for raw in [
            json!({}),
            json!({"launchArgs":"--verbose","future":[1]}),
            json!({"launchArgs":null}),
            json!({"customModels":null}),
        ] {
            let typed =
                studio_runtime_schema::claude_options::ClaudeOptionsRecord::deserialize(&raw)
                    .unwrap();
            let old = bridge_options(&raw);
            let checked = bridge_options_checked(Some(&raw), &typed);
            assert_eq!(checked.is_ok(), old.is_ok(), "{raw}");
            if let (Ok(checked), Ok(old)) = (checked, old) {
                assert_eq!(checked, old);
            }
        }
    }
}
