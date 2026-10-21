//! Delivery timing owner. Legacy spellings are accepted only at input and
//! persisted-event boundaries; new writes and responses use canonical values.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// When an accepted user or agent message may reach the recipient.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMoment {
    /// Join the running turn at its next safe input boundary, or start a turn.
    #[default]
    CurrentTurn,
    /// Wait for the running turn to finish before starting another turn.
    NextTurn,
}

/// Historical spellings retained for existing callers and persisted events.
pub const LEGACY_DELIVERY_MOMENTS: [(&str, DeliveryMoment); 4] = [
    ("queue", DeliveryMoment::CurrentTurn),
    ("steer", DeliveryMoment::CurrentTurn),
    ("after_tool", DeliveryMoment::CurrentTurn),
    ("after_turn", DeliveryMoment::NextTurn),
];

/// Unknown delivery values must never silently select a timing policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidDeliveryMoment;

impl fmt::Display for InvalidDeliveryMoment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("delivery must be current_turn or next_turn")
    }
}

impl std::error::Error for InvalidDeliveryMoment {}

/// Serde compatibility at request boundaries. Internal `DeliveryMoment`
/// deserialization remains canonical-only.
pub mod compatibility {
    use super::DeliveryMoment;
    use serde::{Deserialize, Deserializer, de::Error};

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DeliveryMoment, D::Error> {
        let value = String::deserialize(deserializer)?;
        DeliveryMoment::parse_legacy(&value).map_err(D::Error::custom)
    }

    /// An omitted request field defaults; a present field must be a string.
    pub mod option {
        use super::DeliveryMoment;
        use serde::{Deserializer, Serialize, Serializer};

        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<DeliveryMoment>, D::Error> {
            super::deserialize(deserializer).map(Some)
        }

        pub fn serialize<S: Serializer>(
            value: &Option<DeliveryMoment>,
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            value.serialize(serializer)
        }
    }
}

#[derive(Deserialize)]
struct CompatibleDelivery(#[serde(deserialize_with = "compatibility::deserialize")] DeliveryMoment);

#[derive(Deserialize)]
struct DeliveryMetadata {
    delivery: Option<CompatibleDelivery>,
}

impl DeliveryMoment {
    pub const ALL: [Self; 2] = [Self::CurrentTurn, Self::NextTurn];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CurrentTurn => "current_turn",
            Self::NextTurn => "next_turn",
        }
    }

    /// Parse the canonical wire contract.
    pub fn parse(value: &str) -> Result<Self, InvalidDeliveryMoment> {
        Self::ALL
            .into_iter()
            .find(|moment| moment.as_str() == value)
            .ok_or(InvalidDeliveryMoment)
    }

    /// Normalize historical input without rewriting stored events. Retain this
    /// until old clients and persisted event histories no longer need reading.
    pub fn parse_legacy(value: &str) -> Result<Self, InvalidDeliveryMoment> {
        Self::parse(value).or_else(|_| {
            LEGACY_DELIVERY_MOMENTS
                .into_iter()
                .find_map(|(legacy, moment)| (legacy == value).then_some(moment))
                .ok_or(InvalidDeliveryMoment)
        })
    }

    /// Resolve an event's explicit timing or its semantic default. Missing and
    /// null timing preserve historical defaults; malformed values fail closed.
    pub fn for_event(
        kind: &str,
        text: &str,
        metadata: Option<&serde_json::Value>,
    ) -> Result<Self, InvalidDeliveryMoment> {
        if let Some(delivery) = Self::metadata_delivery(metadata)? {
            return Ok(delivery);
        }
        Ok(if kind == "agent_message" {
            serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .as_ref()
                .map(Self::message_default)
                .unwrap_or_default()
        } else {
            Self::default()
        })
    }

    /// Resolve parsed message arguments without serializing their importance.
    /// Explicit timing uses the same compatibility codec as persisted events.
    pub fn for_message(args: &serde_json::Value) -> Result<Self, InvalidDeliveryMoment> {
        Ok(Self::metadata_delivery(Some(args))?.unwrap_or_else(|| Self::message_default(args)))
    }

    fn message_default(message: &serde_json::Value) -> Self {
        if message
            .get("importance")
            .and_then(serde_json::Value::as_str)
            == Some("progress")
        {
            Self::NextTurn
        } else {
            Self::CurrentTurn
        }
    }

    fn metadata_delivery(
        metadata: Option<&serde_json::Value>,
    ) -> Result<Option<Self>, InvalidDeliveryMoment> {
        // Derived serde structs also accept sequences; persisted metadata is a
        // JSON object (or the historical null), never a positional record.
        if metadata.is_some_and(|value| !value.is_null() && !value.is_object()) {
            return Err(InvalidDeliveryMoment);
        }
        Ok(metadata
            .map(Option::<DeliveryMetadata>::deserialize)
            .transpose()
            .map_err(|_| InvalidDeliveryMoment)?
            .flatten()
            .and_then(|metadata| metadata.delivery)
            .map(|CompatibleDelivery(delivery)| delivery))
    }
}

