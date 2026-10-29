//! Persisted Claude login wire records, including forward-compatible extensions.
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Number, Value};

#[derive(Clone, Deserialize)]
pub struct ClaudeLoginRecord {
    pub receipt: ClaudeLoginReceipt,
    pub metadata: ClaudeLoginMetadata,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LoginStatus {
    Starting,
    Pending,
    Ready,
    Error,
    Cancelled,
}
impl LoginStatus {
    pub fn active(self) -> bool {
        matches!(self, Self::Starting | Self::Pending)
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeLoginReceipt {
    #[serde(deserialize_with = "bounded_identifier")]
    request_id: String,
    #[serde(deserialize_with = "bounded_identifier")]
    account_key: String,
    #[serde(deserialize_with = "crate::string_enum::deserialize")]
    pub status: LoginStatus,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub error: Option<String>,
    pub code_submitted: Option<bool>,
    pub chats_refreshed: Option<bool>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

impl ClaudeLoginReceipt {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }
    pub fn account_key(&self) -> &str {
        &self.account_key
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeLoginMetadata {
    pub binary_path: Option<String>,
    pub config_dir: Option<String>,
    pub email_hint: Option<String>,
    pub label: Option<String>,
    #[serde(default, deserialize_with = "optional_add")]
    pub flow: Option<AddFlow>,
    #[serde(default, deserialize_with = "optional_hash")]
    request_hash: Option<String>,
    pub child: Option<ClaudeChildIdentity>,
    pub cleanup_done: Option<bool>,
    pub code_submitted: Option<bool>,
    pub inherited_lease: Option<bool>,
    pub operator_cleared: Option<bool>,
    pub outcome_unknown: Option<bool>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

impl ClaudeLoginMetadata {
    pub fn request_hash(&self) -> Option<&str> {
        self.request_hash.as_deref()
    }
}

#[derive(Clone, Copy)]
pub struct AddFlow;

#[derive(Clone, Deserialize)]
pub struct ClaudeChildIdentity {
    #[serde(deserialize_with = "pid")]
    pid: Number,
    #[serde(deserialize_with = "child_start")]
    start: String,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}
impl ClaudeChildIdentity {
    pub fn pid(&self) -> &Number {
        &self.pid
    }
    pub fn start(&self) -> &str {
        &self.start
    }
}
fn bounded_identifier<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let text = String::deserialize(d)?;
    if (1..=1024).contains(&text.chars().count()) {
        Ok(text)
    } else {
        Err(serde::de::Error::custom("invalid identifier length"))
    }
}
fn child_start<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let text = String::deserialize(d)?;
    if (1..=128).contains(&text.chars().count()) {
        Ok(text)
    } else {
        Err(serde::de::Error::custom("invalid child start length"))
    }
}
fn pid<'de, D: Deserializer<'de>>(d: D) -> Result<Number, D::Error> {
    let number = Number::deserialize(d)?;
    if number
        .as_f64()
        .is_some_and(|value| value.fract() == 0.0 && (1.0..=u32::MAX as f64).contains(&value))
    {
        Ok(number)
    } else {
        Err(serde::de::Error::custom("invalid child pid"))
    }
}
fn optional_add<'de, D: Deserializer<'de>>(d: D) -> Result<Option<AddFlow>, D::Error> {
    match Option::<String>::deserialize(d)? {
        None => Ok(None),
        Some(value) if value == "add" => Ok(Some(AddFlow)),
        _ => Err(serde::de::Error::custom("invalid login flow")),
    }
}
fn optional_hash<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    let value = Option::<String>::deserialize(d)?;
    if value.as_ref().is_none_or(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        Ok(value)
    } else {
        Err(serde::de::Error::custom("invalid request hash"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    #[test]
    fn login_schema_constraints_are_tracked() {
        let schema: Value =
            serde_json::from_str(include_str!("schemas/persisted-claude-login.schema.json"))
                .unwrap();
        let digest = Sha256::digest(serde_json::to_vec(&schema).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(
            digest,
            "e237da30e8bc17398f4c4df63de0bf89c4443f8907b4926dc465579797529eea"
        );
    }

    #[test]
    fn login_parser_matches_schema_shapes() {
        let valid =
            json!({"receipt":{"requestId":"r","accountKey":"a","status":"pending"},"metadata":{}});
        let candidates = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(1),
            json!(1.0),
            json!(1.5),
            json!(""),
            json!("x"),
            json!([]),
            json!({}),
            json!({"pending":null}),
        ];
        let mut values = vec![valid.clone(), json!([]), json!(null), json!({})];
        for field in ["receipt", "metadata", "extra"] {
            for candidate in &candidates {
                let mut v = valid.clone();
                v[field] = candidate.clone();
                values.push(v);
            }
        }
        for field in [
            "requestId",
            "accountKey",
            "status",
            "chatsRefreshed",
            "codeSubmitted",
            "email",
            "error",
            "plan",
            "extra",
        ] {
            for candidate in &candidates {
                let mut v = valid.clone();
                v["receipt"][field] = candidate.clone();
                values.push(v);
            }
        }
        for field in [
            "binaryPath",
            "child",
            "cleanupDone",
            "codeSubmitted",
            "configDir",
            "emailHint",
            "flow",
            "inheritedLease",
            "label",
            "operatorCleared",
            "outcomeUnknown",
            "requestHash",
            "extra",
        ] {
            for candidate in &candidates {
                let mut v = valid.clone();
                v["metadata"][field] = candidate.clone();
                values.push(v);
            }
        }
        for field in ["requestId", "accountKey", "status"] {
            let mut v = valid.clone();
            v["receipt"].as_object_mut().unwrap().remove(field);
            values.push(v);
        }
        for field in ["receipt", "metadata"] {
            let mut v = valid.clone();
            v.as_object_mut().unwrap().remove(field);
            values.push(v);
        }
        for field in ["requestId", "accountKey"] {
            for size in [0, 1, 1024, 1025] {
                let mut v = valid.clone();
                v["receipt"][field] = json!("é".repeat(size));
                values.push(v);
            }
        }
        for hash in [
            "a".repeat(64),
            "F".repeat(64),
            "g".repeat(64),
            "a".repeat(63),
            "a".repeat(65),
        ] {
            let mut v = valid.clone();
            v["metadata"]["requestHash"] = json!(hash);
            values.push(v);
        }
        for pid in [
            json!(0),
            json!(1),
            json!(1.0),
            json!(1.5),
            json!(u32::MAX),
            json!(u64::from(u32::MAX) + 1),
            json!("1"),
            json!(null),
        ] {
            let mut v = valid.clone();
            v["metadata"]["child"] = json!({"pid":pid,"start":"x","extra":true});
            values.push(v);
        }
        for size in [0, 1, 128, 129] {
            let mut v = valid.clone();
            v["metadata"]["child"] = json!({"pid":1,"start":"é".repeat(size)});
            values.push(v);
        }
        for child in [json!({"pid":1}), json!({"start":"x"}), json!([1, "x"])] {
            let mut v = valid.clone();
            v["metadata"]["child"] = child;
            values.push(v);
        }
        for status in ["starting", "pending", "ready", "error", "cancelled"] {
            let mut v = valid.clone();
            v["receipt"]["status"] = json!({(status):null});
            values.push(v);
        }
        let mut v = valid;
        v["metadata"]["requestHash"] = json!(format!("{}\n", "a".repeat(64)));
        values.push(v);
        for v in values {
            assert_eq!(
                serde_json::from_value::<ClaudeLoginRecord>(v.clone()).is_ok(),
                crate::validate("PersistedClaudeLoginRecord", &v).unwrap(),
                "{v}"
            );
        }
    }
}
