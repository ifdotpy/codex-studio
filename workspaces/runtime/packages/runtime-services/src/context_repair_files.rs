//! Safe Codex rollout lookup and rewrite effects for context repair.
//!
//! The caller owns provider reads and fork submission. This module works only
//! with explicit paths and captured records, outside any store transaction.
//! Its filesystem rules port `_rollout_segments`, `_prefix_records`,
//! `_projected_records`, and `sanitized_rollout` from
//! `apps/server/src/codex_context_repair.py:704-1005`; provider source lookup
//! follows `_repair` at `:2039-2052`.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::context_repair::{HistoryRewrite, RepairError, RepairEventSummary};

#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../tests/support/prepared_python.rs"]
mod prepared_python_test_support;

const MAX_HISTORY_LINE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ANCESTRY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_ANCESTRY_DEPTH: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 65_536;
const COPY_DIRECTORY: &str = ".studio-context-repairs";
const REQUIRED_FREE_SPACE_OVERHEAD: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryProvider {
    Codex,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistorySegment {
    pub path: PathBuf,
    pub thread_id: String,
    pub end_byte_offset: u64,
    pub end_ordinal_exclusive: Option<u64>,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryTerminal {
    pub turn_id: String,
    pub status: Option<String>,
    pub completed_at: Option<f64>,
    pub inherited_empty: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistoryRewriteReceipt {
    pub source_path: PathBuf,
    pub source_sha256: String,
    pub source_bytes: u64,
    pub source_file_identity: [u128; 4],
    pub import_thread_id: String,
    pub copy_path: Option<PathBuf>,
    pub copy_sha256: Option<String>,
    pub copy_bytes: Option<u64>,
    pub event_ids: Vec<String>,
    pub terminal_turn_id: Option<String>,
    pub terminal_status: Option<String>,
    pub terminal_completed_at: Option<f64>,
    pub inherited_empty: bool,
    pub saved_bytes: u64,
    pub ancestry: Vec<HistorySegment>,
}

#[derive(Debug, Error)]
pub enum HistoryFileError {
    #[error("provider history is unavailable or invalid")]
    Unavailable,
    #[error("provider history path is outside its home")]
    OutsideHome,
    #[error("provider history identity is ambiguous")]
    Ambiguous,
    #[error("provider history ancestry is invalid")]
    Ancestry,
    #[error("provider history changed during context repair")]
    Changed,
    #[error("provider history write failed")]
    Write,
    #[error("provider home lacks space for safe context repair copies")]
    InsufficientSpace,
    #[error(transparent)]
    Rewrite(#[from] RepairError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WritePoint {
    BeforeCopy,
    AfterCopy,
    BeforeReplace,
    AfterReplace,
}

#[derive(Clone, Debug)]
enum JsonOrder {
    Object(Vec<(String, JsonOrder)>),
    Array(Vec<JsonOrder>),
    Scalar,
}

impl<'de> Deserialize<'de> for JsonOrder {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OrderVisitor;
        impl<'de> Visitor<'de> for OrderVisitor {
            type Value = JsonOrder;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(entry) = access.next_entry::<String, JsonOrder>()? {
                    entries.push(entry);
                }
                Ok(JsonOrder::Object(entries))
            }

            fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(entry) = access.next_element::<JsonOrder>()? {
                    entries.push(entry);
                }
                Ok(JsonOrder::Array(entries))
            }

            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_string<E: serde::de::Error>(self, _: String) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(JsonOrder::Scalar)
            }
            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                JsonOrder::deserialize(deserializer)
            }
        }
        deserializer.deserialize_any(OrderVisitor)
    }
}

#[derive(Clone, Debug)]
struct HistoryRecord {
    value: Value,
    order: JsonOrder,
    original_line: Vec<u8>,
    projected: bool,
}

/// Find one Codex rollout by its registered thread ID.
pub fn locate_provider_history(
    provider: HistoryProvider,
    provider_home: &Path,
    thread_id: &str,
) -> Result<PathBuf, HistoryFileError> {
    match provider {
        HistoryProvider::Codex => {}
    }
    validate_thread_id(thread_id)?;
    let home = canonical_directory(provider_home)?;
    let mut matches = Vec::new();
    let mut visited = 0;
    for root in history_roots(&home) {
        scan_rollouts(&root, &mut visited, &mut |path| {
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(&format!("-{thread_id}.jsonl")))
            {
                matches.push(validate_source_path(&home, path)?);
            }
            Ok(())
        })?;
    }
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(HistoryFileError::Unavailable),
        _ => Err(HistoryFileError::Ambiguous),
    }
}

