//! Literal output matches and UTF-8 decoder state shared with the Python monitor format.
use crate::monitor_policy::MonitorError;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{
    MONITOR_OUTPUT_MAX_MATCH_COUNT, MONITOR_OUTPUT_MAX_PATTERN_BYTES, MONITOR_OUTPUT_MAX_PATTERNS,
    MONITOR_OUTPUT_RETAINED_EVENT_IDS, MONITOR_OUTPUT_TAIL_CODEPOINTS,
    MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS,
};

/// Maximum output wake batches available to one monitor.
pub const OUTPUT_WAKE_CAPACITY: u64 = 10;
/// Seconds required to replenish one output wake token.
pub const OUTPUT_WAKE_REFILL_SECONDS: f64 = 2.0;
/// Seconds without a further suppression that end a flood episode.
pub const OUTPUT_WAKE_RECOVERY_SECONDS: f64 = 3.0 * OUTPUT_WAKE_REFILL_SECONDS;
/// Seconds of sustained budget suppression before output wakes pause.
pub const OUTPUT_WAKE_FLOOD_SECONDS: f64 = 30.0;
/// Maximum UTF-8 bytes in one matched-line excerpt.
pub const OUTPUT_EXCERPT_BYTES: usize = 1024;
/// Bytes retained per stream to supply context for the next excerpt.
pub const OUTPUT_CONTEXT_BYTES: usize = OUTPUT_EXCERPT_BYTES;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WakeBudget {
    tokens: f64,
    updated_at: f64,
    suppressed_matches: u64,
    total_suppressed_matches: u64,
    paused: bool,
    flood_since: Option<f64>,
    last_suppressed_at: Option<f64>,
    notice_sent: bool,
}
impl WakeBudget {
    fn new(now: f64) -> Self {
        Self {
            tokens: OUTPUT_WAKE_CAPACITY as f64,
            updated_at: now,
            suppressed_matches: 0,
            total_suppressed_matches: 0,
            paused: false,
            flood_since: None,
            last_suppressed_at: None,
            notice_sent: false,
        }
    }
}

