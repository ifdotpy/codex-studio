//! Complaint response state for orchestration context.
use crate::continuation::text;
use serde_json::Value;

/// Python's `Runtime.complaint_needs_response`: the responsible side has not answered.
pub(crate) fn needs_response(c: &Value) -> bool {
    if matches!(text(c, "status"), "resolved" | "declined") {
        return false;
    }
    if text(c, "sourceType") == "user_task" && text(c, "status") == "open" {
        return true;
    }
    let responsible = if text(c, "recipient") == "user" {
        "user"
    } else {
        text(c, "leadId")
    };
    !c.get("responses")
        .and_then(Value::as_array)
        .is_some_and(|rs| rs.iter().any(|r| text(r, "author") == responsible))
}