/// Validate and flatten ancestry, returning root-to-leaf byte-prefix proofs.
pub fn verify_history_ancestry(
    provider_home: &Path,
    source: &Path,
    thread_id: &str,
) -> Result<Vec<HistorySegment>, HistoryFileError> {
    validate_thread_id(thread_id)?;
    let home = canonical_directory(provider_home)?;
    let mut path = validate_source_path(&home, source)?;
    let mut thread = thread_id.to_owned();
    let mut end_byte_offset = file_len(&path)?;
    let mut ordinal_end = None;
    let mut total = 0u64;
    let mut seen = BTreeSet::new();
    let mut leaf_to_root = Vec::new();

    for _ in 0..MAX_ANCESTRY_DEPTH {
        if !seen.insert(thread.clone()) {
            return Err(HistoryFileError::Ancestry);
        }
        path = validate_source_path(&home, &path)?;
        total = total
            .checked_add(end_byte_offset)
            .filter(|value| *value <= MAX_ANCESTRY_BYTES)
            .ok_or(HistoryFileError::Ancestry)?;
        let (records, digest) = read_prefix(&path, end_byte_offset)?;
        let first = records.first().ok_or(HistoryFileError::Ancestry)?;
        if first["type"] != "session_meta" || first["payload"]["id"] != thread {
            return Err(HistoryFileError::Ancestry);
        }

        let mut previous_ordinal = None;
        let requires_ordinals =
            ordinal_end.is_some() || json_truthy(&first["payload"]["history_base"]);
        for record in &records {
            if requires_ordinals {
                let ordinal = record["ordinal"]
                    .as_u64()
                    .ok_or(HistoryFileError::Ancestry)?;
                if previous_ordinal.is_some_and(|previous| ordinal != previous + 1) {
                    return Err(HistoryFileError::Ancestry);
                }
                previous_ordinal = Some(ordinal);
            }
        }
        if let Some(cutoff) = ordinal_end
            && previous_ordinal.map(|last| last.checked_add(1)) != Some(Some(cutoff))
        {
            return Err(HistoryFileError::Ancestry);
        }

        let base = first["payload"]["history_base"].clone();
        if json_truthy(&base) {
            let base_thread = base["thread_id"]
                .as_str()
                .ok_or(HistoryFileError::Ancestry)?;
            validate_thread_id(base_thread)?;
            if uuid::Uuid::parse_str(base_thread)
                .map(|uuid| uuid.to_string() != base_thread)
                .unwrap_or(true)
            {
                return Err(HistoryFileError::Ancestry);
            }
            let base_end_ordinal = base["end_ordinal_exclusive"]
                .as_u64()
                .filter(|value| *value > 0)
                .ok_or(HistoryFileError::Ancestry)?;
            let base_end_byte = base["end_byte_offset"]
                .as_u64()
                .filter(|value| *value > 0)
                .ok_or(HistoryFileError::Ancestry)?;
            let keys = base
                .as_object()
                .ok_or(HistoryFileError::Ancestry)?
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            if keys != BTreeSet::from(["end_byte_offset", "end_ordinal_exclusive", "thread_id"])
                || first["ordinal"].as_u64() != Some(base_end_ordinal)
            {
                return Err(HistoryFileError::Ancestry);
            }
            leaf_to_root.push(HistorySegment {
                path: path.clone(),
                thread_id: thread.clone(),
                end_byte_offset,
                end_ordinal_exclusive: ordinal_end,
                sha256: digest,
            });
            let parent = find_ancestor(&home, base_thread, base_end_byte)?;
            thread = base_thread.to_owned();
            end_byte_offset = base_end_byte;
            ordinal_end = Some(base_end_ordinal);
            path = parent;
            continue;
        }

        if ordinal_end.is_some() && first["ordinal"].as_u64() != Some(0) {
            return Err(HistoryFileError::Ancestry);
        }
        leaf_to_root.push(HistorySegment {
            path,
            thread_id: thread,
            end_byte_offset,
            end_ordinal_exclusive: ordinal_end,
            sha256: digest,
        });
        leaf_to_root.reverse();
        return Ok(leaf_to_root);
    }
    Err(HistoryFileError::Ancestry)
}

/// Create a bounded private copy and rewrite only the captured rollout records.
/// The final JSONL appears by one same-directory rename after file fsync.
pub fn rewrite_provider_history(
    provider_home: &Path,
    source: &Path,
    events: Vec<Value>,
    thread_id: &str,
    terminal: &HistoryTerminal,
    summaries: &impl RepairEventSummary,
) -> Result<HistoryRewriteReceipt, HistoryFileError> {
    let import_thread_id = random_uuid()?;
    rewrite_with_hook(
        provider_home,
        source,
        events,
        thread_id,
        terminal,
        &import_thread_id,
        summaries,
        |_| Ok(()),
    )
}