/// Validate Rust-owned additive state, including its retained byte bounds.
/// Legacy records omit this state and acquire it on their next output.
pub fn validate(record: &Value) -> Result<(), &'static str> {
    if let Some(value) = record.get("outputWakeBudget") {
        let budget: WakeBudget =
            serde_json::from_value(value.clone()).map_err(|_| "outputWakeBudget")?;
        if !(0.0..=OUTPUT_WAKE_CAPACITY as f64).contains(&budget.tokens)
            || !budget.updated_at.is_finite()
            || budget.updated_at < 0.0
            || [budget.flood_since, budget.last_suppressed_at]
                .into_iter()
                .flatten()
                .any(|time| !time.is_finite() || time < 0.0)
        {
            return Err("outputWakeBudget");
        }
    }
    for stream in ["stdout", "stderr"] {
        if record
            .get("outputLineTail")
            .and_then(|value| value.get(stream))
            .is_some()
        {
            let context = bytes(record, "outputLineTail", stream).map_err(|_| "outputLineTail")?;
            let offsets = record["outputLineOffsets"][stream]
                .as_array()
                .ok_or("outputLineOffsets")?;
            if context.len() > OUTPUT_CONTEXT_BYTES || offsets.len() != context.len() {
                return Err("outputLineOffsets");
            }
            let mut last = None;
            for value in offsets {
                match value.as_u64() {
                    Some(offset)
                        if offset < record["bytes"].as_u64().unwrap_or(0)
                            && last.is_none_or(|previous| previous < offset) =>
                    {
                        last = Some(offset);
                    }
                    None if value.is_null() && last.is_none() => (),
                    _ => return Err("outputLineOffsets"),
                }
            }
        }
    }
    if let Some(excerpts) = record.get("outputMatchExcerpts") {
        let excerpts = excerpts.as_object().ok_or("outputMatchExcerpts")?;
        if excerpts.len() > MONITOR_OUTPUT_MAX_PATTERNS * 2 {
            return Err("outputMatchExcerpts");
        }
        for excerpt in excerpts.values() {
            if excerpt["excerpt"]
                .as_str()
                .is_none_or(|text| text.len() > OUTPUT_EXCERPT_BYTES)
                || excerpt["logOffset"]
                    .as_u64()
                    .is_none_or(|offset| offset >= record["bytes"].as_u64().unwrap_or(0))
            {
                return Err("outputMatchExcerpts");
            }
        }
    }
    Ok(())
}

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
    let matcher_tail = bytes(record, "outputMatcherTail", stream)?;
    let previous = match record.get("outputLineTail").and_then(|v| v.get(stream)) {
        Some(_) => bytes(record, "outputLineTail", stream)?,
        None => matcher_tail,
    };
    let offset = record["bytes"].as_u64().unwrap_or(0);
    let mut offsets: Vec<Option<u64>> = record["outputLineOffsets"][stream]
        .as_array()
        .filter(|values| values.len() == previous.len())
        .map(|values| values.iter().map(Value::as_u64).collect())
        // Legacy matcher tails have no stream-to-log offsets. Never invent them.
        .unwrap_or_else(|| vec![None; previous.len()]);
    offsets.extend((0..chunk.len()).map(|index| Some(offset + index as u64)));
    let first_known = offsets
        .iter()
        .position(Option::is_some)
        .unwrap_or(previous.len());
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
        let mut hits = combined[start..]
            .windows(needle.len())
            .enumerate()
            .filter_map(|(index, part)| (part == needle).then_some(start + index));
        let first = hits.next();
        let count = first.map_or(0, |_| 1 + hits.count());
        if count > 0 {
            let key = format!("{stream}:{index}");
            increment(record, "outputMatchesPending", &key, count);
            increment(record, "outputMatchesTotal", &key, count);
            if !record["outputMatchExcerpts"].is_object() {
                record["outputMatchExcerpts"] = json!({});
            }
            if record["outputMatchExcerpts"].get(&key).is_none() {
                let first = first.expect("nonzero match count");
                let line_start = combined[..first]
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |index| index + 1);
                // Each malformed source byte can expand to one replacement character.
                // Reserve that expansion so the matched literal remains in the excerpt.
                let raw_limit = OUTPUT_EXCERPT_BYTES / '\u{fffd}'.len_utf8();
                let excerpt_start = line_start
                    .max((first + needle.len()).saturating_sub(raw_limit))
                    .max(first_known);
                let match_end = first + needle.len();
                let line_end = combined[match_end..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(combined.len(), |index| match_end + index);
                let excerpt_end = line_end.min(excerpt_start + raw_limit);
                let mut excerpt =
                    String::from_utf8_lossy(&combined[excerpt_start..excerpt_end]).into_owned();
                let mut end = excerpt.len().min(OUTPUT_EXCERPT_BYTES);
                while !excerpt.is_char_boundary(end) {
                    end -= 1;
                }
                excerpt.truncate(end);
                record["outputMatchExcerpts"][&key] =
                    json!({"excerpt":excerpt,"logOffset":offsets[excerpt_start]});
            }
        }
    }
    let context_start = combined.len().saturating_sub(OUTPUT_CONTEXT_BYTES);
    save_bytes(record, "outputLineTail", stream, &combined[context_start..]);
    if !record["outputLineOffsets"].is_object() {
        record["outputLineOffsets"] = json!({});
    }
    record["outputLineOffsets"][stream] = json!(&offsets[context_start..]);
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
    let mut matches = counts(record, "outputMatchesPending");
    if matches.is_empty()
        || (!final_chunk
            && now - record["outputWakeAt"].as_f64().unwrap_or(0.0)
                < MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS)
    {
        return None;
    }
    // Excerpts retain the first matching line for each stream/pattern in this batch.
    if let Some(excerpts) = record["outputMatchExcerpts"].as_object() {
        for item in &mut matches {
            let stream = item["stream"].as_str().unwrap_or_default();
            let index = record["outputPatterns"]
                .as_array()?
                .iter()
                .position(|pattern| pattern == &item["pattern"])?;
            if let Some(excerpt) = excerpts.get(&format!("{stream}:{index}")) {
                item["excerpt"] = excerpt["excerpt"].clone();
                item["logOffset"] = excerpt["logOffset"].clone();
            }
        }
    }
    let count = matches
        .iter()
        .filter_map(|item| item["count"].as_u64())
        .fold(0_u64, u64::saturating_add);
    let mut budget = record
        .get("outputWakeBudget")
        .map(|value| serde_json::from_value::<WakeBudget>(value.clone()))
        .transpose()
        .ok()?
        .unwrap_or_else(|| WakeBudget::new(now));
    budget.tokens = (budget.tokens
        + (now - budget.updated_at).max(0.0) / OUTPUT_WAKE_REFILL_SECONDS)
        .min(OUTPUT_WAKE_CAPACITY as f64);
    budget.updated_at = now.max(budget.updated_at);
    if !budget.paused
        && budget
            .last_suppressed_at
            .is_some_and(|last| now - last > OUTPUT_WAKE_RECOVERY_SECONDS)
    {
        budget.flood_since = None;
    }
    let suppress = budget.paused || budget.tokens < 1.0;
    let mut notice = false;
    let mut newly_paused = false;
    if suppress {
        budget.suppressed_matches = budget.suppressed_matches.saturating_add(count);
        budget.total_suppressed_matches = budget.total_suppressed_matches.saturating_add(count);
        if !budget.paused {
            let since = *budget.flood_since.get_or_insert(now);
            budget.last_suppressed_at = Some(now);
            newly_paused = now - since >= OUTPUT_WAKE_FLOOD_SECONDS;
            budget.paused = newly_paused;
            // One initial notice, then suppression counts ride admitted batches.
            notice = !budget.notice_sent || newly_paused || final_chunk;
            budget.notice_sent = true;
        }
    } else {
        budget.tokens -= 1.0;
    }
    record["outputMatchesPending"] = json!({});
    record["outputMatchExcerpts"] = json!({});
    let suppressed = if newly_paused {
        budget.total_suppressed_matches
    } else {
        budget.suppressed_matches
    };
    if !suppress || notice {
        budget.suppressed_matches = 0;
    }
    record["outputWakeBudget"] = json!(budget);
    if suppress && !notice {
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
    let mut payload = json!({"monitorId":record["id"],"epoch":record["epoch"],"sequence":sequence,"matches":matches,"final":final_chunk});
    if suppressed > 0 {
        payload["suppressedMatches"] = json!(suppressed);
        payload["message"] = json!(if newly_paused {
            format!(
                "Output-match wakes paused after sustained output. {suppressed} matches suppressed. The command and exit wake remain active. To resume output wakes, cancel this monitor and start a new monitor with more selective output_patterns."
            )
        } else {
            format!("{suppressed} output matches suppressed by the monitor wake budget.")
        });
    }
    if newly_paused {
        payload["outputWakesPaused"] = json!(true);
    }
    Some((id, payload))
}
pub fn summary(record: &Value) -> Option<Value> {
    if record["outputPatterns"]
        .as_array()
        .is_none_or(Vec::is_empty)
    {
        return None;
    }
    let mut summary = json!({"events":record["outputEventCount"].as_u64().unwrap_or(0),"matches":counts(record,"outputMatchesTotal"),"pending":counts(record,"outputMatchesPending")});
    if let Some(suppressed) = record["outputWakeBudget"]["totalSuppressedMatches"]
        .as_u64()
        .filter(|count| *count > 0)
    {
        summary["suppressedMatches"] = json!(suppressed);
        summary["outputWakesPaused"] = json!(record["outputWakeBudget"]["paused"] == true);
    }
    Some(summary)
}
