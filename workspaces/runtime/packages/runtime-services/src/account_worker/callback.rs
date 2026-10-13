//! Callback services plug into this typed dispatch boundary.
use serde_json::json;
use studio_runtime_providers::{Frame, ResponseBody};

pub enum CallbackDecision {
    Answer(ResponseBody),
    Unsupported,
}
pub fn dispatch(_request: &Frame) -> CallbackDecision {
    CallbackDecision::Unsupported
}
pub(super) fn response(frame: &Frame) -> Option<Frame> {
    let id = match frame {
        Frame::Request(request) => request.id.clone(),
        Frame::UnknownMethod(request) => {
            serde_json::from_value(request.fields.get("id")?.clone()).ok()?
        }
        _ => return None,
    };
    let body = match dispatch(frame) {
        CallbackDecision::Answer(body) => body,
        CallbackDecision::Unsupported => ResponseBody::Error(
            json!({"code":-32601,"message":"Provider callback is not supported by this runtime","data":{"kind":"unsupported_callback"}}),
        ),
    };
    Some(Frame::Response {
        id,
        body,
        fields: Default::default(),
    })
}
