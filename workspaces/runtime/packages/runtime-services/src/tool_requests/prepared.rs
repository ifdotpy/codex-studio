//! Callback payload capability: SQL identities, verified bytes, durable hashes.
//! Construction performs I/O; using the capability never touches the filesystem.
use super::*;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Selection {
    List,
    Receipt(String),
    Output(Vec<String>),
}

#[derive(Debug, PartialEq)]
pub(crate) struct Rows {
    selection: Selection,
    requests: Vec<Option<Value>>,
    results: BTreeMap<String, Option<Value>>,
}
impl Rows {
    pub(crate) fn read(
        tx: &Transaction<'_>,
        actor: &str,
        selection: Selection,
    ) -> Result<Self, ToolRequestError> {
        let requests = match &selection {
            Selection::List => list_records(tx, actor)?.into_iter().map(Some).collect(),
            Selection::Receipt(key) => vec![resolve(tx, actor, key)?],
            Selection::Output(keys) => keys
                .iter()
                .map(|key| resolve(tx, actor, key))
                .collect::<Result<_, _>>()?,
        };
        let mut results = BTreeMap::new();
        for request in requests
            .iter()
            .flatten()
            .filter(|row| text(row, "stage") != "ambiguous")
        {
            if matches!(selection, Selection::List) && terminal(request) {
                continue;
            }
            let id = text(request, "id");
            results.insert(id.to_owned(), records::get(tx, "runtime_tool_results", id)?);
        }
        Ok(Self {
            selection,
            requests,
            results,
        })
    }

    pub(crate) fn prepare(
        self,
        root: &std::path::Path,
        now: f64,
    ) -> Result<Payloads, ToolRequestError> {
        studio_runtime_store::debug_assert_outside_sql();
        let specific = !matches!(self.selection, Selection::List);
        let output = matches!(self.selection, Selection::Output(_));
        let mut resolved = Vec::new();
        let mut published = BTreeSet::new();
        for raw in self
            .requests
            .iter()
            .flatten()
            .filter(|row| text(row, "stage") != "ambiguous")
        {
            // Match refresh's list projection: terminal receipts normally need no payload.
            if !specific && terminal(raw) && !terminal_payload_needed(raw) {
                continue;
            }
            let mut record = prepare_record(root, raw, &mut resolved)?;
            if let Some(saved) = self.results.get(text(raw, "id")).and_then(Option::as_ref)
                && (!terminal(raw) || output)
            {
                let mut result = prepare_record(root, saved, &mut resolved)?;
                if specific {
                    expire(&mut result, now);
                }
                record["result"] = result;
            }
            if !terminal(raw) {
                // Only result is externalized by the request owner. Lifecycle
                // and timing fields cannot change these content hashes.
                let decision = crate::payload_blobs::decide_externalize("tool_requests", &record)?;
                crate::payload_blobs::write_blobs(root, &decision.writes)?;
                published.extend(decision.writes.into_iter().map(|write| write.sha256));
            }
        }
        Ok(Payloads {
            rows: self,
            resolved,
            published,
        })
    }
}

pub(crate) struct Payloads {
    rows: Rows,
    resolved: Vec<(Value, Value)>,
    published: BTreeSet<String>,
}
impl Payloads {
    pub(crate) fn revalidate(
        &self,
        tx: &Transaction<'_>,
        actor: &str,
        selection: Selection,
    ) -> Result<(), crate::provider_callbacks::CallbackError> {
        if Rows::read(tx, actor, selection)? != self.rows {
            return Err(crate::provider_callbacks::CallbackError::Stale);
        }
        Ok(())
    }
    pub(crate) fn execute(
        &self,
        tx: &Transaction<'_>,
        actor: &str,
        action: Action<'_>,
        now: f64,
    ) -> Result<Value, ToolRequestError> {
        let mut record = execute_with_payload_recovery(tx, actor, action, now, Some(self))?;
        if let Some(result) = record.get_mut("result") {
            expire(result, now);
        }
        Ok(record)
    }
    pub(crate) fn resolve(&self, raw: &Value) -> Result<Value, ToolRequestError> {
        Ok(crate::payload_blobs::resolve_record_with(
            raw,
            |reference| {
                self.resolved
                    .iter()
                    .find(|(before, _)| before == reference)
                    .map(|(_, after)| after.clone())
                    .ok_or(crate::payload_blobs::PayloadBlobError::InvalidReference)
            },
        )?)
    }
}
impl PayloadAccess for Payloads {
    fn resolve(&self, raw: &Value) -> Result<Value, ToolRequestError> {
        self.resolve(raw)
    }
    fn publish(&self, writes: &[crate::payload_blobs::BlobWrite]) -> Result<(), ToolRequestError> {
        if writes
            .iter()
            .all(|write| self.published.contains(&write.sha256))
        {
            Ok(())
        } else {
            Err(ToolRequestError::InvalidRequest(
                "Tool payload changed before settlement",
            ))
        }
    }
}

fn prepare_record(
    root: &std::path::Path,
    raw: &Value,
    resolved: &mut Vec<(Value, Value)>,
) -> Result<Value, ToolRequestError> {
    Ok(crate::payload_blobs::resolve_record_with(
        raw,
        |reference| {
            if let Some((_, value)) = resolved.iter().find(|(before, _)| before == reference) {
                return Ok(value.clone());
            }
            let value: Value =
                serde_json::from_slice(&crate::payload_blobs::load_blob(root, reference)?)?;
            resolved.push((reference.clone(), value.clone()));
            Ok(value)
        },
    )?)
}
