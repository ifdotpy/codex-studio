use serde::Deserialize;
use serde_json::{Value, json};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq)]
pub struct TaskFeedCursor {
    pub id: String,
    pub time: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorKind {
    Current,
    Before,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCursor;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFeedCursorInput {
    id: String,
    #[serde(default, deserialize_with = "deserialize_cursor_number")]
    updated: Option<CursorNumber>,
    #[serde(default, deserialize_with = "deserialize_cursor_number")]
    created: Option<CursorNumber>,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "serde_json::Number")]
struct CursorNumber(f64);

impl TryFrom<serde_json::Number> for CursorNumber {
    type Error = &'static str;

    fn try_from(value: serde_json::Number) -> Result<Self, Self::Error> {
        if value
            .as_i64()
            .is_some_and(|integer| integer < 0 || integer as u64 > MAX_SAFE_INTEGER)
            || value
                .as_u64()
                .is_some_and(|integer| integer > MAX_SAFE_INTEGER)
        {
            return Err("integer is outside the safe range");
        }
        let number = value.as_f64().ok_or("invalid cursor number")?;
        number
            .is_finite()
            .then_some(Self(number))
            .ok_or("number is not finite")
    }
}

fn deserialize_cursor_number<'de, D>(deserializer: D) -> Result<Option<CursorNumber>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    CursorNumber::deserialize(deserializer).map(Some)
}

pub fn parse_task_cursor(
    value: Option<&str>,
    kind: CursorKind,
) -> Result<Option<TaskFeedCursor>, InvalidCursor> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let parsed: TaskFeedCursorInput = serde_json::from_str(value).map_err(|_| InvalidCursor)?;
    let time = match kind {
        CursorKind::Current if parsed.created.is_none() => parsed.updated,
        CursorKind::Before if parsed.updated.is_none() => parsed.created,
        _ => return Err(InvalidCursor),
    }
    .ok_or(InvalidCursor)?;
    Ok(Some(TaskFeedCursor {
        id: parsed.id,
        time: time.0,
    }))
}

pub fn task_feed_response(
    tasks: Vec<Value>,
    has_more: bool,
    cursor: Option<&TaskFeedCursor>,
    before: Option<&TaskFeedCursor>,
) -> Value {
    let latest = tasks
        .iter()
        .map(task_cursor)
        .max_by(|left, right| {
            left.time
                .total_cmp(&right.time)
                .then_with(|| left.id.cmp(&right.id))
        })
        .unwrap_or(TaskFeedCursor {
            id: String::new(),
            time: 0.0,
        });
    let next_before = has_more.then(|| tasks.last()).flatten().map(|task| {
        json!({
            "created": task.get("created").cloned().unwrap_or(json!(0)),
            "id": task.get("id").cloned().unwrap_or(Value::Null),
        })
    });
    if before.is_some() {
        json!({"tasks": tasks, "hasMore": has_more, "nextBefore": next_before})
    } else if let Some(cursor) = cursor {
        let next = tasks
            .last()
            .map(task_cursor)
            .unwrap_or_else(|| cursor.clone());
        json!({
            "tasks": tasks, "cursor": {"updated": next.time, "id": next.id},
            "hasMore": false, "hasMoreChanges": has_more, "nextBefore": null, "reset": false,
        })
    } else {
        json!({
            "tasks": tasks, "cursor": {"updated": latest.time, "id": latest.id},
            "hasMore": has_more, "nextBefore": next_before, "reset": false,
        })
    }
}

fn task_cursor(task: &Value) -> TaskFeedCursor {
    let created = task.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    let finished = task.get("finished").and_then(Value::as_f64).unwrap_or(0.0);
    TaskFeedCursor {
        id: task
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        time: created.max(finished),
    }
}

#[cfg(test)]
mod tests {
    use super::{CursorKind, parse_task_cursor, task_feed_response};
    use serde_json::json;

    #[test]
    fn typed_cursor_rejects_wrong_fields_unknown_fields_and_unsafe_integers() {
        assert_eq!(
            parse_task_cursor(Some(r#"{"id":"task-1","updated":2}"#), CursorKind::Current)
                .expect("valid current cursor parses")
                .expect("cursor present")
                .time,
            2.0
        );
        assert!(parse_task_cursor(Some("{"), CursorKind::Current).is_err());
        assert!(
            parse_task_cursor(Some(""), CursorKind::Current)
                .expect("empty cursor is valid")
                .is_none()
        );
        assert!(parse_task_cursor(Some(r#"{"id":"x","created":2}"#), CursorKind::Current).is_err());
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"x","updated":2,"extra":true}"#),
                CursorKind::Current
            )
            .is_err()
        );
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"x","updated":9007199254740992}"#),
                CursorKind::Current
            )
            .is_err()
        );
        assert!(
            parse_task_cursor(Some(r#"{"id":"x","updated":-1}"#), CursorKind::Current).is_err()
        );
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"x","updated":null,"created":2}"#),
                CursorKind::Before
            )
            .is_err()
        );
    }

    #[test]
    fn changes_cursor_advances_to_the_last_row_and_empty_poll_preserves_it() {
        let task = json!({"id":"a","created":1,"finished":2});
        let first = task_feed_response(
            vec![task],
            false,
            Some(&super::TaskFeedCursor {
                id: String::new(),
                time: 0.0,
            }),
            None,
        );
        let cursor = super::TaskFeedCursor {
            id: "a".into(),
            time: 2.0,
        };
        assert_eq!(first["cursor"], json!({"updated":2.0,"id":"a"}));
        let second = task_feed_response(vec![], false, Some(&cursor), None);
        assert_eq!(second["cursor"], first["cursor"]);
        assert!(second["tasks"].as_array().expect("tasks array").is_empty());
    }

    #[test]
    fn first_page_cursor_and_history_cursor_follow_python_contract() {
        let response = task_feed_response(
            vec![json!({"id":"a","created":2.0,"finished":5.0})],
            true,
            None,
            None,
        );
        assert_eq!(response["cursor"], json!({"updated":5.0,"id":"a"}));
        assert_eq!(response["nextBefore"], json!({"created":2.0,"id":"a"}));
        assert_eq!(response["hasMore"], true);
    }
}
