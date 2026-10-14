//! Durable Claude sign-in decisions. Codes and browser URLs are transient only.
use crate::account_driver::{AuthMetadata, AuthStatus};
pub mod chats;
#[cfg(unix)]
mod identity;
pub mod ports;
#[cfg(unix)]
pub mod process;
pub mod receipts;
pub mod service;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

const RESTARTED: &str = "Studio restarted during sign-in. Start a new sign-in request.";
pub const CODE_UNKNOWN: &str = "Cannot confirm delivery of the code. Start a new sign-in request.";
pub const FAILED: &str = "Claude sign-in failed. Start a new sign-in request.";
pub const TIMED_OUT: &str = "Claude sign-in timed out. Start a new sign-in request.";

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("Invalid Claude sign-in record at {0}")]
    InvalidRecord(String),
    #[error("Supply a UUID request_id")]
    InvalidIdentity,
    #[error("Unknown Claude sign-in request")]
    Unknown,
    #[error("This request identity has different sign-in details")]
    Conflict,
    #[error("Claude sign-in is already active for this configuration")]
    Busy,
    #[error("Wait for the Claude sign-in link")]
    WaitForLink,
    #[error("Paste a valid authorization code")]
    InvalidCode,
    #[error("Cannot persist Claude sign-in receipt; outcome unknown")]
    Persistence,
    #[error("Cannot complete Claude sign-in. Start a new sign-in request.")]
    Unavailable,
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
struct Receipt {
    request_id: String,
    account_key: String,
    status: LoginStatus,
    email: Option<String>,
    code_submitted: Option<bool>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    flow: Option<String>,
    request_hash: Option<String>,
    code_submitted: Option<bool>,
}

#[derive(Clone)]
pub struct LoginSnapshot {
    record: Value,
    receipt: Receipt,
    metadata: Metadata,
}

impl LoginSnapshot {
    pub fn from_record(mut record: Value) -> Result<Self, LoginError> {
        // Early Python versions wrote the receipt directly.
        if record.get("receipt").is_none() {
            record = json!({"receipt":record,"metadata":{}});
        }
        if let Some((path, _)) =
            studio_runtime_schema::first_error("PersistedClaudeLoginRecord", &record)
                .map_err(|_| LoginError::InvalidRecord("schema".into()))?
        {
            return Err(LoginError::InvalidRecord(path));
        }
        let receipt: Receipt = serde_json::from_value(record["receipt"].clone())
            .map_err(|_| LoginError::InvalidRecord("receipt".into()))?;
        request_id(&receipt.request_id)?;
        let metadata: Metadata = serde_json::from_value(record["metadata"].clone())
            .map_err(|_| LoginError::InvalidRecord("metadata".into()))?;
        if let Some(receipt) = record.get_mut("receipt").and_then(Value::as_object_mut) {
            receipt.remove("verificationUrl");
        }
        Ok(Self {
            record,
            receipt,
            metadata,
        })
    }
    pub fn record(&self) -> &Value {
        &self.record
    }
    pub fn receipt(&self) -> Value {
        self.record["receipt"].clone()
    }
    pub fn request_id(&self) -> &str {
        &self.receipt.request_id
    }
    pub fn account_key(&self) -> &str {
        &self.receipt.account_key
    }
    pub fn active(&self) -> bool {
        self.receipt.status.active()
    }
    pub fn adding(&self) -> bool {
        self.metadata.flow.as_deref() == Some("add")
    }
    pub fn code_submitted(&self) -> bool {
        self.metadata.code_submitted == Some(true) || self.receipt.code_submitted == Some(true)
    }

