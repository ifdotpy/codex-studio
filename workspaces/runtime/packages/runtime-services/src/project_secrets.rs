//! Project secrets RPC adaptation. Secret values cross the guest boundary once and are never
//! included in a response, error message, or diagnostic representation.

use serde_json::{Value, json};
use std::time::Duration;

const GUEST_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The narrow guest RPC surface used here; the Linux VM slice supplies the production adapter.
pub trait GuestRpc: Send + Sync {
    fn call(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, GuestCallError>;
}

/// Guest errors preserve their protocol classification while avoiding derived Debug output.
pub struct GuestCallError {
    pub code: String,
    pub detail: String,
    pub uncertain: bool,
}

impl std::fmt::Debug for GuestCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GuestCallError")
            .field("code", &self.code)
            .field("uncertain", &self.uncertain)
            .finish_non_exhaustive()
    }
}

pub enum ProjectSecretsError {
    Guest(GuestCallError),
    InvalidReply,
}

impl std::fmt::Debug for ProjectSecretsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Guest(error) => formatter.debug_tuple("Guest").field(error).finish(),
            Self::InvalidReply => formatter.write_str("InvalidReply"),
        }
    }
}

impl ProjectSecretsError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Guest(error) => Some(&error.code),
            Self::InvalidReply => None,
        }
    }

    pub fn detail(&self) -> &'static str {
        match self {
            Self::Guest(_) => "The guest request failed",
            Self::InvalidReply => "The VM returned an invalid list of secret names.",
        }
    }

    pub fn uncertain(&self) -> bool {
        match self {
            Self::Guest(error) => error.uncertain,
            Self::InvalidReply => true,
        }
    }
}

pub fn list_names(guest: &dyn GuestRpc, project_id: &str) -> Result<Value, ProjectSecretsError> {
    let result = guest
        .call(
            "secrets.list",
            &json!({"projectId": project_id}),
            None,
            GUEST_CALL_TIMEOUT,
        )
        .map_err(ProjectSecretsError::Guest)?;
    names_reply(project_id, result)
}

pub fn set_secret(
    guest: &dyn GuestRpc,
    project_id: &str,
    name: &str,
    value: &str,
    request_id: &str,
) -> Result<Value, ProjectSecretsError> {
    let operation_id = format!("secrets-set:{request_id}");
    let result = guest.call(
        "secrets.set",
        &json!({"projectId": project_id, "name": name, "value": value}),
        Some(&operation_id),
        GUEST_CALL_TIMEOUT,
    );
    let result = result.map_err(|mut error| {
        if !value.is_empty() {
            error.detail = error.detail.replace(value, "[redacted]");
        }
        ProjectSecretsError::Guest(error)
    })?;
    names_reply(project_id, result)
}

pub fn remove_secret(
    guest: &dyn GuestRpc,
    project_id: &str,
    name: &str,
    request_id: &str,
) -> Result<Value, ProjectSecretsError> {
    let operation_id = format!("secrets-remove:{request_id}");
    let result = guest
        .call(
            "secrets.remove",
            &json!({"projectId": project_id, "name": name}),
            Some(&operation_id),
            GUEST_CALL_TIMEOUT,
        )
        .map_err(ProjectSecretsError::Guest)?;
    names_reply(project_id, result)
}

fn names_reply(project_id: &str, result: Value) -> Result<Value, ProjectSecretsError> {
    let names = result
        .get("names")
        .and_then(Value::as_array)
        .filter(|names| names.iter().all(Value::is_string))
        .ok_or(ProjectSecretsError::InvalidReply)?;
    Ok(json!({"projectId": project_id, "names": names}))
}

#[cfg(test)]
mod tests {
    use super::{
        GuestCallError, GuestRpc, ProjectSecretsError, list_names, remove_secret, set_secret,
    };
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Default)]
    struct FakeGuest {
        calls: Mutex<Vec<(String, Value, Option<String>, Duration)>>,
        reply: Mutex<Value>,
        error: Mutex<Option<GuestCallError>>,
    }

    impl GuestRpc for FakeGuest {
        fn call(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            timeout: Duration,
        ) -> Result<Value, GuestCallError> {
            self.calls.lock().unwrap().push((
                method.to_owned(),
                params.clone(),
                request_id.map(str::to_owned),
                timeout,
            ));
            if let Some(error) = self.error.lock().unwrap().take() {
                return Err(error);
            }
            Ok(self.reply.lock().unwrap().clone())
        }
    }

    #[test]
    fn list_returns_only_the_guest_names() {
        let guest = FakeGuest::default();
        *guest.reply.lock().unwrap() = json!({"projectId":"different","names":["TOKEN"]});

        let response = list_names(&guest, "project-1").unwrap();

        assert_eq!(response, json!({"projectId":"project-1","names":["TOKEN"]}));
        assert_eq!(
            guest.calls.lock().unwrap().as_slice(),
            [(
                "secrets.list".to_owned(),
                json!({"projectId":"project-1"}),
                None,
                Duration::from_secs(30)
            )]
        );
    }

    #[test]
    fn set_binds_the_guest_receipt_and_never_returns_the_value() {
        let guest = FakeGuest::default();
        *guest.reply.lock().unwrap() = json!({"names":["TOKEN"]});

        let response = set_secret(&guest, "p", "TOKEN", "generated-test-value", "req-9").unwrap();

        assert_eq!(response, json!({"projectId":"p","names":["TOKEN"]}));
        let calls = guest.calls.lock().unwrap();
        assert_eq!(calls[0].0, "secrets.set");
        assert_eq!(
            calls[0].1,
            json!({"projectId":"p","name":"TOKEN","value":"generated-test-value"})
        );
        assert_eq!(calls[0].2.as_deref(), Some("secrets-set:req-9"));
        assert_eq!(calls[0].3, Duration::from_secs(30));
        assert!(!response.to_string().contains("generated-test-value"));
    }

    #[test]
    fn remove_binds_a_distinct_receipt_namespace() {
        let guest = FakeGuest::default();
        *guest.reply.lock().unwrap() = json!({"names":[]});

        remove_secret(&guest, "p", "TOKEN", "req-9").unwrap();

        assert_eq!(
            guest.calls.lock().unwrap()[0].2.as_deref(),
            Some("secrets-remove:req-9")
        );
    }

    #[test]
    fn malformed_guest_names_are_a_located_uncertain_refusal() {
        let guest = FakeGuest::default();
        *guest.reply.lock().unwrap() = json!({"names":["TOKEN", 7]});

        let error = list_names(&guest, "p").unwrap_err();

        assert!(matches!(error, ProjectSecretsError::InvalidReply));
        assert!(error.uncertain());
    }

    #[test]
    fn guest_errors_cannot_echo_the_submitted_value() {
        let guest = FakeGuest::default();
        *guest.error.lock().unwrap() = Some(GuestCallError {
            code: "operation_failed".to_owned(),
            detail: "rejected generated-test-value".to_owned(),
            uncertain: true,
        });

        let error = set_secret(&guest, "p", "TOKEN", "generated-test-value", "req").unwrap_err();

        assert_eq!(error.detail(), "The guest request failed");
        if let ProjectSecretsError::Guest(error) = error {
            assert!(!error.detail.contains("generated-test-value"));
        }
    }
}
