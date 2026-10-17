//! Literal output matches and UTF-8 decoder state shared with the Python monitor format.
use crate::monitor_policy::MonitorError;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    MONITOR_OUTPUT_MAX_MATCH_COUNT, MONITOR_OUTPUT_MAX_PATTERN_BYTES, MONITOR_OUTPUT_MAX_PATTERNS,
    MONITOR_OUTPUT_RETAINED_EVENT_IDS, MONITOR_OUTPUT_TAIL_CODEPOINTS,
    MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS,
};

pub fn patterns(value: Option<&Value>) -> Result<Vec<String>, MonitorError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    let values = value
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= MONITOR_OUTPUT_MAX_PATTERNS)
        .ok_or(MonitorError::Refused(
            "output_patterns must contain up to 16 non-empty literal strings",
        ))?;
    let mut result = Vec::new();
    for value in values {
        let value = value
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or(MonitorError::Refused(
                "output_patterns must contain up to 16 non-empty literal strings",
            ))?;
        if value.len() > MONITOR_OUTPUT_MAX_PATTERN_BYTES {
            return Err(MonitorError::Refused(
                "Each output pattern must be at most 256 UTF-8 bytes",
            ));
        }
        if result.iter().any(|v| v == value) {
            return Err(MonitorError::Refused(
                "output_patterns must not contain duplicate strings",
            ));
        }
        result.push(value.to_owned());
    }
    Ok(result)
}