    pub fn replay(
        &self,
        account: Option<&str>,
        signature: Option<&str>,
    ) -> Result<Value, LoginError> {
        if self.adding() {
            if account.is_some() || signature != self.metadata.request_hash.as_deref() {
                return Err(LoginError::Conflict);
            }
        } else if signature.is_some() || account != Some(self.account_key()) {
            return Err(LoginError::Conflict);
        }
        Ok(self.receipt())
    }
    pub fn finish(&self, status: LoginStatus, error: Option<&str>) -> Result<Self, LoginError> {
        let mut record = self.record.clone();
        record["receipt"]["status"] =
            serde_json::to_value(status).map_err(|_| LoginError::Unavailable)?;
        if let Some(error) = error {
            record["receipt"]["error"] = json!(error);
        }
        Self::from_record(record)
    }
    /// Must be durable before touching stdin. A lost pipe ACK cannot undo it.
    pub fn code_intent(&self, ready: bool) -> Result<(Self, bool), LoginError> {
        if self.code_submitted() || !self.active() {
            return Ok((self.clone(), false));
        }
        if !ready {
            return Err(LoginError::WaitForLink);
        }
        let mut record = self.record.clone();
        record["receipt"]["codeSubmitted"] = json!(true);
        record["metadata"]["codeSubmitted"] = json!(true);
        Ok((Self::from_record(record)?, true))
    }
    pub fn restore(&self, registered: Option<&Value>) -> Result<Self, LoginError> {
        if self.adding()
            && let Some(account) = registered
        {
            let mut record = self.record.clone();
            for key in ["email", "plan"] {
                record["receipt"][key] = account.get(key).cloned().unwrap_or(Value::Null);
            }
            record["receipt"]["accountKey"] =
                account.get("id").cloned().ok_or(LoginError::Unavailable)?;
            record["receipt"]["status"] = json!("ready");
            record["receipt"]["chatsRefreshed"] = json!(true);
            record["metadata"]["cleanupDone"] = json!(true);
            if let Some(r) = record["receipt"].as_object_mut() {
                r.remove("error");
            }
            return Self::from_record(record);
        }
        if self.active() {
            self.finish(LoginStatus::Error, Some(RESTARTED))
        } else {
            Ok(self.clone())
        }
    }
    pub fn validate_completion(
        &self,
        metadata: &AuthMetadata,
        success: bool,
    ) -> Result<(), LoginError> {
        if !self.active() {
            return Err(LoginError::Conflict);
        }
        if !success
            || metadata.status != AuthStatus::Ready
            || metadata.email.as_deref().is_none_or(str::is_empty)
        {
            return Err(LoginError::Unavailable);
        }
        if self.adding() {
            let hint = self
                .record
                .pointer("/metadata/emailHint")
                .and_then(Value::as_str);
            if hint.zip(metadata.email.as_deref()).is_some_and(|(a, b)| {
                a != b && !(a.is_ascii() && b.is_ascii() && a.eq_ignore_ascii_case(b))
            }) || metadata.plan.as_deref().is_none_or(|v| v.trim().is_empty())
            {
                return Err(LoginError::Conflict);
            }
        } else if self
            .receipt
            .email
            .as_ref()
            .map(|email| format!("claude:{email}"))
            != metadata.account_id
        {
            return Err(LoginError::Conflict);
        }
        Ok(())
    }
    pub fn ready(&self, account: &Value, refreshed: bool) -> Result<Self, LoginError> {
        let mut record = self.record.clone();
        record["receipt"]["status"] = json!("ready");
        record["receipt"]["chatsRefreshed"] = json!(refreshed);
        if self.adding() {
            record["receipt"]["accountKey"] =
                account.get("id").cloned().ok_or(LoginError::Unavailable)?;
            for key in ["email", "plan"] {
                record["receipt"][key] = account.get(key).cloned().unwrap_or(Value::Null);
            }
            record["metadata"]["cleanupDone"] = json!(true);
        }
        Self::from_record(record)
    }
}

pub fn request_id(value: &str) -> Result<(), LoginError> {
    if uuid::Uuid::parse_str(value)
        .ok()
        .is_none_or(|id| id.to_string() != value)
    {
        Err(LoginError::InvalidIdentity)
    } else {
        Ok(())
    }
}
pub fn validate_code(code: &str) -> Result<(), LoginError> {
    static PRINTABLE: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    let printable = PRINTABLE
        .get_or_init(|| regex::Regex::new(r"^[\p{L}\p{M}\p{N}\p{P}\p{S} ]+$"))
        .as_ref()
        .map_err(|_| LoginError::Unavailable)?;
    if code.chars().count() <= 4096 && printable.is_match(code) {
        Ok(())
    } else {
        Err(LoginError::InvalidCode)
    }
}
pub fn verification_url(value: &str) -> bool {
    static URL: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    let cleaned: String = value
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    URL.get_or_init(|| {
        regex::Regex::new(
            r"^(?i:https)://claude\.com/cai/oauth/authorize(?:;[^?#]*)?(?:\?[^#]*)?#?$",
        )
    })
    .as_ref()
    .is_ok_and(|pattern| pattern.is_match(&cleaned))
}

pub fn start_record(
    id: &str,
    account: &str,
    email: Option<&str>,
    metadata: Map<String, Value>,
) -> Result<LoginSnapshot, LoginError> {
    request_id(id)?;
    let adding = metadata.get("flow") == Some(&json!("add"));
    let mut receipt =
        json!({"requestId":id,"accountKey":account,"status":"starting","email":email});
    if adding {
        receipt["plan"] = Value::Null;
        receipt["codeSubmitted"] = json!(false);
    }
    LoginSnapshot::from_record(json!({"receipt":receipt,"metadata":metadata}))
}