#[allow(clippy::too_many_arguments)] // The hook is a private fault-injection seam.
fn rewrite_with_hook(
    provider_home: &Path,
    source: &Path,
    events: Vec<Value>,
    thread_id: &str,
    terminal: &HistoryTerminal,
    import_thread_id: &str,
    summaries: &impl RepairEventSummary,
    mut hook: impl FnMut(WritePoint) -> io::Result<()>,
) -> Result<HistoryRewriteReceipt, HistoryFileError> {
    let home = canonical_directory(provider_home)?;
    let source = validate_source_path(&home, source)?;
    let source_identity = file_identity(&source)?;
    let ancestry = verify_history_ancestry(&home, &source, thread_id)?;
    let leaf = ancestry.last().ok_or(HistoryFileError::Ancestry)?;
    let source_hash = leaf.sha256.clone();
    let source_bytes = leaf.end_byte_offset;
    validate_thread_id(&terminal.turn_id)?;
    let mut projected = projected_records(&ancestry)?;
    if terminal.inherited_empty {
        let local = projected_records(std::slice::from_ref(leaf))?;
        if ancestry.len() < 2
            || local.iter().skip(1).any(|record| {
                record.value["type"] != "event_msg"
                    || record.value["payload"]["type"] != "thread_settings_applied"
            })
        {
            return Err(HistoryFileError::Rewrite(RepairError::ReceiptHold));
        }
    }
    let (latest_started, latest_terminal) = terminal_state(&projected);
    if terminal.turn_id == latest_started.as_deref().unwrap_or_default()
        && latest_terminal.as_deref() != Some(&terminal.turn_id)
        && (terminal.status.as_deref() == Some("interrupted")
            || (terminal.status.as_deref() == Some("completed") && terminal.completed_at.is_some()))
    {
        let marker_kind = if terminal.status.as_deref() == Some("interrupted") {
            "turn_aborted"
        } else {
            "task_complete"
        };
        let mut payload = serde_json::Map::new();
        payload.insert("type".into(), json!(marker_kind));
        payload.insert("turn_id".into(), json!(terminal.turn_id));
        let mut payload_order = vec![
            ("type".to_owned(), JsonOrder::Scalar),
            ("turn_id".to_owned(), JsonOrder::Scalar),
        ];
        if let Some(completed_at) = terminal.completed_at {
            payload.insert("completed_at".into(), json!(completed_at));
            payload_order.push(("completed_at".to_owned(), JsonOrder::Scalar));
        }
        let mut marker = serde_json::Map::new();
        marker.insert("type".into(), json!("event_msg"));
        marker.insert("payload".into(), Value::Object(payload));
        projected.push(HistoryRecord {
            value: Value::Object(marker),
            order: JsonOrder::Object(vec![
                ("type".to_owned(), JsonOrder::Scalar),
                ("payload".to_owned(), JsonOrder::Object(payload_order)),
            ]),
            original_line: Vec::new(),
            projected: true,
        });
    }
    let source_events = events.clone();
    let core = HistoryRewrite::from_records(
        projected
            .iter()
            .map(|record| record.value.clone())
            .collect(),
        events,
        thread_id.to_owned(),
        terminal.turn_id.clone(),
    )?;
    validate_thread_id(import_thread_id)?;
    let (rewritten, event_ids) = core.rewrite(import_thread_id, summaries)?;
    let terminal_turn_id = rewritten.iter().rev().find_map(|record| {
        (record["type"] == "event_msg"
            && matches!(
                record["payload"]["type"].as_str(),
                Some("task_complete" | "turn_aborted")
            ))
        .then(|| record["payload"]["turn_id"].as_str().map(str::to_owned))
        .flatten()
    });
    if event_ids.is_empty() {
        return Ok(HistoryRewriteReceipt {
            source_path: source,
            source_sha256: source_hash,
            source_bytes,
            source_file_identity: identity_receipt(source_identity),
            import_thread_id: import_thread_id.to_owned(),
            copy_path: None,
            copy_sha256: None,
            copy_bytes: None,
            event_ids,
            terminal_turn_id,
            terminal_status: terminal.status.clone(),
            terminal_completed_at: terminal_completed_at(&rewritten, terminal),
            inherited_empty: terminal.inherited_empty,
            saved_bytes: 0,
            ancestry,
        });
    }

    let copy_budget = ancestry
        .iter()
        .try_fold(0u64, |total, segment| {
            total.checked_add(segment.end_byte_offset)
        })
        .and_then(|total| total.checked_mul(3))
        .and_then(|total| total.checked_add(REQUIRED_FREE_SPACE_OVERHEAD))
        .ok_or(HistoryFileError::Ancestry)?;
    let disk = rustix::fs::statvfs(&home).map_err(|_| HistoryFileError::Unavailable)?;
    let available = disk.f_bavail.saturating_mul(disk.f_frsize);
    if available < copy_budget {
        return Err(HistoryFileError::InsufficientSpace);
    }
    let source_name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(HistoryFileError::Unavailable)?;
    let source_name = source_name.replace(thread_id, import_thread_id);
    let copy_root = history_copy_root(&home)?;
    let repair_id = random_uuid()?;
    let repair_dir = copy_root.join(&repair_id);
    fs::create_dir(&repair_dir).map_err(|_| HistoryFileError::Write)?;
    if let Err(error) = set_directory_mode(&repair_dir) {
        drop(fs::remove_dir(&repair_dir));
        return Err(error);
    }
    let destination = repair_dir.join(source_name);
    let temp_path = repair_dir.join(format!(".{import_thread_id}.tmp"));
    let result = (|| {
        hook(WritePoint::BeforeCopy).map_err(|_| HistoryFileError::Write)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|_| HistoryFileError::Write)?;
        set_file_mode(&temp_path)?;
        let mut copy_digest = Sha256::new();
        let mut copy_bytes = 0u64;
        let mut turn = None;
        for (index, record) in rewritten.iter().enumerate() {
            let original = projected.get(index);
            if record["type"] == "turn_context" {
                turn = record["payload"]["turn_id"].as_str().map(str::to_owned);
            }
            let python_rewrites_record =
                has_python_rewrite(record, turn.as_deref(), &source_events, &event_ids);
            let line = if let Some(original) = original {
                if !original.projected && *record == original.value && !python_rewrites_record {
                    original.original_line.clone()
                } else {
                    let mut line = Vec::new();
                    serialize_in_order(record, &original.order, &mut line)?;
                    line.push(b'\n');
                    line
                }
            } else {
                let mut line = Vec::new();
                serialize_in_order(record, &order_for_value(record), &mut line)?;
                line.push(b'\n');
                line
            };
            output
                .write_all(&line)
                .map_err(|_| HistoryFileError::Write)?;
            copy_digest.update(&line);
            copy_bytes = copy_bytes
                .checked_add(line.len() as u64)
                .ok_or(HistoryFileError::Write)?;
        }
        output.flush().map_err(|_| HistoryFileError::Write)?;
        output.sync_all().map_err(|_| HistoryFileError::Write)?;
        drop(output);
        hook(WritePoint::AfterCopy).map_err(|_| HistoryFileError::Write)?;
        if validate_source_path(&home, &source)? != source
            || file_identity(&source)? != source_identity
        {
            return Err(HistoryFileError::Changed);
        }
        for segment in &ancestry {
            if prefix_hash(&segment.path, segment.end_byte_offset)? != segment.sha256 {
                return Err(HistoryFileError::Changed);
            }
        }
        hook(WritePoint::BeforeReplace).map_err(|_| HistoryFileError::Write)?;
        fs::rename(&temp_path, &destination).map_err(|_| HistoryFileError::Write)?;
        sync_directory(&repair_dir)?;
        hook(WritePoint::AfterReplace).map_err(|_| HistoryFileError::Write)?;
        let copy_hash = data_encoding::HEXLOWER.encode(&copy_digest.finalize());
        Ok((copy_bytes, copy_hash))
    })();
    match result {
        Ok((copy_bytes, copy_sha256)) => {
            let saved_bytes = saved_event_bytes(&source_events, &event_ids, summaries)?;
            Ok(HistoryRewriteReceipt {
                source_path: source,
                source_sha256: source_hash,
                source_bytes,
                source_file_identity: identity_receipt(source_identity),
                import_thread_id: import_thread_id.to_owned(),
                copy_path: Some(destination),
                copy_sha256: Some(copy_sha256),
                copy_bytes: Some(copy_bytes),
                event_ids,
                terminal_turn_id,
                terminal_status: terminal.status.clone(),
                terminal_completed_at: terminal_completed_at(&rewritten, terminal),
                inherited_empty: terminal.inherited_empty,
                saved_bytes,
                ancestry,
            })
        }
        Err(error) => {
            drop(fs::remove_file(&temp_path));
            drop(fs::remove_file(&destination));
            drop(fs::remove_dir(&repair_dir));
            Err(error)
        }
    }
}

