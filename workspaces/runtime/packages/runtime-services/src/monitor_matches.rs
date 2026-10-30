//! Literal output matches and UTF-8 decoder state shared with the Python monitor format.
use crate::monitor_policy::MonitorError;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use studio_runtime_schema::monitor_record::{MonitorRecord, MonitorStream, StreamValues, OutputMatchExcerpt, OutputWakeBudget};
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

fn new_budget(now: f64) -> OutputWakeBudget {
    OutputWakeBudget { tokens: OUTPUT_WAKE_CAPACITY as f64, updated_at: now, suppressed_matches: 0, total_suppressed_matches: 0, paused: false, flood_since: None, last_suppressed_at: None, notice_sent: false }
}

/// Validate retained cross-field output bounds.
pub fn validate(record: &MonitorRecord) -> Result<(), &'static str> {
    if let Some(budget) = &record.output_wake_budget
        && (!(0.0..=OUTPUT_WAKE_CAPACITY as f64).contains(&budget.tokens)
            || !budget.updated_at.is_finite() || budget.updated_at < 0.0
            || [budget.flood_since, budget.last_suppressed_at].into_iter().flatten().any(|time| !time.is_finite() || time < 0.0)) {
        return Err("outputWakeBudget");
    }
    for stream in [MonitorStream::Stdout, MonitorStream::Stderr] {
        if record.output_line_tail.as_ref().and_then(|tail| tail.get(stream)).is_some() {
            let context = bytes(&record.output_line_tail, stream).map_err(|_| "outputLineTail")?;
            let offsets = record.output_line_offsets.as_ref().and_then(|offsets| offsets.get(stream)).ok_or("outputLineOffsets")?;
            if context.len() > OUTPUT_CONTEXT_BYTES || offsets.len() != context.len() { return Err("outputLineOffsets"); }
            let mut last = None;
            for value in offsets {
                match value {
                    Some(offset) if *offset < record.bytes.get() as u64 && last.is_none_or(|previous| previous < *offset) => last = Some(*offset),
                    None if last.is_none() => (),
                    _ => return Err("outputLineOffsets"),
                }
            }
        }
    }
    if let Some(excerpts) = &record.output_match_excerpts
        && (excerpts.len() > MONITOR_OUTPUT_MAX_PATTERNS * 2 || excerpts.values().any(|value| value.excerpt.len() > OUTPUT_EXCERPT_BYTES || value.log_offset >= record.bytes.get() as u64)) {
        return Err("outputMatchExcerpts");
    }
    Ok(())
}

pub fn patterns(values: Option<&[String]>) -> Result<Vec<String>, MonitorError> {
    let Some(values) = values else { return Ok(vec![]); };
    if values.is_empty() || values.len() > MONITOR_OUTPUT_MAX_PATTERNS {
        return Err(MonitorError::Refused("output_patterns must contain up to 16 non-empty literal strings"));
    }
    let mut result = Vec::new();
    for value in values {
        if value.is_empty() { return Err(MonitorError::Refused("output_patterns must contain up to 16 non-empty literal strings")); }
        if value.len() > MONITOR_OUTPUT_MAX_PATTERN_BYTES { return Err(MonitorError::Refused("Each output pattern must be at most 256 UTF-8 bytes")); }
        if result.contains(value) { return Err(MonitorError::Refused("output_patterns must not contain duplicate strings")); }
        result.push(value.clone());
    }
    Ok(result)
}

