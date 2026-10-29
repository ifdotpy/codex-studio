//! Private thread channel. The UI receives data, never native pointers.
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Events emitted by the native voice peer to its subscribing view.
#[derive(Serialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NativeVoiceEvent {
    Message { data: String },
    State { state: String },
    Error { message: String },
    Microphone { state: String },
    Channel { state: String },
}

pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_MESSAGE: usize = 32 * 1024 * 1024;

// Each frame has a one-byte continuation flag. A writer must hold its lock
// through the final fragment so events cannot interrupt command responses.
#[derive(Default)]
pub struct Assembler {
    bytes: Vec<u8>,
}
impl Assembler {
    pub fn push(&mut self, frame: &[u8]) -> io::Result<Option<Vec<u8>>> {
        if frame.len() < 2
            || frame.len() > MAX_FRAME
            || frame[0] > 1
            || self.bytes.len() + frame.len() - 1 > MAX_MESSAGE
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid message fragment",
            ));
        }
        self.bytes.extend_from_slice(&frame[1..]);
        Ok(if frame[0] == 0 {
            Some(std::mem::take(&mut self.bytes))
        } else {
            None
        })
    }
    pub fn finish(self) -> io::Result<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated message",
            ))
        }
    }
}

pub fn fragments(bytes: &[u8]) -> io::Result<impl Iterator<Item = (u8, &[u8])>> {
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid message size",
        ));
    }
    let count = bytes.len().div_ceil(MAX_FRAME - 1);
    Ok(bytes
        .chunks(MAX_FRAME - 1)
        .enumerate()
        .map(move |(index, part)| (u8::from(index + 1 < count), part)))
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct Response {
    pub id: String,
    #[serde(
        default,
        deserialize_with = "present_result",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CommandError>,
}

// A successful void command has an explicit JSON null result. Missing and null
// must remain distinct or decoding it would turn success into a broken channel.
fn present_result<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

/// A native event retains its arbitrary JSON payload; replies are always
/// exactly one success or command error.
#[derive(Debug)]
pub enum NativeFrame {
    Event(Value),
    Response(NativeResponse),
}

#[derive(Debug)]
pub struct NativeResponse {
    pub id: String,
    pub outcome: NativeOutcome,
}

#[derive(Debug)]
pub enum NativeOutcome {
    Success(Value),
    Failure(CommandError),
}