impl FromStr for DeliveryMoment {
    type Err = InvalidDeliveryMoment;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl fmt::Display for DeliveryMoment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_serialization_and_parsing_agree() {
        assert_eq!(DeliveryMoment::default(), DeliveryMoment::CurrentTurn);
        for moment in DeliveryMoment::ALL {
            let value = serde_json::json!(moment.as_str());
            assert_eq!(serde_json::to_value(moment).expect("serialize"), value);
            assert_eq!(
                serde_json::from_value::<DeliveryMoment>(value).expect("deserialize"),
                moment
            );
            assert_eq!(moment.as_str().parse(), Ok(moment));
        }
    }

    #[test]
    fn legacy_values_normalize_only_at_the_compatibility_boundary() {
        for (legacy, moment) in LEGACY_DELIVERY_MOMENTS {
            assert_eq!(DeliveryMoment::parse_legacy(legacy), Ok(moment));
            assert!(DeliveryMoment::parse(legacy).is_err());
            assert!(serde_json::from_value::<DeliveryMoment>(serde_json::json!(legacy)).is_err());
        }
        for invalid in ["", "unknown", "CURRENT_TURN", " next_turn"] {
            assert!(DeliveryMoment::parse_legacy(invalid).is_err());
        }
    }

    #[test]
    fn ingress_serde_codec_defaults_only_missing_timing_and_rejects_wrong_types() {
        #[derive(Deserialize)]
        struct Request {
            #[serde(default, with = "compatibility::option")]
            delivery: Option<DeliveryMoment>,
        }
        let missing: Request =
            serde_json::from_value(serde_json::json!({})).expect("omitted delivery");
        assert_eq!(
            missing.delivery.unwrap_or_default(),
            DeliveryMoment::CurrentTurn
        );
        for (value, expected) in DeliveryMoment::ALL
            .map(|moment| (moment.as_str(), moment))
            .into_iter()
            .chain(LEGACY_DELIVERY_MOMENTS)
        {
            let parsed: Request = serde_json::from_value(serde_json::json!({"delivery":value}))
                .expect("supported timing");
            assert_eq!(parsed.delivery, Some(expected));
        }
        for value in [
            serde_json::json!("unknown"),
            serde_json::json!(null),
            serde_json::json!(7),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            assert!(
                serde_json::from_value::<Request>(serde_json::json!({"delivery":value})).is_err()
            );
        }
    }

    #[test]
    fn managed_message_schema_accepts_supported_input_and_rejects_unknown_values() {
        let openapi = serde_json::from_slice(include_bytes!("../../runtime-http/openapi.json"))
            .expect("OpenAPI");
        let registry = crate::registry_for_document(&openapi).expect("request registry");
        let schema = serde_json::json!({"$ref":format!("{}#/components/schemas/ManagedMessageBody", crate::OPENAPI_URI)});
        let validator = jsonschema::options()
            .with_registry(&registry)
            .build(&schema)
            .expect("request validator");
        for value in DeliveryMoment::ALL
            .map(DeliveryMoment::as_str)
            .into_iter()
            .chain(LEGACY_DELIVERY_MOMENTS.map(|(name, _)| name))
        {
            assert!(validator.is_valid(&serde_json::json!({
                "id":"00000000-0000-4000-8000-000000000001", "room":"agent", "text":"hello", "delivery":value
            })), "{value}");
        }
        let mut body = serde_json::json!({"id":"00000000-0000-4000-8000-000000000001", "room":"agent", "text":"hello"});
        assert!(validator.is_valid(&body), "default remains optional");
        for count in [
            crate::shared_values::MAX_TURN_ATTACHMENTS,
            crate::shared_values::MAX_TURN_ATTACHMENTS + 1,
        ] {
            body["assets"] = serde_json::json!(vec!["asset-id"; count]);
            assert_eq!(
                validator.is_valid(&body),
                count <= crate::shared_values::MAX_TURN_ATTACHMENTS
            );
        }
        body.as_object_mut().expect("body object").remove("assets");
        for value in [
            serde_json::json!("unknown"),
            serde_json::json!(null),
            serde_json::json!(7),
        ] {
            body["delivery"] = value;
            assert!(!validator.is_valid(&body));
        }
    }

    #[test]
    fn event_defaults_follow_message_intent_and_explicit_timing_wins() {
        use serde_json::json;
        for (kind, text, expected) in [
            (
                "agent_message",
                r#"{"importance":"progress"}"#,
                DeliveryMoment::NextTurn,
            ),
            (
                "agent_message",
                r#"{"importance":"question"}"#,
                DeliveryMoment::CurrentTurn,
            ),
            (
                "agent_message",
                "plain message",
                DeliveryMoment::CurrentTurn,
            ),
            ("user", "hello", DeliveryMoment::CurrentTurn),
            ("notification", "finished", DeliveryMoment::CurrentTurn),
            ("child_result", "done", DeliveryMoment::CurrentTurn),
            ("followup", "continue", DeliveryMoment::CurrentTurn),
        ] {
            for metadata in [
                None,
                Some(json!(null)),
                Some(json!({})),
                Some(json!({"delivery":null})),
            ] {
                assert_eq!(
                    DeliveryMoment::for_event(kind, text, metadata.as_ref()),
                    Ok(expected)
                );
            }
            assert_eq!(
                DeliveryMoment::for_event(kind, text, Some(&json!({"delivery":"after_tool"}))),
                Ok(DeliveryMoment::CurrentTurn)
            );
            assert_eq!(
                DeliveryMoment::for_event(kind, text, Some(&json!({"delivery":"next_turn"}))),
                Ok(DeliveryMoment::NextTurn)
            );
            for metadata in [
                json!({"delivery":"unknown"}),
                json!({"delivery":5}),
                json!({"delivery":[]}),
                json!("invalid"),
            ] {
                assert!(DeliveryMoment::for_event(kind, text, Some(&metadata)).is_err());
            }
        }
    }

