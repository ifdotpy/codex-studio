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
/// Private account facts selected for a Claude launch; options retain exact
/// provider-owned extensions and field presence for bridge output.
pub struct ClaudeLaunchProfile {
    pub provider: crate::account_policy::PublicString,
    pub status: crate::account_policy::PublicString,
    pub account_id: crate::account_policy::PublicString,
    pub email: crate::account_policy::PublicString,
    pub plan: crate::account_policy::PublicString,
    pub credential_identity: crate::account_policy::PublicString,
    pub home: crate::account_policy::PublicString,
    pub binary_path: crate::account_policy::PublicString,
    pub native_auth_pending: bool,
    pub raw_options: Option<Value>,
    pub checked_options: Option<ClaudeOptionsRecord>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchFields {
    #[serde(default)]
    provider: crate::account_policy::PublicString,
    status: Option<crate::account_policy::PublicString>,
    #[serde(default)]
    account_id: crate::account_policy::PublicString,
    #[serde(default)]
    email: crate::account_policy::PublicString,
    #[serde(default)]
    plan: crate::account_policy::PublicString,
    #[serde(default)]
    home: crate::account_policy::PublicString,
    #[serde(default, rename = "_credentialIdentity")]
    credential_identity: crate::account_policy::PublicString,
    #[serde(default, rename = "_nativeAuthPending")]
    native_auth_pending: crate::account_policy::PublicBoolean,
    #[serde(default, rename = "claudeOptions", deserialize_with = "present_value")]
    raw_options: Option<Value>,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchBinary {
    #[serde(default)]
    binary_path: crate::account_policy::PublicString,
}
fn present_value<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}
impl ClaudeLaunchProfile {
    /// Ingress for callers which already hold a public/private profile value.
    pub fn from_record(record: &Value) -> Result<Self, BridgeHold> {
        if !record.is_object() {
            return Err(BridgeHold::Account);
        }
        let fields: LaunchFields =
            Deserialize::deserialize(record).map_err(|_| BridgeHold::Account)?;
        let binary = match fields.raw_options.as_ref() {
            Some(value) if value.is_object() => {
                LaunchBinary::deserialize(value)
                    .map_err(|_| BridgeHold::Options)?
                    .binary_path
            }
            None | Some(_) => crate::account_policy::PublicString::Missing,
        };
        Ok(Self {
            provider: fields.provider,
            status: fields.status.unwrap_or_default(),
            account_id: fields.account_id,
            email: fields.email,
            plan: fields.plan,
            credential_identity: fields.credential_identity,
            home: fields.home,
            binary_path: binary,
            native_auth_pending: fields.native_auth_pending.as_bool() == Some(true),
            raw_options: fields.raw_options,
            checked_options: None,
        })
    }
}

pub struct BridgeSnapshot {
    profile: Profile,
    credential_identity: Option<String>,
    native_auth_pending: bool,
    raw_options: Option<Value>,
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
        let (credential_identity, native_auth_pending, raw_options) = match record {
            Value::Object(mut fields) => {
                let credential =
                    fields
                        .remove("_credentialIdentity")
                        .and_then(|value| match value {
                            Value::String(value) => Some(value),
                            Value::Null
                            | Value::Bool(_)
                            | Value::Number(_)
                            | Value::Array(_)
                            | Value::Object(_) => None,
                        });
                (
                    credential,
                    fields.remove("_nativeAuthPending") == Some(Value::Bool(true)),
                    fields.remove("claudeOptions"),
                )
            }
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_) => (None, false, None),
        };
        Ok(Self {
            profile,
            credential_identity,
            native_auth_pending,
            raw_options,
            options,
        })
    }
    /// Validate only after the adapter has resolved executables and created its root.
    pub fn from_launch(profile: &ClaudeLaunchProfile) -> Result<Self, BridgeHold> {
        use crate::account_policy::PublicString;
        let optional = |field: &PublicString| match field {
            PublicString::Missing | PublicString::Other(Value::Null) => Ok(None),
            PublicString::String(value) => Ok(Some(value.clone())),
            PublicString::Other(_) => Err(BridgeHold::Account),
        };
        let provider = profile
            .provider
            .as_str()
            .ok_or(BridgeHold::Account)?
            .to_owned();
        let status = profile
            .status
            .as_str()
            .ok_or(BridgeHold::Account)?
            .to_owned();
        let account_id = optional(&profile.account_id)?;
        let email = optional(&profile.email)?;
        if provider != ProviderKind::Claude.key() {
            return Err(BridgeHold::Account);
        }
        let options = match (&profile.raw_options, &profile.checked_options) {
            (None, _) => ClaudeOptionsRecord::default(),
            (Some(Value::Null), _) => return Err(BridgeHold::Options),
            (Some(_), Some(options)) => options.clone(),
            (Some(raw), None) => Deserialize::deserialize(raw).map_err(|_| BridgeHold::Options)?,
        };
        Ok(Self {
            profile: Profile {
                provider,
                status,
                account_id,
                email,
            },
            credential_identity: profile.credential_identity.as_str().map(str::to_owned),
            native_auth_pending: profile.native_auth_pending,
            raw_options: profile.raw_options.clone(),
            options,
        })
    }
    pub fn launch(
        &self,
        paths: ResolvedBridge,
        metadata: &AuthMetadata,
        environment: BTreeMap<String, String>,
    ) -> Result<ProviderLaunchSpec, BridgeHold> {
        let identity = if self.native_auth_pending {
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
            .credential_identity
            .as_deref()
            .ok_or(BridgeHold::Account)?;
        if !crate::account_driver::claude_identity::belongs_to(credential, &identity) {
            return Err(BridgeHold::Account);
        }
        if !self.native_auth_pending
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
                self.raw_options.as_ref(),
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
    fn launch_projection_preserves_literal_pending_and_credential_pin() {
        use crate::account_driver::{AuthMetadata, AuthStatus};
        for pending in [json!(true), json!(false), json!("true"), json!(null)] {
            let row = json!({"provider":"claude","status":"error","email":"a@example.test","accountId":"claude:a@example.test","_credentialIdentity":"claude:a@example.test","_nativeAuthPending":pending,"claudeOptions":{"vendor":{"n":1.0}}});
            let legacy = BridgeSnapshot::from_record(row.clone()).expect("legacy profile");
            let projected = super::ClaudeLaunchProfile::from_record(&row).expect("profile");
            let projected = BridgeSnapshot::from_launch(&projected).expect("projected profile");
            let paths = || super::ResolvedBridge {
                node: "/node".into(),
                executable: "/claude".into(),
                bridge: "/bridge".into(),
                root: "/root".into(),
                sdk_present: true,
            };
            let metadata = AuthMetadata {
                status: AuthStatus::Error,
                account_id: None,
                email: None,
                plan: None,
                organization_name: None,
                credential_identity: None,
                error: None,
                error_kind: None,
            };
            let old = legacy.launch(paths(), &metadata, Default::default());
            let new = projected.launch(paths(), &metadata, Default::default());
            assert_eq!(old.is_ok(), new.is_ok(), "pending={pending}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(old.environment, new.environment);
            }
        }
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
