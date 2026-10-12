use serde::Deserialize;
use serde_json::{Value, json};

const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

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
struct CurrentCursorInput {
    id: String,
    updated: serde_json::Number,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BeforeCursorInput {
    id: String,
    created: serde_json::Number,
}

pub fn parse_task_cursor(
    value: Option<&str>,
    kind: CursorKind,
) -> Result<Option<TaskFeedCursor>, InvalidCursor> {
    let Some(value) = value else {
        return Ok(None);
    };
    let (id, time) = match kind {
        CursorKind::Current => {
            let parsed: CurrentCursorInput =
                serde_json::from_str(value).map_err(|_| InvalidCursor)?;
            (parsed.id, parsed.updated)
        }
        CursorKind::Before => {
            let parsed: BeforeCursorInput =
                serde_json::from_str(value).map_err(|_| InvalidCursor)?;
            (parsed.id, parsed.created)
        }
    };
    if (time.is_i64()
        && time
            .as_i64()
            .is_some_and(|number| number.unsigned_abs() > MAX_SAFE_INTEGER as u64))
        || (time.is_u64()
            && time
                .as_u64()
                .is_some_and(|number| number > MAX_SAFE_INTEGER as u64))
    {
        return Err(InvalidCursor);
    }
    let time = time
        .as_f64()
        .filter(|number| number.is_finite())
        .ok_or(InvalidCursor)?;
    Ok(Some(TaskFeedCursor { id, time }))
}

pub fn task_feed_response(
    tasks: Vec<Value>,
    rows_returned: usize,
    cursor: Option<&TaskFeedCursor>,
    before: Option<&TaskFeedCursor>,
    limit: usize,
    last_sort_value: Option<f64>,
) -> Value {
    let has_more = rows_returned > limit;
    let page = tasks.into_iter().take(limit).collect::<Vec<_>>();
    let next_before = || {
        has_more
            .then(|| {
                page.last().map(|task| {
                    json!({
                        "created": task.get("created").cloned().unwrap_or(json!(0)),
                        "id": task.get("id").cloned().unwrap_or(Value::Null),
                    })
                })
            })
            .flatten()
    };

    if before.is_some() {
        return json!({"tasks": page, "hasMore": has_more, "nextBefore": next_before()});
    }
    if let Some(cursor) = cursor {
        let next = page.last().map_or_else(
            || json!({"updated": cursor.time, "id": cursor.id}),
            |_| {
                json!({
                    "updated": last_sort_value.unwrap_or(cursor.time),
                    "id": page.last().and_then(|task| task.get("id")).cloned().unwrap_or(Value::Null),
                })
            },
        );
        return json!({
            "tasks": page,
            "cursor": next,
            "hasMore": false,
            "hasMoreChanges": has_more,
            "nextBefore": null,
            "reset": false,
        });
    }

    let latest = page
        .iter()
        .map(|task| {
            let created = task.get("created").and_then(Value::as_f64).unwrap_or(0.0);
            let finished = task.get("finished").and_then(Value::as_f64).unwrap_or(0.0);
            (
                created.max(finished),
                task.get("id").and_then(Value::as_str).unwrap_or(""),
            )
        })
        .max_by(|left, right| left.0.total_cmp(&right.0).then_with(|| left.1.cmp(right.1)))
        .unwrap_or((0.0, ""));
    json!({
        "tasks": page,
        "cursor": {"updated": latest.0, "id": latest.1},
        "hasMore": has_more,
        "nextBefore": next_before(),
        "reset": false,
    })
}

#[cfg(test)]
mod tests {
    use super::{CursorKind, parse_task_cursor, task_feed_response};
    use serde_json::json;

    #[test]
    fn cursor_is_a_strict_json_object_with_the_expected_time_field() {
        assert_eq!(
            parse_task_cursor(Some(r#"{"id":"task-1","updated":2}"#), CursorKind::Current)
                .expect("valid current cursor parses")
                .expect("cursor present")
                .time,
            2.0
        );
        assert!(parse_task_cursor(Some("{"), CursorKind::Current).is_err());
        assert!(
            parse_task_cursor(Some(r#"{"id":"task-1","created":2}"#), CursorKind::Current).is_err()
        );
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"task-1","updated":2,"extra":true}"#),
                CursorKind::Current
            )
            .is_err()
        );
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"task-1","updated":9007199254740992}"#),
                CursorKind::Current
            )
            .is_err()
        );
    }

    #[test]
    fn first_page_cursor_and_history_cursor_follow_python_contract() {
        let response = task_feed_response(
            vec![json!({"id":"a","created":2.0,"finished":5.0})],
            2,
            None,
            None,
            1,
            None,
        );
        assert_eq!(response["cursor"], json!({"updated":5.0,"id":"a"}));
        assert_eq!(response["nextBefore"], json!({"created":2.0,"id":"a"}));
        assert_eq!(response["hasMore"], true);
    }
}