    #[test]
    fn parsed_messages_resolve_timing_without_a_serialization_round_trip() {
        use serde_json::json;
        for importance in ["progress", "question", "message"] {
            let mut args = json!({"importance":importance,"text":"hello","extra":{"rich":true}});
            assert_eq!(
                DeliveryMoment::for_message(&args),
                Ok(if importance == "progress" {
                    DeliveryMoment::NextTurn
                } else {
                    DeliveryMoment::CurrentTurn
                })
            );
            for (value, expected) in DeliveryMoment::ALL
                .map(|moment| (moment.as_str(), moment))
                .into_iter()
                .chain(LEGACY_DELIVERY_MOMENTS)
            {
                args["delivery"] = json!(value);
                assert_eq!(DeliveryMoment::for_message(&args), Ok(expected));
            }
            for invalid in [json!("unknown"), json!(5), json!({}), json!([])] {
                args["delivery"] = invalid;
                assert!(DeliveryMoment::for_message(&args).is_err());
            }
        }
    }

    #[test]
    fn python_event_delivery_matches_the_rust_owner() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        use serde_json::{Value, json};

        let mut cases = Vec::new();
        let mut metadata = vec![
            None,
            Some(Value::Null),
            Some(json!({})),
            Some(json!({"delivery":null})),
        ];
        metadata.extend(DeliveryMoment::ALL.map(|moment| Some(json!({"delivery":moment}))));
        metadata
            .extend(LEGACY_DELIVERY_MOMENTS.map(|(legacy, _)| Some(json!({"delivery":legacy}))));
        metadata.extend([
            Some(json!({"delivery":"unknown"})),
            Some(json!({"delivery":5})),
            Some(json!({"delivery":false})),
            Some(json!({"delivery":[]})),
            Some(json!({"delivery":{}})),
            Some(json!("invalid metadata")),
            Some(json!([])),
            Some(json!([null])),
            Some(json!({"delivery":null,"assets":[],"timing":{"queuedAt":1},"unrelated":{"rich":true}})),
            Some(json!({"delivery":"steer","assets":[],"timing":{"queuedAt":1}})),
        ]);
        for (kind, text) in [
            ("agent_message", r#"{"importance":"progress"}"#),
            ("agent_message", r#"{"importance":"question"}"#),
            ("agent_message", "plain message"),
            ("agent_message", "[]"),
            ("agent_message", r#"["progress"]"#),
            ("user", r#"{"importance":"progress"}"#),
            ("followup", "continue"),
            ("notification", "finished"),
            ("child_result", "done"),
        ] {
            for value in &metadata {
                let mut case = json!({"kind":kind,"text":text});
                if let Some(value) = value {
                    case["metadata"] = value.clone();
                }
                cases.push(case);
            }
        }
        let script = r#"
import json, sqlite3, sys
sys.path.insert(0, sys.argv[1])
from codex_delivery import event_delivery

results = []
with sqlite3.connect(":memory:") as db:
    db.execute("CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY, record TEXT)")
    for index, case in enumerate(json.load(sys.stdin)):
        key = str(index)
        if "metadata" in case:
            db.execute("INSERT INTO runtime_event_meta VALUES (?,?)", (key, json.dumps(case["metadata"])))
        try:
            results.append(event_delivery(db, {"id":key, "kind":case["kind"], "text":case["text"]}))
        except (ValueError, TypeError):
            results.append(None)
print(json.dumps(results))
"#;
        let source = crate::contracts::repository_root().join("workspaces/runtime/apps/server/src");
        let mut python = Command::new("python3")
            .args(["-B", "-c", script])
            .arg(source)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start stdlib Python parity oracle");
        python
            .stdin
            .take()
            .expect("oracle stdin")
            .write_all(&serde_json::to_vec(&cases).expect("serialize shared cases"))
            .expect("write shared cases");
        let output = python.wait_with_output().expect("Python oracle exits");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: Vec<Value> = serde_json::from_slice(&output.stdout).expect("oracle results");
        assert_eq!(actual.len(), cases.len());
        for (case, actual) in cases.iter().zip(actual) {
            let expected = DeliveryMoment::for_event(
                case["kind"].as_str().expect("case kind"),
                case["text"].as_str().expect("case text"),
                case.get("metadata"),
            )
            .ok();
            assert_eq!(
                actual,
                json!(expected),
                "Rust/Python delivery parity: {case}"
            );
        }
    }
}
