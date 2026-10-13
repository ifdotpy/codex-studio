//! Callback services plug into this typed dispatch boundary.
use serde_json::{Value, json};
use studio_runtime_providers::{Frame, Method, RequestId, ResponseBody};

#[derive(Clone, Copy)]
pub enum CallbackFamily {
    Approval,
    Question,
    DynamicTool,
    Other,
}
pub struct CallbackRequest {
    pub id: RequestId,
    pub family: CallbackFamily,
    pub params: Value,
}
pub enum CallbackDecision {
    Answer(ResponseBody),
    Unsupported,
}
pub fn dispatch(_request: &CallbackRequest) -> CallbackDecision {
    CallbackDecision::Unsupported
}
pub(super) fn response(frame: &Frame) -> Option<Frame> {
    let (id, method, params) = match frame {
        Frame::Request(request) => (
            request.id.clone(),
            Some(request.method),
            request.params.clone(),
        ),
        Frame::UnknownMethod(request) => (
            serde_json::from_value(request.fields.get("id")?.clone()).ok()?,
            None,
            request.fields.get("params").cloned(),
        ),
        _ => return None,
    };
    let request = CallbackRequest {
        id,
        family: match method {
            Some(
                Method::ItemCommandExecutionRequestApproval
                | Method::ItemFileChangeRequestApproval
                | Method::ItemPermissionsRequestApproval,
            ) => CallbackFamily::Approval,
            Some(Method::ItemToolRequestUserInput) => CallbackFamily::Question,
            Some(Method::ItemToolCall) => CallbackFamily::DynamicTool,
            _ => CallbackFamily::Other,
        },
        params: params.unwrap_or(Value::Null),
    };
    let body = match dispatch(&request) {
        CallbackDecision::Answer(body) => body,
        CallbackDecision::Unsupported => ResponseBody::Error(
            json!({"code":-32601,"message":"Provider callback is not supported by this runtime","data":{"kind":"unsupported_callback"}}),
        ),
    };
    Some(Frame::Response {
        id: request.id,
        body,
        fields: Default::default(),
    })
}