/// Remove only a copy produced by this module, after its fork is known not to
/// have been accepted. The source rollout is never removed.
pub fn cleanup_rewrite_copy(
    provider_home: &Path,
    receipt: &HistoryRewriteReceipt,
) -> Result<(), HistoryFileError> {
    let Some(copy_path) = receipt.copy_path.as_deref() else {
        return Ok(());
    };
    let home = canonical_directory(provider_home)?;
    let path = validate_copy_path(&home, copy_path)?;
    fs::remove_file(&path).map_err(|_| HistoryFileError::Write)?;
    if let Some(parent) = path.parent() {
        drop(fs::remove_dir(parent));
    }
    Ok(())
}

fn projected_records(segments: &[HistorySegment]) -> Result<Vec<HistoryRecord>, HistoryFileError> {
    let Some(leaf) = segments.last() else {
        return Err(HistoryFileError::Ancestry);
    };
    if segments.len() == 1 {
        return read_prefix_detailed(&leaf.path, leaf.end_byte_offset).map(|(records, _)| records);
    }
    let mut records = Vec::new();
    let mut metadata = read_prefix_detailed(&leaf.path, leaf.end_byte_offset)?
        .0
        .into_iter()
        .next()
        .ok_or(HistoryFileError::Ancestry)?;
    metadata.value["payload"]
        .as_object_mut()
        .ok_or(HistoryFileError::Ancestry)?
        .remove("history_base");
    metadata.value["ordinal"] = json!(0);
    metadata.projected = true;
    records.push(metadata);
    let mut ordinal = 1u64;
    for segment in segments {
        let segment_records = read_prefix_detailed(&segment.path, segment.end_byte_offset)?.0;
        for (index, mut record) in segment_records.into_iter().enumerate() {
            if index == 0 {
                continue;
            }
            if record.value["type"] == "session_meta" {
                return Err(HistoryFileError::Ancestry);
            }
            record.value["ordinal"] = json!(ordinal);
            record.projected = true;
            ordinal = ordinal.checked_add(1).ok_or(HistoryFileError::Ancestry)?;
            records.push(record);
        }
    }
    Ok(records)
}

