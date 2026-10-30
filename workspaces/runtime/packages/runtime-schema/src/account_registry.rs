//! Typed persisted account registry and account records.
use crate::{
    claude_options::ClaudeOptionsRecord, json_object::JsonObject, provider_kind::ProviderKind,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Number;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountRegistry {
    #[serde(rename = "version", deserialize_with = "version")]
    _version: Number,
    pub default_account_key: String,
    pub accounts: BTreeMap<String, JsonObject<AccountRecord>>,
    #[serde(default, deserialize_with = "present")]
    pub logins: Option<BTreeMap<String, JsonObject<LoginReceipt>>>,
    #[serde(default, deserialize_with = "present")]
    pub delete_receipts: Option<BTreeMap<String, JsonObject<DeleteReceipt>>>,
    #[serde(default, deserialize_with = "present")]
    pub name_receipts: Option<BTreeMap<String, JsonObject<NameReceipt>>>,
    #[serde(default, deserialize_with = "present")]
    pub pending_accounts_change: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountRecord {
    pub id: String,
    pub home: String,
    pub label: String,
    pub source: String,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: AccountStatus,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub provider: ProviderKind,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub error: Option<String>,
    pub plan: Option<String>,
    pub deleted: Option<bool>,
    pub disconnected: Option<bool>,
    #[serde(default, deserialize_with = "present")]
    pub duplicate_of: Option<Option<String>>,
    #[serde(rename = "_authErrorKind")]
    pub auth_error_kind: Option<String>,
    #[serde(rename = "_credentialIdentity")]
    pub credential_identity: Option<String>,
    #[serde(rename = "_organizationName")]
    pub organization_name: Option<String>,
    pub claude_options: Option<JsonObject<ClaudeOptionsRecord>>,
    pub start_throttle: Option<JsonObject<AccountStartThrottle>>,
    #[serde(default, deserialize_with = "present")]
    pub organization_mismatch: Option<JsonObject<OrganizationMismatch>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountStatus {
    Ready,
    SignedOut,
    Pending,
    Error,
    Changed,
    Duplicate,
}

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccountStartThrottle {
    #[serde(rename = "burst", deserialize_with = "burst")]
    _burst: Number,
    #[serde(rename = "pauseSeconds", deserialize_with = "pause")]
    _pause_seconds: Number,
    #[serde(rename = "maxRunningTurns", deserialize_with = "running")]
    _max_running_turns: Number,
}
impl AccountStartThrottle {
    pub fn burst_number(&self) -> &Number {
        &self._burst
    }
    pub fn pause_seconds_number(&self) -> &Number {
        &self._pause_seconds
    }
    pub fn max_running_turns_number(&self) -> &Number {
        &self._max_running_turns
    }
}
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OrganizationMismatch {
    pub expected: JsonObject<Organization>,
    pub observed: JsonObject<Organization>,
}
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Organization {
    #[serde(deserialize_with = "required_nullable")]
    pub name: Option<String>,
    pub id_suffix: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoginReceipt {
    pub request_id: String,
    pub account_key: String,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: LoginStatus,
    pub created_at: Option<Number>,
    pub email: Option<String>,
    pub email_hint: Option<String>,
    pub error: Option<String>,
    pub expected_account_id: Option<String>,
    pub expires_at: Option<Number>,
    pub login_id: Option<String>,
    pub native_completed: Option<bool>,
    pub reauth_account_key: Option<String>,
    pub requested_label: Option<String>,
    pub resolved_account_key: Option<String>,
    pub user_code: Option<String>,
    pub verification_url: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LoginStatus {
    Starting,
    Pending,
    Ready,
    Duplicate,
    Cancelled,
    Error,
    Uncertain,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteReceipt {
    pub account_key: String,
    pub deleted_at: Number,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NameReceipt {
    pub account_key: String,
    pub label: String,
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn required_nullable<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(d)
}
fn integer<'de, D: Deserializer<'de>>(d: D, min: u64, max: u64) -> Result<Number, D::Error> {
    let n = Number::deserialize(d)?;
    if n.as_f64()
        .is_some_and(|x| x.fract() == 0.0 && x >= min as f64 && x <= max as f64)
    {
        Ok(n)
    } else {
        Err(serde::de::Error::custom("integer outside permitted bounds"))
    }
}
fn version<'de, D: Deserializer<'de>>(d: D) -> Result<Number, D::Error> {
    integer(
        d,
        u64::from(crate::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION),
        u64::from(crate::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION),
    )
}
fn burst<'de, D: Deserializer<'de>>(d: D) -> Result<Number, D::Error> {
    integer(
        d,
        crate::shared_values::ACCOUNT_START_BURST_MIN,
        crate::shared_values::ACCOUNT_START_BURST_MAX,
    )
}
fn pause<'de, D: Deserializer<'de>>(d: D) -> Result<Number, D::Error> {
    integer(
        d,
        crate::shared_values::ACCOUNT_START_PAUSE_MIN_SECONDS,
        crate::shared_values::ACCOUNT_START_PAUSE_MAX_SECONDS,
    )
}
fn running<'de, D: Deserializer<'de>>(d: D) -> Result<Number, D::Error> {
    integer(
        d,
        crate::shared_values::ACCOUNT_RUNNING_TURNS_MIN,
        crate::shared_values::ACCOUNT_RUNNING_TURNS_MAX,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    #[test]
    fn registry_schema_constraints_are_tracked() {
        let mut schema: Value =
            serde_json::from_str(include_str!("schemas/account-registry.schema.json")).unwrap();
        let defs = schema.as_object_mut().unwrap().remove("$defs").unwrap();
        let mut watched = serde_json::Map::new();
        watched.insert("AccountRegistryRecord".into(), schema);
        for name in [
            "AccountRecord",
            "AccountStartThrottle",
            "ClaudeOrganizationMismatch",
            "ClaudeOrganization",
            "CodexLoginReceiptRecord",
            "DeleteReceiptRecord",
            "NameReceiptRecord",
            "ProviderKind",
        ] {
            watched.insert(name.into(), defs[name].clone());
        }
        assert_eq!(
            Sha256::digest(serde_json::to_vec(&watched).unwrap())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "1331b5904e05cad1c56e009723ba0ccca9905d3d9449dcebf9c21a9497eddb54"
        );
    }

    #[test]
    fn registry_enums_require_string_wire_values() {
        let valid = json!({"version":2,"defaultAccountKey":"a","accounts":{"a":{"id":"a","home":"/","label":"A","source":"local","status":"ready","provider":"claude"}},"logins":{"r":{"requestId":"r","accountKey":"a","status":"ready"}}});
        for (section, field, variants) in [
            ("accounts", "provider", &["codex", "claude"][..]),
            (
                "accounts",
                "status",
                &[
                    "ready",
                    "signedOut",
                    "pending",
                    "error",
                    "changed",
                    "duplicate",
                ][..],
            ),
            (
                "logins",
                "status",
                &[
                    "starting",
                    "pending",
                    "ready",
                    "duplicate",
                    "cancelled",
                    "error",
                    "uncertain",
                ][..],
            ),
        ] {
            for variant in variants {
                let mut value = valid.clone();
                let key = if section == "accounts" { "a" } else { "r" };
                value[section][key][field] = json!({(*variant): null});
                assert!(
                    !crate::validate("account-registry/AccountRegistryRecord", &value).unwrap()
                );
                assert!(
                    serde_json::from_value::<JsonObject<AccountRegistry>>(value.clone()).is_err(),
                    "{value}"
                );
            }
        }
    }

    #[test]
    fn registry_parser_matches_schema_shapes() {
        let account = json!({"id":"a","home":"/home","label":"A","source":"local","status":"ready","provider":"claude"});
        let login = json!({"requestId":"r","accountKey":"a","status":"starting"});
        let valid = json!({"version":2,"defaultAccountKey":"a","accounts":{"a":account},"logins":{"r":login}});
        let candidates = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(1),
            json!(2.0),
            json!(1.5),
            json!(""),
            json!("x"),
            json!([]),
            json!({}),
            json!({"unexpected":true}),
        ];
        let mut values = vec![valid.clone(), json!([]), json!(null), json!({})];
        for field in [
            "version",
            "defaultAccountKey",
            "accounts",
            "logins",
            "deleteReceipts",
            "nameReceipts",
            "pendingAccountsChange",
            "unknown",
        ] {
            for candidate in &candidates {
                let mut v = valid.clone();
                v[field] = candidate.clone();
                values.push(v);
            }
        }
        for field in [
            "id",
            "home",
            "label",
            "source",
            "status",
            "provider",
            "accountId",
            "email",
            "error",
            "plan",
            "deleted",
            "disconnected",
            "duplicateOf",
            "_authErrorKind",
            "_credentialIdentity",
            "_organizationName",
            "claudeOptions",
            "startThrottle",
            "organizationMismatch",
            "unknown",
        ] {
            for candidate in &candidates {
                let mut v = valid.clone();
                v["accounts"]["a"][field] = candidate.clone();
                values.push(v);
            }
        }
        for field in [
            "requestId",
            "accountKey",
            "status",
            "createdAt",
            "email",
            "emailHint",
            "error",
            "expectedAccountId",
            "expiresAt",
            "loginId",
            "nativeCompleted",
            "reauthAccountKey",
            "requestedLabel",
            "resolvedAccountKey",
            "userCode",
            "verificationUrl",
            "unknown",
        ] {
            for candidate in &candidates {
                let mut v = valid.clone();
                v["logins"]["r"][field] = candidate.clone();
                values.push(v);
            }
        }
        for field in ["burst", "pauseSeconds", "maxRunningTurns"] {
            for candidate in [
                json!(null),
                json!(0),
                json!(1),
                json!(1.0),
                json!(1.5),
                json!(512),
                json!(513),
                json!(3600),
                json!(3601),
                json!("1"),
            ] {
                let mut v = valid.clone();
                v["accounts"]["a"]["startThrottle"] =
                    json!({"burst":1,"pauseSeconds":0,"maxRunningTurns":1});
                v["accounts"]["a"]["startThrottle"][field] = candidate;
                values.push(v);
            }
        }
        for field in ["id", "home", "label", "source", "status", "provider"] {
            let mut v = valid.clone();
            v["accounts"]["a"].as_object_mut().unwrap().remove(field);
            values.push(v);
        }
        for field in ["requestId", "accountKey", "status"] {
            let mut v = valid.clone();
            v["logins"]["r"].as_object_mut().unwrap().remove(field);
            values.push(v);
        }
        for field in ["version", "defaultAccountKey", "accounts"] {
            let mut v = valid.clone();
            v.as_object_mut().unwrap().remove(field);
            values.push(v);
        }
        for receipt in [
            json!({"accountKey":"a","deletedAt":1}),
            json!({"accountKey":"a","deletedAt":null}),
            json!({"accountKey":"a"}),
            json!(["a", 1]),
            json!({"accountKey":"a","deletedAt":1,"extra":true}),
        ] {
            let mut v = valid.clone();
            v["deleteReceipts"] = json!({"x":receipt});
            values.push(v);
        }
        for receipt in [
            json!({"accountKey":"a","label":"A"}),
            json!({"accountKey":"a","label":null}),
            json!({"accountKey":"a"}),
            json!(["a", "A"]),
            json!({"accountKey":"a","label":"A","extra":true}),
        ] {
            let mut v = valid.clone();
            v["nameReceipts"] = json!({"x":receipt});
            values.push(v);
        }
        for org in [
            json!({"name":null,"idSuffix":"x"}),
            json!({"name":"N","idSuffix":"x"}),
            json!({"idSuffix":"x"}),
            json!([null, "x"]),
            json!({"name":null,"idSuffix":"x","extra":true}),
        ] {
            let mut v = valid.clone();
            v["accounts"]["a"]["organizationMismatch"] =
                json!({"expected":org,"observed":{"name":null,"idSuffix":"x"}});
            values.push(v);
        }
        for v in values {
            assert_eq!(
                serde_json::from_value::<JsonObject<AccountRegistry>>(v.clone()).is_ok(),
                crate::validate("account-registry/AccountRegistryRecord", &v).unwrap(),
                "{v}"
            );
        }
    }
}