impl From<NativeResponse> for Response {
    fn from(response: NativeResponse) -> Self {
        match response.outcome {
            NativeOutcome::Success(result) => Self::success(response.id, result),
            NativeOutcome::Failure(error) => Self {
                id: response.id,
                result: None,
                error: Some(error),
            },
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireFrame {
    Event {
        event: String,
        #[serde(flatten)]
        fields: serde_json::Map<String, Value>,
    },
    Response(WireResponse),
}

#[derive(Deserialize)]
struct WireResponse {
    id: String,
    #[serde(flatten)]
    outcome: WireOutcome,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireOutcome {
    Success {
        result: Value,
        #[serde(default, rename = "error")]
        _error: Option<()>,
    },
    Failure {
        error: CommandError,
        #[serde(default, rename = "result", deserialize_with = "reject_result")]
        _result: (),
    },
}

fn reject_result<'de, D: serde::Deserializer<'de>>(_: D) -> Result<(), D::Error> {
    Err(serde::de::Error::custom(
        "a failure cannot contain a result",
    ))
}

impl<'de> Deserialize<'de> for NativeFrame {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match WireFrame::deserialize(deserializer)? {
            WireFrame::Event { event, mut fields } => {
                fields.insert("event".into(), Value::String(event));
                Self::Event(Value::Object(fields))
            }
            WireFrame::Response(WireResponse { id, outcome }) => Self::Response(NativeResponse {
                id,
                outcome: match outcome {
                    WireOutcome::Success { result, _error: _ } => NativeOutcome::Success(result),
                    WireOutcome::Failure { error, _result: _ } => NativeOutcome::Failure(error),
                },
            }),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommandError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Response {
    pub fn success(id: String, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn failure(id: String, code: &str, message: impl ToString) -> Self {
        Self {
            id,
            result: None,
            error: Some(CommandError {
                code: code.into(),
                message: message.to_string(),
                status: None,
                details: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn native_frames_prioritize_events_and_validate_response_shapes() {
        use serde_json::json;
        let event = json!({"event":"changed", "id":"also-a-response", "result":null, "error":{"bad":true}, "payload":{"nested":[null,1]}});
        let decoded: super::NativeFrame = serde_json::from_value(event.clone()).unwrap();
        match decoded {
            super::NativeFrame::Event(value) => assert_eq!(value, event),
            _ => panic!("event must take priority"),
        }
        let success: super::NativeFrame =
            serde_json::from_str(r#"{"id":"void","result":null}"#).unwrap();
        assert!(matches!(
            success,
            super::NativeFrame::Response(super::NativeResponse {
                outcome: super::NativeOutcome::Success(serde_json::Value::Null),
                ..
            })
        ));
        let failure: super::NativeFrame =
            serde_json::from_str(r#"{"id":"bad","error":{"code":"denied","message":"Denied"}}"#)
                .unwrap();
        assert!(matches!(
            failure,
            super::NativeFrame::Response(super::NativeResponse {
                outcome: super::NativeOutcome::Failure(_),
                ..
            })
        ));
        for value in [
            json!({"id":"missing"}),
            json!({"id":"both","result":null,"error":{"code":"x","message":"x"}}),
            json!({"id":"null","error":null}),
        ] {
            assert!(serde_json::from_value::<super::NativeFrame>(value).is_err());
        }
        let explicit_null_error: super::NativeFrame =
            serde_json::from_str(r#"{"id":"reply","result":null,"error":null}"#).unwrap();
        assert!(matches!(
            explicit_null_error,
            super::NativeFrame::Response(super::NativeResponse {
                outcome: super::NativeOutcome::Success(serde_json::Value::Null),
                ..
            })
        ));
        for response in [
            super::Response::success("ok".into(), json!(null)),
            super::Response::failure("failed".into(), "denied", "Denied"),
        ] {
            let original = serde_json::to_value(&response).unwrap();
            let super::NativeFrame::Response(decoded) =
                serde_json::from_value(original.clone()).unwrap()
            else {
                panic!("response")
            };
            assert_eq!(
                serde_json::to_value(super::Response::from(decoded)).unwrap(),
                original
            );
        }
        let non_string_event: super::NativeFrame =
            serde_json::from_str(r#"{"event":null,"id":"reply","result":false}"#).unwrap();
        assert!(matches!(non_string_event, super::NativeFrame::Response(_)));
    }

    #[test]
    fn void_success_round_trips_without_becoming_a_missing_result() {
        let response = super::Response::success("void".into(), serde_json::Value::Null);
        let decoded: super::Response =
            serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(decoded.result, Some(serde_json::Value::Null));
        assert!(decoded.error.is_none());
        let failed: super::Response = serde_json::from_value(serde_json::json!({
            "id":"failed", "error":{"code":"denied","message":"Denied"}
        }))
        .unwrap();
        assert!(failed.result.is_none());
        assert!(failed.error.is_some());
    }

    use super::*;
    #[test]
    fn fragments_preserve_large_unicode_messages_and_boundaries() {
        let text = "Черновик 🦀".repeat(MAX_FRAME / 4);
        let mut input = Assembler::default();
        for message in [text.as_bytes(), b"second"] {
            let mut result = None;
            for (flag, part) in fragments(message).unwrap() {
                let frame = [&[flag], part].concat();
                let assembled = input.push(&frame).unwrap();
                if assembled.is_some() {
                    assert!(result.is_none());
                    result = assembled;
                }
            }
            assert_eq!(result.unwrap(), message);
        }
        input.finish().unwrap();
    }
    #[test]
    fn rejects_truncation_bad_flags_and_oversized_allocation() {
        for frame in [vec![], vec![0], vec![2, 1], vec![0; MAX_FRAME + 1]] {
            assert!(Assembler::default().push(&frame).is_err());
        }
        let mut partial = Assembler::default();
        assert!(partial.push(&[1, 42]).unwrap().is_none());
        assert!(partial.finish().is_err());
        let mut too_large = Assembler::default();
        let mut frame = vec![42; MAX_FRAME];
        frame[0] = 1;
        for _ in 0..32 {
            assert!(too_large.push(&frame).unwrap().is_none());
        }
        assert!(too_large.push(&frame).is_err());
        assert!(fragments(&[]).is_err());
        assert!(fragments(&vec![0; MAX_MESSAGE + 1]).is_err());
    }
}