fn terminal_state(records: &[HistoryRecord]) -> (Option<String>, Option<String>) {
    let mut started = None;
    let mut terminal = None;
    for record in records {
        match record.value["type"].as_str() {
            Some("turn_context") => {
                started = record.value["payload"]["turn_id"]
                    .as_str()
                    .map(str::to_owned);
            }
            Some("event_msg") => match record.value["payload"]["type"].as_str() {
                Some("task_started") => {
                    started = record.value["payload"]["turn_id"]
                        .as_str()
                        .map(str::to_owned)
                }
                Some("task_complete" | "turn_aborted") => {
                    terminal = record.value["payload"]["turn_id"]
                        .as_str()
                        .map(str::to_owned)
                }
                _ => {}
            },
            _ => {}
        }
    }
    (started, terminal)
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn terminal_completed_at(records: &[Value], terminal: &HistoryTerminal) -> Option<f64> {
    records.iter().rev().find_map(|record| {
        if record["type"] != "event_msg"
            || !matches!(
                record["payload"]["type"].as_str(),
                Some("task_complete" | "turn_aborted")
            )
            || record["payload"]["turn_id"].as_str() != Some(terminal.turn_id.as_str())
        {
            return None;
        }
        record["payload"]["completed_at"].as_f64().or_else(|| {
            chrono::DateTime::parse_from_rfc3339(record["timestamp"].as_str()?)
                .ok()
                .map(|stamp| {
                    stamp.timestamp() as f64
                        + f64::from(stamp.timestamp_subsec_nanos()) / 1_000_000_000.0
                })
        })
    })
}

fn identity_receipt(identity: (u64, u64, u64, u128)) -> [u128; 4] {
    [
        identity.0.into(),
        identity.1.into(),
        identity.2.into(),
        identity.3,
    ]
}

fn order_for_value(value: &Value) -> JsonOrder {
    match value {
        Value::Object(values) => JsonOrder::Object(
            values
                .keys()
                .map(|key| (key.clone(), order_for_value(&values[key])))
                .collect(),
        ),
        Value::Array(values) => JsonOrder::Array(values.iter().map(order_for_value).collect()),
        _ => JsonOrder::Scalar,
    }
}

fn saved_event_bytes(
    events: &[Value],
    matched_ids: &[String],
    summaries: &impl RepairEventSummary,
) -> Result<u64, HistoryFileError> {
    let mut saved = 0u64;
    for event in events {
        let Some(id) = event["id"].as_str() else {
            continue;
        };
        if !matched_ids.iter().any(|matched| matched == id) {
            continue;
        }
        let kind = event["kind"]
            .as_str()
            .ok_or(RepairError::Invalid("rewrite.kind"))?;
        let text = event["text"]
            .as_str()
            .ok_or(RepairError::Invalid("rewrite.event.text"))?;
        let summary = summaries.summarize(event)?;
        let full = format!("[Orchestration event: {kind}]\n{text}").len();
        let compact = format!("[Orchestration event: {kind}]\n{summary}").len();
        saved = saved.saturating_add(full.saturating_sub(compact) as u64);
    }
    Ok(saved)
}

fn find_ancestor(home: &Path, thread_id: &str, end: u64) -> Result<PathBuf, HistoryFileError> {
    let mut visited = 0;
    let mut candidates = Vec::new();
    for root in history_roots(home) {
        scan_rollouts(&root, &mut visited, &mut |path| {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            if !name.contains(thread_id) {
                return Ok(());
            }
            let candidate = validate_source_path(home, path)?;
            if file_len(&candidate)? < end {
                return Ok(());
            }
            let (records, _) = read_prefix(&candidate, end)?;
            if records.first().is_some_and(|record| {
                record["type"] == "session_meta" && record["payload"]["id"] == thread_id
            }) {
                candidates.push(candidate);
            }
            Ok(())
        })?;
    }
    candidates.sort();
    candidates.dedup();
    if candidates.is_empty() {
        return Err(HistoryFileError::Ancestry);
    }
    let hashes = candidates
        .iter()
        .map(|candidate| prefix_hash(candidate, end))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if hashes.len() != 1 {
        return Err(HistoryFileError::Ancestry);
    }
    candidates
        .into_iter()
        .next()
        .ok_or(HistoryFileError::Ancestry)
}

fn read_prefix(path: &Path, end: u64) -> Result<(Vec<Value>, String), HistoryFileError> {
    read_prefix_detailed(path, end).map(|(records, hash)| {
        (
            records.into_iter().map(|record| record.value).collect(),
            hash,
        )
    })
}

fn read_prefix_detailed(
    path: &Path,
    end: u64,
) -> Result<(Vec<HistoryRecord>, String), HistoryFileError> {
    let file = File::open(path).map_err(|_| HistoryFileError::Unavailable)?;
    let mut reader = BufReader::new(file);
    let mut remaining = end;
    let mut records = Vec::new();
    let mut digest = Sha256::new();
    while remaining > 0 {
        let mut raw = Vec::new();
        loop {
            let available = reader
                .fill_buf()
                .map_err(|_| HistoryFileError::Unavailable)?;
            if available.is_empty() || remaining == 0 {
                return Err(HistoryFileError::Ancestry);
            }
            let bound = available
                .len()
                .min(remaining.min(MAX_HISTORY_LINE_BYTES.saturating_add(1)) as usize);
            let newline = available[..bound].iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(bound, |index| index + 1);
            raw.extend_from_slice(&available[..take]);
            reader.consume(take);
            remaining = remaining
                .checked_sub(take as u64)
                .ok_or(HistoryFileError::Ancestry)?;
            if raw.len() as u64 > MAX_HISTORY_LINE_BYTES {
                return Err(HistoryFileError::Ancestry);
            }
            if newline.is_some() {
                break;
            }
        }
        if raw.last() != Some(&b'\n') {
            return Err(HistoryFileError::Ancestry);
        }
        digest.update(&raw);
        let value =
            serde_json::from_slice::<Value>(&raw).map_err(|_| HistoryFileError::Ancestry)?;
        let order =
            serde_json::from_slice::<JsonOrder>(&raw).map_err(|_| HistoryFileError::Ancestry)?;
        records.push(HistoryRecord {
            value,
            order,
            original_line: raw,
            projected: false,
        });
    }
    Ok((records, data_encoding::HEXLOWER.encode(&digest.finalize())))
}

fn serialize_in_order(
    value: &Value,
    order: &JsonOrder,
    output: &mut Vec<u8>,
) -> Result<(), HistoryFileError> {
    match (value, order) {
        (Value::Object(values), JsonOrder::Object(entries)) => {
            output.push(b'{');
            let mut first = true;
            let mut emitted = BTreeSet::new();
            for (key, child_order) in entries {
                let Some(child) = values.get(key) else {
                    continue;
                };
                if !first {
                    output.push(b',');
                }
                first = false;
                emitted.insert(key.as_str());
                serde_json::to_writer(&mut *output, key).map_err(|_| HistoryFileError::Write)?;
                output.push(b':');
                serialize_in_order(child, child_order, output)?;
            }
            for (key, child) in values {
                if emitted.contains(key.as_str()) {
                    continue;
                }
                if !first {
                    output.push(b',');
                }
                first = false;
                serde_json::to_writer(&mut *output, key).map_err(|_| HistoryFileError::Write)?;
                output.push(b':');
                serde_json::to_writer(&mut *output, child).map_err(|_| HistoryFileError::Write)?;
            }
            output.push(b'}');
        }
        (Value::Array(values), JsonOrder::Array(entries)) => {
            output.push(b'[');
            for (index, child) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                if let Some(child_order) = entries.get(index) {
                    serialize_in_order(child, child_order, output)?;
                } else {
                    serde_json::to_writer(&mut *output, child)
                        .map_err(|_| HistoryFileError::Write)?;
                }
            }
            output.push(b']');
        }
        _ => serde_json::to_writer(output, value).map_err(|_| HistoryFileError::Write)?,
    }
    Ok(())
}

fn has_python_rewrite(
    record: &Value,
    turn: Option<&str>,
    events: &[Value],
    event_ids: &[String],
) -> bool {
    let items = match record["type"].as_str() {
        Some("response_item") => vec![&record["payload"]],
        Some("compacted") => record["payload"]["replacement_history"]
            .as_array()
            .map(|items| items.iter().collect())
            .unwrap_or_default(),
        _ => return false,
    };
    items.into_iter().any(|item| {
        if item["type"] != "message" || item["role"] != "user" {
            return false;
        }
        let item_turn = item["internal_chat_message_metadata_passthrough"]["turn_id"]
            .as_str()
            .or(turn);
        let Some(item_turn) = item_turn else {
            return false;
        };
        item["content"].as_array().is_some_and(|contents| {
            contents.iter().any(|content| {
                if content["type"] != "input_text" {
                    return false;
                }
                let Some(text) = content["text"].as_str() else {
                    return false;
                };
                events.iter().any(|event| {
                    let Some(id) = event["id"].as_str() else {
                        return false;
                    };
                    if !event_ids.iter().any(|matched| matched == id)
                        || event["turn_id"].as_str() != Some(item_turn)
                    {
                        return false;
                    }
                    let Some(kind) = event["kind"].as_str() else {
                        return false;
                    };
                    let Some(message) = event["text"].as_str() else {
                        return false;
                    };
                    text.starts_with(&format!("[Orchestration event: {kind}]\n{message}"))
                })
            })
        })
    })
}

