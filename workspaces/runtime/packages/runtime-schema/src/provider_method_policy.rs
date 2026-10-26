//! Per-thread ordering policy shared by the Rust transport and Claude bridge.

use serde_json::Value;

/// A method reserves its thread lane unconditionally, or when a parameter is true.
pub struct ThreadSerializationRule {
    pub method: &'static str,
    pub true_parameter: Option<&'static str>,
}

/// Methods whose work must finish before the next operation on the same thread.
pub const THREAD_SERIALIZATION_RULES: &[ThreadSerializationRule] = &[
    ThreadSerializationRule {
        method: "turn/start",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "turn/steer",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "thread/resume",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "thread/fork",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "thread/read",
        true_parameter: Some("includeTurns"),
    },
    ThreadSerializationRule {
        method: "thread/rollback",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "claude/settings",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "claude/state",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "claude/fileBackups",
        true_parameter: None,
    },
    ThreadSerializationRule {
        method: "thread/compact/start",
        true_parameter: None,
    },
];

#[must_use]
pub fn thread_serialization_rule(method: &str) -> Option<&'static ThreadSerializationRule> {
    THREAD_SERIALIZATION_RULES
        .iter()
        .find(|rule| rule.method == method)
}

#[must_use]
pub fn is_thread_serialized(method: &str, params: &Value) -> bool {
    thread_serialization_rule(method).is_some_and(|rule| {
        rule.true_parameter
            .is_none_or(|parameter| params.get(parameter) == Some(&Value::Bool(true)))
    })
}

/// Recovery cannot prove a parameter-dependent request lightweight when its
/// persisted parameters are unreadable. Keep its ownership until settlement.
#[must_use]
pub fn is_recovered_thread_serialized(method: &str, params: &Value) -> bool {
    thread_serialization_rule(method).is_some_and(|rule| {
        rule.true_parameter.is_none_or(|parameter| {
            params.as_object().is_none_or(|params| {
                params
                    .get(parameter)
                    .is_some_and(|value| value.as_bool().unwrap_or(true))
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recovered_policy_keeps_unreadable_parameters_owned() {
        for params in [
            Value::Null,
            json!("unreadable"),
            json!([]),
            json!({"includeTurns":"true"}),
            json!({"includeTurns":null}),
        ] {
            assert!(is_recovered_thread_serialized("thread/read", &params));
        }
        for params in [json!({}), json!({"includeTurns":false})] {
            assert!(!is_recovered_thread_serialized("thread/read", &params));
        }
        assert!(is_recovered_thread_serialized(
            "thread/read",
            &json!({"includeTurns":true})
        ));
        assert!(is_recovered_thread_serialized(
            "claude/settings",
            &Value::Null
        ));
        assert!(!is_recovered_thread_serialized(
            "turn/interrupt",
            &Value::Null
        ));
    }
}
