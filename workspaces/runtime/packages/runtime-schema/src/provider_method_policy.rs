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