fn prefix_hash(path: &Path, end: u64) -> Result<String, HistoryFileError> {
    read_prefix(path, end).map(|(_, hash)| hash)
}

fn scan_rollouts(
    root: &Path,
    visited: &mut usize,
    visit: &mut impl FnMut(&Path) -> Result<(), HistoryFileError>,
) -> Result<(), HistoryFileError> {
    let metadata = match fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(HistoryFileError::Unavailable),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HistoryFileError::OutsideHome);
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).map_err(|_| HistoryFileError::Unavailable)? {
            *visited += 1;
            if *visited > MAX_DIRECTORY_ENTRIES {
                return Err(HistoryFileError::Unavailable);
            }
            let entry = entry.map_err(|_| HistoryFileError::Unavailable)?;
            let kind = entry
                .file_type()
                .map_err(|_| HistoryFileError::Unavailable)?;
            if kind.is_symlink() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".jsonl"))
                {
                    visit(&entry.path())?;
                }
                continue;
            }
            if kind.is_dir() {
                directories.push(entry.path());
            } else if kind.is_file()
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                visit(&entry.path())?;
            }
        }
    }
    Ok(())
}

fn history_roots(home: &Path) -> [PathBuf; 2] {
    [home.join("sessions"), home.join("archived_sessions")]
}

fn canonical_directory(path: &Path) -> Result<PathBuf, HistoryFileError> {
    let home = fs::canonicalize(path).map_err(|_| HistoryFileError::Unavailable)?;
    if !home.is_dir() {
        return Err(HistoryFileError::Unavailable);
    }
    Ok(home)
}

fn validate_source_path(home: &Path, path: &Path) -> Result<PathBuf, HistoryFileError> {
    let canonical = fs::canonicalize(path).map_err(|_| HistoryFileError::Unavailable)?;
    if !canonical.starts_with(home)
        || canonical.extension().and_then(|ext| ext.to_str()) != Some("jsonl")
    {
        return Err(HistoryFileError::OutsideHome);
    }
    let relative = canonical
        .strip_prefix(home)
        .map_err(|_| HistoryFileError::OutsideHome)?;
    let mut components = relative.components();
    let first = components.next().ok_or(HistoryFileError::OutsideHome)?;
    if !matches!(first, Component::Normal(name) if name == "sessions" || name == "archived_sessions")
    {
        return Err(HistoryFileError::OutsideHome);
    }
    if components.next().is_none() {
        return Err(HistoryFileError::OutsideHome);
    }
    Ok(canonical)
}

fn validate_copy_path(home: &Path, path: &Path) -> Result<PathBuf, HistoryFileError> {
    let canonical_parent = fs::canonicalize(path.parent().ok_or(HistoryFileError::OutsideHome)?)
        .map_err(|_| HistoryFileError::Unavailable)?;
    let copy_root = fs::canonicalize(home.join("sessions").join(COPY_DIRECTORY))
        .map_err(|_| HistoryFileError::Unavailable)?;
    if !canonical_parent.starts_with(&copy_root) {
        return Err(HistoryFileError::OutsideHome);
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| HistoryFileError::Unavailable)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(HistoryFileError::OutsideHome);
    }
    let canonical = fs::canonicalize(path).map_err(|_| HistoryFileError::Unavailable)?;
    if !canonical.starts_with(&copy_root) {
        return Err(HistoryFileError::OutsideHome);
    }
    Ok(canonical)
}

fn history_copy_root(home: &Path) -> Result<PathBuf, HistoryFileError> {
    let sessions = home.join("sessions");
    let sessions_meta = fs::symlink_metadata(&sessions).map_err(|_| HistoryFileError::Write)?;
    if sessions_meta.file_type().is_symlink() || !sessions_meta.is_dir() {
        return Err(HistoryFileError::OutsideHome);
    }
    let sessions = fs::canonicalize(sessions).map_err(|_| HistoryFileError::Write)?;
    if !sessions.starts_with(home) {
        return Err(HistoryFileError::OutsideHome);
    }
    let root = sessions.join(COPY_DIRECTORY);
    match fs::create_dir(&root) {
        Ok(()) => {
            if let Err(error) = set_directory_mode(&root) {
                drop(fs::remove_dir(&root));
                return Err(error);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&root).map_err(|_| HistoryFileError::Write)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(HistoryFileError::OutsideHome);
            }
            set_directory_mode(&root)?;
        }
        Err(_) => return Err(HistoryFileError::Write),
    }
    let canonical = fs::canonicalize(root).map_err(|_| HistoryFileError::Write)?;
    if !canonical.starts_with(home) {
        return Err(HistoryFileError::OutsideHome);
    }
    Ok(canonical)
}

fn validate_thread_id(thread_id: &str) -> Result<(), HistoryFileError> {
    if thread_id.is_empty()
        || thread_id.len() > 128
        || !thread_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(HistoryFileError::Unavailable);
    }
    Ok(())
}

fn file_len(path: &Path) -> Result<u64, HistoryFileError> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|_| HistoryFileError::Unavailable)
}

fn file_identity(path: &Path) -> Result<(u64, u64, u64, u128), HistoryFileError> {
    let metadata = fs::metadata(path).map_err(|_| HistoryFileError::Unavailable)?;
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let identity = (0, 0);
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    Ok((identity.0, identity.1, metadata.len(), modified))
}

fn random_uuid() -> Result<String, HistoryFileError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| HistoryFileError::Write)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(uuid::Uuid::from_bytes(bytes).to_string())
}

fn set_directory_mode(path: &Path) -> Result<(), HistoryFileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| HistoryFileError::Write)?;
    }
    Ok(())
}