fn bytes(fields: &Option<StreamValues<String>>, stream: MonitorStream) -> Result<Vec<u8>, MonitorError> {
    fields.as_ref().and_then(|fields| fields.get(stream)).map_or(Ok(vec![]), |value| STANDARD.decode(value).map_err(|_| MonitorError::InvalidOutputReceipt("invalid saved matcher bytes")))
}
fn save_bytes(fields: &mut Option<StreamValues<String>>, stream: MonitorStream, bytes: &[u8]) {
    let slot = fields.get_or_insert_with(StreamValues::default).slot(stream);
    *slot = (!bytes.is_empty()).then(|| STANDARD.encode(bytes));
}
fn append_tail(record: &mut MonitorRecord, text: &str) -> Result<(), MonitorError> {
    if !text.is_empty() {
        let tail = format!("{}{text}", record.tail.as_str());
        record.tail = tail.chars().skip(tail.chars().count().saturating_sub(MONITOR_OUTPUT_TAIL_CODEPOINTS)).collect::<String>().try_into().map_err(|_| MonitorError::InvalidOutputReceipt("monitor tail exceeds bound"))?;
    }
    Ok(())
}
fn decode(
    record: &mut MonitorRecord,
    stream: MonitorStream,
    chunk: &[u8],
    final_chunk: bool,
) -> Result<(), MonitorError> {
    let mut combined = bytes(&record.output_utf8_pending, stream)?;
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
    save_bytes(&mut record.output_utf8_pending, stream, remaining);
    append_tail(record, &text)?;
    Ok(())
}
fn increment(values: &mut Option<std::collections::BTreeMap<String, u64>>, key: &str, count: usize) {
    let entry = values.get_or_insert_with(Default::default).entry(key.to_owned()).or_default();
    *entry = entry.saturating_add(count as u64).min(MONITOR_OUTPUT_MAX_MATCH_COUNT as u64);
}
pub fn append(record: &mut MonitorRecord, stream: &str, chunk: &[u8], match_output: bool) -> Result<(), MonitorError> {
    let stream_kind = match stream { "stdout" => MonitorStream::Stdout, "stderr" => MonitorStream::Stderr, _ => return Err(MonitorError::InvalidOutputReceipt("invalid output stream")) };
    decode(record, stream_kind, chunk, false)?;
    let patterns = record.output_patterns.clone().unwrap_or_default();
    if !match_output || patterns.is_empty() || chunk.is_empty() { return Ok(()); }
    let matcher_tail = bytes(&record.output_matcher_tail, stream_kind)?;
    let previous = if record.output_line_tail.as_ref().and_then(|v| v.get(stream_kind)).is_some() { bytes(&record.output_line_tail, stream_kind)? } else { matcher_tail };
    let offset = record.bytes.get() as u64;
    let mut offsets = record.output_line_offsets.as_ref().and_then(|v| v.get(stream_kind)).filter(|v| v.len() == previous.len()).cloned().unwrap_or_else(|| vec![None; previous.len()]);
    offsets.extend((0..chunk.len()).map(|index| Some(offset + index as u64)));
    let first_known = offsets.iter().position(Option::is_some).unwrap_or(previous.len());
    let mut combined = previous.clone();
    combined.extend_from_slice(chunk);
    record.output_matches_pending.get_or_insert_with(Default::default);
    record.output_matches_total.get_or_insert_with(Default::default);
    let mut longest = 0;
    for (index, pattern) in patterns.iter().enumerate() {
        let needle = pattern.as_bytes();
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
            increment(&mut record.output_matches_pending, &key, count);
            increment(&mut record.output_matches_total, &key, count);
            let excerpts = record.output_match_excerpts.get_or_insert_with(Default::default);
            if let std::collections::btree_map::Entry::Vacant(entry) = excerpts.entry(key) {
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
                let log_offset = offsets[excerpt_start].ok_or(MonitorError::InvalidOutputReceipt("missing output offset"))?;
                entry.insert(OutputMatchExcerpt { excerpt, log_offset });
            }
        }
    }
    let context_start = combined.len().saturating_sub(OUTPUT_CONTEXT_BYTES);
    save_bytes(&mut record.output_line_tail, stream_kind, &combined[context_start..]);
    *record.output_line_offsets.get_or_insert_with(StreamValues::default).slot(stream_kind) = Some(offsets[context_start..].to_vec());
    let keep = longest.saturating_sub(1);
    save_bytes(&mut record.output_matcher_tail, stream_kind, &combined[combined.len().saturating_sub(keep)..]);
    Ok(())
}
pub fn flush(record: &mut MonitorRecord) -> Result<(), MonitorError> {
    for stream in [MonitorStream::Stdout, MonitorStream::Stderr] { decode(record, stream, &[], true)?; }
    record.output_matcher_tail = None;
    Ok(())
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputMatch {
    pub stream: String,
    pub pattern: String,
    pub count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_offset: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputEvent {
    pub monitor_id: String,
    pub epoch: i64,
    pub sequence: u64,
    pub matches: Vec<OutputMatch>,
    #[serde(rename = "final")]
    pub final_chunk: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressed_matches: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_wakes_paused: Option<bool>,
}

impl OutputEvent {
    /// Persist output events using the legacy Python sorted, ASCII JSON spelling.
    pub fn python_json(&self) -> String {
        use studio_runtime_schema::json_compat::python_quote;
        let quote = |text: &str| python_quote(text, true);
        let entries = self.matches.iter().map(|item| {
            let mut fields = vec![format!("\"count\": {}", item.count)];
            if let Some(excerpt) = &item.excerpt { fields.push(format!("\"excerpt\": {}", quote(excerpt))); }
            if let Some(offset) = item.log_offset { fields.push(format!("\"logOffset\": {offset}")); }
            fields.push(format!("\"pattern\": {}", quote(&item.pattern)));
            fields.push(format!("\"stream\": {}", quote(&item.stream)));
            format!("{{{}}}", fields.join(", "))
        }).collect::<Vec<_>>().join(", ");
        let mut fields = vec![format!("\"epoch\": {}", self.epoch), format!("\"final\": {}", self.final_chunk), format!("\"matches\": [{entries}]")];
        if let Some(message) = &self.message { fields.push(format!("\"message\": {}", quote(message))); }
        fields.push(format!("\"monitorId\": {}", quote(&self.monitor_id)));
        if let Some(paused) = self.output_wakes_paused { fields.push(format!("\"outputWakesPaused\": {paused}")); }
        fields.push(format!("\"sequence\": {}", self.sequence));
        if let Some(count) = self.suppressed_matches { fields.push(format!("\"suppressedMatches\": {count}")); }
        format!("{{{}}}", fields.join(", "))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputMatchSummary {
    pub events: u64,
    pub matches: Vec<OutputMatch>,
    pub pending: Vec<OutputMatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppressed_matches: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_wakes_paused: Option<bool>,
}

fn counts(record: &MonitorRecord, values: &Option<std::collections::BTreeMap<String, u64>>) -> Vec<OutputMatch> {
    values.iter().flat_map(|values| values.iter()).filter_map(|(key, count)| {
        let (stream, index) = key.split_once(':')?;
        let pattern = record.output_patterns.as_ref()?.get(index.parse::<usize>().ok()?)?;
        (*count > 0).then(|| OutputMatch { stream: stream.to_owned(), pattern: pattern.clone(), count: *count, excerpt: None, log_offset: None })
    }).collect()
}
pub fn pending(record: &mut MonitorRecord, now: f64, final_chunk: bool) -> Option<(String, OutputEvent)> {
    let mut matches = counts(record, &record.output_matches_pending);
    if matches.is_empty() || (!final_chunk && now - record.output_wake_at.unwrap_or(0.0) < MONITOR_OUTPUT_WAKE_COOLDOWN_SECONDS) { return None; }
    // Excerpts retain the first matching line for each stream/pattern in this batch.
    if let Some(excerpts) = record.output_match_excerpts.as_ref() {
        for item in &mut matches {
            let stream = &item.stream;
            let index = record.output_patterns.as_ref()?
                .iter()
                .position(|pattern| pattern == &item.pattern)?;
            if let Some(excerpt) = excerpts.get(&format!("{stream}:{index}")) {
                item.excerpt = Some(excerpt.excerpt.clone());
                item.log_offset = Some(excerpt.log_offset);
            }
        }
    }
    let count = matches
        .iter()
        .map(|item| item.count)
        .fold(0_u64, u64::saturating_add);
    let mut budget = record.output_wake_budget.clone().unwrap_or_else(|| new_budget(now));
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
    record.output_matches_pending = Some(Default::default());
    record.output_match_excerpts = Some(Default::default());
    let suppressed = if newly_paused {
        budget.total_suppressed_matches
    } else {
        budget.suppressed_matches
    };
    if !suppress || notice {
        budget.suppressed_matches = 0;
    }
    record.output_wake_budget = Some(budget);
    if suppress && !notice {
        return None;
    }
    let sequence = record.output_event_sequence.unwrap_or(0).checked_add(1)?;
    let id = format!("monitor-output:{}:{}:{sequence}", record.id, record.epoch.get());
    let ids = record.output_event_ids.get_or_insert_with(Vec::new);
    ids.push(id.clone());
    let keep = ids.len().saturating_sub(MONITOR_OUTPUT_RETAINED_EVENT_IDS);
    ids.drain(..keep);
    record.output_event_sequence = Some(sequence);
    record.output_event_count = Some(record.output_event_count.unwrap_or(0).saturating_add(1).min(MONITOR_OUTPUT_MAX_MATCH_COUNT as u64));
    record.output_wake_at = Some(now);
    record.output_matches_pending = Some(Default::default());
    let message = (suppressed > 0).then(|| if newly_paused {
        format!("Output-match wakes paused after sustained output. {suppressed} matches suppressed. The command and exit wake remain active. To resume output wakes, cancel this monitor and start a new monitor with more selective output_patterns.")
    } else {
        format!("{suppressed} output matches suppressed by the monitor wake budget.")
    });
    Some((id, OutputEvent { monitor_id: record.id.clone(), epoch: record.epoch.get(), sequence, matches, final_chunk, suppressed_matches: (suppressed > 0).then_some(suppressed), message, output_wakes_paused: newly_paused.then_some(true) }))
}
pub fn summary(record: &MonitorRecord) -> Option<OutputMatchSummary> {
    if record.output_patterns.as_ref().is_none_or(Vec::is_empty) { return None; }
    let budget = record.output_wake_budget.as_ref().filter(|budget| budget.total_suppressed_matches > 0);
    Some(OutputMatchSummary { events: record.output_event_count.unwrap_or(0), matches: counts(record,&record.output_matches_total), pending: counts(record,&record.output_matches_pending), suppressed_matches: budget.map(|b| b.total_suppressed_matches), output_wakes_paused: budget.map(|b| b.paused) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_event_python_bytes_match_legacy_encoding() {
        let payload = OutputEvent {
            monitor_id: "watch-雪".into(), epoch: 4, sequence: 7, final_chunk: true,
            matches: vec![OutputMatch { stream: "stdout".into(), pattern: "ready\n雪".into(), count: 2, excerpt: Some("é".into()), log_offset: Some(13) }],
            suppressed_matches: Some(3), message: Some("paused 雪".into()), output_wakes_paused: Some(true),
        };
        let legacy = studio_runtime_schema::json_compat::python_json(&serde_json::json!({
            "monitorId":"watch-雪", "epoch":4, "sequence":7, "final":true,
            "matches":[{"stream":"stdout", "pattern":"ready\n雪", "count":2, "excerpt":"é", "logOffset":13}],
            "suppressedMatches":3, "message":"paused 雪", "outputWakesPaused":true
        }));
        assert_eq!(payload.python_json(), legacy);
    }
}