fn bytes(record: &Value, field: &str, stream: &str) -> Result<Vec<u8>, MonitorError> {
    match record
        .get(field)
        .and_then(|v| v.get(stream))
        .and_then(Value::as_str)
    {
        Some(value) => STANDARD
            .decode(value)
            .map_err(|_| MonitorError::InvalidOutputReceipt("invalid saved matcher bytes")),
        None => Ok(vec![]),
    }
}
fn save_bytes(record: &mut Value, field: &str, stream: &str, bytes: &[u8]) {
    if !record[field].is_object() {
        record[field] = json!({});
    }
    if bytes.is_empty() {
        record[field]
            .as_object_mut()
            .expect("object")
            .remove(stream);
    } else {
        record[field][stream] = json!(STANDARD.encode(bytes));
    }
}
fn append_tail(record: &mut Value, text: &str) {
    if text.is_empty() {
        return;
    }
    let tail = format!("{}{text}", record["tail"].as_str().unwrap_or_default());
    record["tail"] = json!(
        tail.chars()
            .skip(
                tail.chars()
                    .count()
                    .saturating_sub(MONITOR_OUTPUT_TAIL_CODEPOINTS)
            )
            .collect::<String>()
    );
}
fn decode(
    record: &mut Value,
    stream: &str,
    chunk: &[u8],
    final_chunk: bool,
) -> Result<(), MonitorError> {
    let mut combined = bytes(record, "outputUtf8Pending", stream)?;
    combined.extend_from_slice(chunk);
    let mut remaining = combined.as_slice();
    let mut text = String::new();
    loop {
        match std::str::from_utf8(remaining) {
            Ok(value) => {
                text.push_str(value);
                remaining = &[];
                break;
            }
            Err(error) => {
                text.push_str(
                    std::str::from_utf8(&remaining[..error.valid_up_to()]).expect("valid prefix"),
                );
                remaining = &remaining[error.valid_up_to()..];
                match error.error_len() {
                    Some(length) => {
                        text.push('\u{fffd}');
                        remaining = &remaining[length..];
                    }
                    None => {
                        if final_chunk {
                            text.push('\u{fffd}');
                            remaining = &[];
                        }
                        break;
                    }
                }
            }
        }
    }
    save_bytes(record, "outputUtf8Pending", stream, remaining);
    append_tail(record, &text);
    Ok(())
}
fn increment(record: &mut Value, field: &str, key: &str, count: usize) {
    if !record[field].is_object() {
        record[field] = json!({});
    }
    let previous = record[field][key].as_u64().unwrap_or(0);
    record[field][key] = json!(
        previous
            .saturating_add(count as u64)
            .min(MONITOR_OUTPUT_MAX_MATCH_COUNT as u64)
    );
}
pub fn append(
    record: &mut Value,
    stream: &str,
    chunk: &[u8],
    match_output: bool,
) -> Result<(), MonitorError> {
    if !matches!(stream, "stdout" | "stderr") {
        return Err(MonitorError::InvalidOutputReceipt("invalid output stream"));
    }
    decode(record, stream, chunk, false)?;
    let patterns = record["outputPatterns"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !match_output || patterns.is_empty() || chunk.is_empty() {
        return Ok(());
    }
    let previous = bytes(record, "outputMatcherTail", stream)?;
    let mut combined = previous.clone();
    combined.extend_from_slice(chunk);
    for field in ["outputMatchesPending", "outputMatchesTotal"] {
        if !record[field].is_object() {
            record[field] = json!({});
        }
    }
    let mut longest = 0;
    for (index, pattern) in patterns.iter().enumerate() {
        let needle = pattern
            .as_str()
            .ok_or(MonitorError::InvalidOutputReceipt("invalid saved pattern"))?
            .as_bytes();
        if needle.is_empty() || needle.len() > MONITOR_OUTPUT_MAX_PATTERN_BYTES {
            return Err(MonitorError::InvalidOutputReceipt(
                "invalid saved pattern size",
            ));
        }
        longest = longest.max(needle.len());
        let start = previous.len().saturating_sub(needle.len() - 1);
        let count = combined[start..]
            .windows(needle.len())
            .filter(|part| *part == needle)
            .count();
        if count > 0 {
            let key = format!("{stream}:{index}");
            increment(record, "outputMatchesPending", &key, count);
            increment(record, "outputMatchesTotal", &key, count);
        }
    }
    let keep = longest.saturating_sub(1);
    save_bytes(
        record,
        "outputMatcherTail",
        stream,
        &combined[combined.len().saturating_sub(keep)..],
    );
    Ok(())
}
pub fn flush(record: &mut Value) -> Result<(), MonitorError> {
    for stream in ["stdout", "stderr"] {
        decode(record, stream, &[], true)?;
    }
    record
        .as_object_mut()
        .expect("monitor object")
        .remove("outputMatcherTail");
    Ok(())
}
fn counts(record: &Value, field: &str) -> Vec<Value> {
    let Some(values) = record[field].as_object() else {
        return vec![];
    };
    values
        .iter()
        .filter_map(|(key, count)| {
            let (stream, index) = key.split_once(':')?;
            let pattern = record["outputPatterns"].get(index.parse::<usize>().ok()?)?;
            (count.as_u64()? > 0).then(|| json!({"stream":stream,"pattern":pattern,"count":count}))
        })
        .collect()
}
pub fn pending(record: &mut Value, now: f64, final_chunk: bool) -> Option<(String, Value)> {
    let matches = counts(record, "outputMatchesPending");
    if matches.is_empty()
        || (!final_chunk
            && now - record["outputWakeAt"].as_f64().unwrap_or(0.0)
                < MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS)
    {
        return None;
    }
    let sequence = record["outputEventSequence"]
        .as_u64()
        .unwrap_or(0)
        .checked_add(1)?;
    let id = format!(
        "monitor-output:{}:{}:{sequence}",
        record["id"].as_str()?,
        record["epoch"].as_i64()?
    );
    let mut ids = record["outputEventIds"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    ids.push(json!(id));
    let keep = ids.len().saturating_sub(MONITOR_OUTPUT_RETAINED_EVENT_IDS);
    ids.drain(..keep);
    record["outputEventSequence"] = json!(sequence);
    record["outputEventCount"] = json!(
        record["outputEventCount"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(1)
            .min(MONITOR_OUTPUT_MAX_MATCH_COUNT as u64)
    );
    record["outputWakeAt"] = json!(now);
    record["outputEventIds"] = json!(ids);
    record["outputMatchesPending"] = json!({});
    Some((
        id.clone(),
        json!({"monitorId":record["id"],"epoch":record["epoch"],"sequence":sequence,"matches":matches,"final":final_chunk}),
    ))
}
pub fn summary(record: &Value) -> Option<Value> {
    if record["outputPatterns"]
        .as_array()
        .is_none_or(Vec::is_empty)
    {
        return None;
    }
    Some(
        json!({"events":record["outputEventCount"].as_u64().unwrap_or(0),"matches":counts(record,"outputMatchesTotal"),"pending":counts(record,"outputMatchesPending")}),
    )
}