fn set_file_mode(path: &Path) -> Result<(), HistoryFileError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| HistoryFileError::Write)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), HistoryFileError> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| HistoryFileError::Write)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use serde_json::{Value, json};

    use super::*;

    const THREAD: &str = "11111111-1111-4111-8111-111111111111";
    const IMPORT: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const TERMINAL: &str = "turn-1";

    fn terminal() -> HistoryTerminal {
        HistoryTerminal {
            turn_id: TERMINAL.into(),
            status: Some("completed".into()),
            completed_at: Some(10.0),
            inherited_empty: false,
        }
    }

    struct IdentitySummary;
    impl RepairEventSummary for IdentitySummary {
        fn summarize(&self, event: &Value) -> Result<String, RepairError> {
            Ok(event["text"]
                .as_str()
                .ok_or(RepairError::Invalid("rewrite.event.text"))?
                .to_owned())
        }
    }

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Vec<Value>) {
        let root = tempfile::tempdir().expect("tmpfs fixture root");
        let home = root.path().join("provider");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).expect("provider fixture sessions");
        let source = sessions.join(format!("rollout-2026-10-14-{THREAD}.jsonl"));
        let text = "[Orchestration event: work_review]\nreview event";
        let records = [
            format!(r#"{{"type":"session_meta","payload":{{"id":"{THREAD}"}},"ordinal":0}}"#),
            r#"{"type":"turn_context","payload":{"turn_id":"turn-1"},"ordinal":1}"#.to_owned(),
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-1"},"ordinal":2}"#.to_owned(),
            format!(r#"{{"type":"response_item","payload":{{"type":"message","role":"user","id":"item-1","internal_chat_message_metadata_passthrough":{{"turn_id":"turn-1"}},"content":[{{"type":"input_text","text":{}}}]}},"ordinal":3}}"#, serde_json::to_string(text).expect("JSON text")),
            r#"{"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-1","completed_at":10.0},"ordinal":4}"#.to_owned(),
        ];
        let source_bytes = records.join("\n") + "\n";
        fs::write(&source, source_bytes.as_bytes()).expect("fixture JSONL");
        let events = vec![json!({
            "id": "event-1", "turn_id": TERMINAL, "kind": "work_review", "text": "review event"
        })];
        (root, home, source, events)
    }

    fn run_python_oracle(
        root: &Path,
        home: &Path,
        source: &Path,
        events: &[Value],
    ) -> (Value, Vec<u8>) {
        let server_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python_test_support::prepared_python(&server_source);
        let oracle =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/context_repair_files/oracle.py");
        let events_path = root.join("events.json");
        let expected_copy = root.join("python-copy.jsonl");
        fs::write(
            &events_path,
            serde_json::to_vec(events).expect("events JSON"),
        )
        .expect("events fixture");
        let mut command = Command::new(python);
        command
            .arg(&oracle)
            .arg(home)
            .arg(source)
            .arg(THREAD)
            .arg(IMPORT)
            .arg(events_path)
            .arg(TERMINAL)
            .arg(&expected_copy)
            .env_clear()
            .env("HOME", root.join("runner-home"))
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", root)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        let output = prepared_python_test_support::output_with_timeout(
            &mut command,
            &oracle,
            std::time::Duration::from_secs(60),
        )
        .expect("bounded Python filesystem oracle");
        assert!(
            output.status.success(),
            "Python oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
        (
            result["report"].clone(),
            fs::read(expected_copy).expect("Python copy"),
        )
    }

    fn run_python_refusal(root: &Path, home: &Path, source: &Path, thread_id: &str) {
        let server_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let python = prepared_python_test_support::prepared_python(&server_source);
        let oracle =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/context_repair_files/oracle.py");
        let events_path = root.join("refusal-events.json");
        let output_path = root.join("refusal-copy.jsonl");
        fs::write(&events_path, b"[]").expect("empty refusal event fixture");
        let mut command = Command::new(python);
        command
            .arg(&oracle)
            .arg(home)
            .arg(source)
            .arg(thread_id)
            .arg(IMPORT)
            .arg(events_path)
            .arg(TERMINAL)
            .arg(output_path)
            .env_clear()
            .env("HOME", root.join("runner-home"))
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", root)
            .env("PYTHONDONTWRITEBYTECODE", "1");
        let output = prepared_python_test_support::output_with_timeout(
            &mut command,
            &oracle,
            std::time::Duration::from_secs(60),
        )
        .expect("bounded Python refusal oracle");
        assert!(
            output.status.success(),
            "Python refusal oracle failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
        assert_eq!(result["refused"], true, "Python must refuse {source:?}");
    }

    #[test]
    fn history_lookup_and_rewrite_match_python_bytes() {
        let (root, home, source, events) = fixture();
        assert_eq!(
            locate_provider_history(HistoryProvider::Codex, &home, THREAD)
                .expect("unique provider history"),
            source
        );
        let (expected_report, expected_copy) =
            run_python_oracle(root.path(), &home, &source, &events);
        let receipt = rewrite_with_hook(
            &home,
            &source,
            events,
            THREAD,
            &terminal(),
            IMPORT,
            &IdentitySummary,
            |_| Ok(()),
        )
        .expect("rewrite fixture history");
        assert_eq!(receipt.import_thread_id, IMPORT);
        assert_eq!(json!(receipt.event_ids), expected_report["eventIds"]);
        assert_eq!(receipt.source_sha256, expected_report["sourceSha256"]);
        assert_eq!(receipt.source_bytes, expected_report["sourceBytes"]);
        assert_eq!(
            fs::read(receipt.copy_path.as_ref().expect("copy path")).expect("Rust copy"),
            expected_copy,
            "rewritten JSONL must match Python bytes"
        );
        cleanup_rewrite_copy(&home, &receipt).expect("cleanup private copy");
        assert!(source.exists(), "cleanup must retain provider source");
    }

    #[test]
    fn inherited_history_matches_python_projection_bytes() {
        let root = tempfile::tempdir().expect("tmpfs fixture root");
        let home = root.path().join("provider");
        let sessions = home.join("sessions");
        fs::create_dir_all(&sessions).expect("provider fixture sessions");
        let parent_id = "22222222-2222-4222-8222-222222222222";
        let parent_path = sessions.join(format!("rollout-parent-{parent_id}.jsonl"));
        let parent = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"22222222-2222-4222-8222-222222222222\"},\"ordinal\":0}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"thread_settings_applied\"},\"ordinal\":1}\n",
            "{\"type\":\"turn_context\",\"payload\":{\"turn_id\":\"turn-parent\"},\"ordinal\":2}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\",\"turn_id\":\"turn-parent\"},\"ordinal\":3}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"turn_id\":\"turn-parent\",\"completed_at\":9.0},\"ordinal\":4}\n",
        );
        fs::write(&parent_path, parent).expect("parent history");
        let child_path = sessions.join(format!("rollout-child-{THREAD}.jsonl"));
        let text = "[Orchestration event: work_review]\nreview event";
        let child = format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{THREAD}\",\"history_base\":{{\"thread_id\":\"{parent_id}\",\"end_ordinal_exclusive\":5,\"end_byte_offset\":{}}}}},\"ordinal\":5}}\n",
            parent.len()
        ) + &format!(
            "{{\"type\":\"turn_context\",\"payload\":{{\"turn_id\":\"{TERMINAL}\"}},\"ordinal\":6}}\n"
        ) + &format!(
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"{TERMINAL}\"}},\"ordinal\":7}}\n"
        ) + &format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"id\":\"item-child\",\"internal_chat_message_metadata_passthrough\":{{\"turn_id\":\"{TERMINAL}\"}},\"content\":[{{\"type\":\"input_text\",\"text\":{}}}]}},\"ordinal\":8}}\n",
            serde_json::to_string(text).expect("JSON text")
        ) + &format!(
            "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"{TERMINAL}\",\"completed_at\":10.0}},\"ordinal\":9}}\n"
        );
        fs::write(&child_path, child).expect("child history");
        let events = vec![json!({
            "id": "event-1", "turn_id": TERMINAL, "kind": "work_review", "text": "review event"
        })];
        let (expected_report, expected_copy) =
            run_python_oracle(root.path(), &home, &child_path, &events);
        let receipt = rewrite_with_hook(
            &home,
            &child_path,
            events,
            THREAD,
            &terminal(),
            IMPORT,
            &IdentitySummary,
            |_| Ok(()),
        )
        .expect("rewrite inherited history");
        assert_eq!(receipt.ancestry.len(), 2);
        assert_eq!(receipt.source_sha256, expected_report["sourceSha256"]);
        assert_eq!(
            fs::read(receipt.copy_path.as_ref().expect("private copy")).expect("copy bytes"),
            expected_copy,
            "inherited flattening and JSONL bytes must match Python"
        );
    }

    #[test]
    fn history_path_and_ancestry_refusals_fail_closed() {
        let (root, home, source, _) = fixture();
        assert!(locate_provider_history(HistoryProvider::Codex, &home, "../escape").is_err());
        assert!(locate_provider_history(HistoryProvider::Codex, &home, "missing").is_err());
        let outside = root.path().join("outside.jsonl");
        fs::write(&outside, b"{}\n").expect("outside fixture file");
        assert!(matches!(
            verify_history_ancestry(&home, &outside, THREAD),
            Err(HistoryFileError::OutsideHome)
        ));
        run_python_refusal(root.path(), &home, &outside, THREAD);
        let missing = home.join("sessions/rollout-missing.jsonl");
        run_python_refusal(root.path(), &home, &missing, THREAD);
        let mut corrupt = fs::read(&source).expect("source bytes");
        corrupt.truncate(corrupt.len() - 2);
        fs::write(&source, corrupt).expect("incomplete final record");
        assert!(matches!(
            verify_history_ancestry(&home, &source, THREAD),
            Err(HistoryFileError::Ancestry)
        ));
        run_python_refusal(root.path(), &home, &source, THREAD);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused() {
        use std::os::unix::fs::symlink;

        let (root, home, _, _) = fixture();
        let outside = root.path().join("outside");
        fs::create_dir(&outside).expect("outside directory");
        let outside_source = outside.join(format!("rollout-{THREAD}.jsonl"));
        fs::write(&outside_source, b"{}\n").expect("outside history");
        symlink(
            &outside_source,
            home.join("sessions")
                .join(format!("rollout-{THREAD}.jsonl")),
        )
        .expect("history symlink");
        assert!(matches!(
            locate_provider_history(HistoryProvider::Codex, &home, THREAD),
            Err(HistoryFileError::OutsideHome)
        ));
        run_python_refusal(
            root.path(),
            &home,
            &home
                .join("sessions")
                .join(format!("rollout-{THREAD}.jsonl")),
            THREAD,
        );
    }

    #[test]
    fn every_copy_and_atomic_publish_failure_cleans_its_attempt() {
        for fail_at in [
            WritePoint::BeforeCopy,
            WritePoint::AfterCopy,
            WritePoint::BeforeReplace,
            WritePoint::AfterReplace,
        ] {
            let (_root, home, source, events) = fixture();
            let source_before = fs::read(&source).expect("source before failure");
            let result = rewrite_with_hook(
                &home,
                &source,
                events,
                THREAD,
                &terminal(),
                IMPORT,
                &IdentitySummary,
                |point| {
                    if point == fail_at {
                        Err(io::Error::other("injected copy failure"))
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(result.is_err(), "failure point {fail_at:?} must fail");
            assert_eq!(fs::read(&source).expect("source retained"), source_before);
            let copy_root = home.join("sessions").join(COPY_DIRECTORY);
            if copy_root.exists() {
                assert_eq!(
                    fs::read_dir(copy_root).expect("copy root").count(),
                    0,
                    "failure point {fail_at:?} leaked an attempt"
                );
            }
        }
    }
}
