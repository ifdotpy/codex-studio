//! Bounded read-only history port. The account worker supplies reads after its
//! journal barrier; no recovery operation may load a thread or submit input.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

pub const READ_DEADLINE: Duration = Duration::from_secs(20);
const MAX_PAGES: usize = 128;
const MAX_ITEMS: usize = 4096;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionScope {
    pub account: String,
    pub connection: String,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryMethod {
    Thread,
    Turns,
    Items,
    BackgroundTerminals,
}
impl HistoryMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thread => "thread/read",
            Self::Turns => "thread/turns/list",
            Self::Items => "thread/items/list",
            Self::BackgroundTerminals => "thread/backgroundTerminals/list",
        }
    }
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HistoryError {
    #[error("recovery history method unavailable")]
    Unsupported,
    #[error("recovery history deadline expired")]
    Deadline,
    #[error("recovery history capacity exceeded")]
    Capacity,
    #[error("recovery history identity changed")]
    Identity,
    #[error("invalid recovery history at {0}")]
    Invalid(&'static str),
    #[error("recovery history unavailable")]
    Unavailable,
}
pub struct HistoryReceipt {
    pub scope: ConnectionScope,
    pub body: Value,
}
pub trait RecoveryReader {
    /// Must enforce the deadline and cancellation while waiting, and must not
    /// send the read on a different connection than `scope`.
    fn read(
        &mut self,
        scope: &ConnectionScope,
        method: HistoryMethod,
        params: Value,
        deadline: Instant,
    ) -> Result<HistoryReceipt, HistoryError>;
    /// Drain earlier account notifications before snapshot validation/commit.
    fn barrier(&mut self, scope: &ConnectionScope, deadline: Instant) -> Result<(), HistoryError>;
}

pub struct HistoryRead<'a> {
    reader: &'a mut dyn RecoveryReader,
    scope: ConnectionScope,
    deadline: Instant,
    calls: usize,
}
impl<'a> HistoryRead<'a> {
    pub fn new(
        reader: &'a mut dyn RecoveryReader,
        scope: ConnectionScope,
        deadline: Instant,
    ) -> Self {
        Self {
            reader,
            scope,
            deadline,
            calls: 0,
        }
    }
    pub fn scope(&self) -> &ConnectionScope {
        &self.scope
    }
    pub fn barrier(&mut self) -> Result<(), HistoryError> {
        if Instant::now() >= self.deadline {
            return Err(HistoryError::Deadline);
        }
        self.reader.barrier(&self.scope, self.deadline)?;
        if Instant::now() >= self.deadline {
            return Err(HistoryError::Deadline);
        }
        Ok(())
    }
    pub fn call(&mut self, method: HistoryMethod, params: Value) -> Result<Value, HistoryError> {
        if Instant::now() >= self.deadline {
            return Err(HistoryError::Deadline);
        }
        if self.calls >= MAX_PAGES {
            return Err(HistoryError::Capacity);
        }
        self.calls += 1;
        let response = self
            .reader
            .read(&self.scope, method, params, self.deadline)?;
        if Instant::now() >= self.deadline {
            return Err(HistoryError::Deadline);
        }
        if response.scope != self.scope {
            return Err(HistoryError::Identity);
        }
        if response.body.to_string().len() > MAX_RESPONSE_BYTES {
            return Err(HistoryError::Capacity);
        }
        Ok(response.body)
    }
    pub fn thread_state(&mut self, thread: &str) -> Result<Value, HistoryError> {
        let result = self.call(
            HistoryMethod::Thread,
            json!({"threadId":thread,"includeTurns":false}),
        )?;
        if result["thread"]["id"] != thread {
            return Err(HistoryError::Identity);
        }
        Ok(result["thread"]["status"].clone())
    }
    pub fn exact_turn(&mut self, thread: &str, turn: &str) -> Result<Option<Value>, HistoryError> {
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        loop {
            let mut params = json!({"threadId":thread,"limit":10,"sortDirection":"desc","itemsView":"notLoaded"});
            if let Some(c) = &cursor {
                params["cursor"] = Value::String(c.clone());
            }
            let page = self.turn_page(params.clone(), thread)?;
            if let Some(found) = data(&page)?.iter().find(|t| t["id"] == turn) {
                if found.get("itemsView").is_none_or(|v| v == "full") {
                    return checked_full(found, thread, turn).map(Some);
                }
                let mut items = Vec::new();
                let mut known = BTreeMap::new();
                let mut item_cursor: Option<String> = None;
                let mut item_seen = BTreeSet::new();
                loop {
                    let mut ip =
                        json!({"threadId":thread,"turnId":turn,"limit":100,"sortDirection":"asc"});
                    if let Some(c) = &item_cursor {
                        ip["cursor"] = Value::String(c.clone());
                    }
                    let item_page = match self.call(HistoryMethod::Items, ip) {
                        Err(HistoryError::Unsupported) => {
                            params["itemsView"] = json!("full");
                            let fallback = self.turn_page(params, thread)?;
                            let exact = data(&fallback)?
                                .iter()
                                .find(|t| t["id"] == turn)
                                .ok_or(HistoryError::Identity)?;
                            return checked_full(exact, thread, turn).map(Some);
                        }
                        result => result?,
                    };
                    for entry in data(&item_page)? {
                        let item = if entry.get("item").is_some() {
                            if entry["turnId"] != turn
                                || entry.get("threadId").is_some_and(|v| v != thread)
                            {
                                return Err(HistoryError::Identity);
                            }
                            &entry["item"]
                        } else {
                            entry
                        };
                        add_item(&mut items, &mut known, item, thread, turn)?;
                    }
                    item_cursor = next_cursor(&item_page, &mut item_seen)?;
                    if item_cursor.is_none() {
                        break;
                    }
                }
                let confirmation = self.turn_page(params, thread)?;
                let confirmed = data(&confirmation)?
                    .iter()
                    .find(|t| t["id"] == turn)
                    .ok_or(HistoryError::Identity)?;
                if confirmed["status"] != found["status"] || confirmed["error"] != found["error"] {
                    return Err(HistoryError::Identity);
                }
                let mut result = confirmed.clone();
                result["items"] = json!(items);
                result["itemsView"] = json!("full");
                return Ok(Some(result));
            }
            cursor = next_cursor(&page, &mut seen)?;
            if cursor.is_none() {
                return Ok(None);
            }
        }
    }
    /// Task proofs retain duplicate receipts: even identical duplicates are
    /// ambiguous to the task reconciler, unlike transcript item coalescing.
    pub fn task_items(&mut self, thread: &str, turn: &str) -> Result<Vec<Value>, HistoryError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        loop {
            let mut params =
                json!({"threadId":thread,"turnId":turn,"limit":1000,"sortDirection":"asc"});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let page = self.call(HistoryMethod::Items, params)?;
            for entry in data(&page)? {
                let item = &entry["item"];
                if entry["turnId"] != turn
                    || entry.get("threadId").is_some_and(|id| id != thread)
                    || !item["id"].is_string()
                    || item.get("threadId").is_some_and(|id| id != thread)
                    || item.get("turnId").is_some_and(|id| id != turn)
                {
                    return Err(HistoryError::Identity);
                }
                if items.len() >= MAX_ITEMS {
                    return Err(HistoryError::Capacity);
                }
                items.push(item.clone());
            }
            cursor = next_cursor(&page, &mut seen)?;
            if cursor.is_none() {
                return Ok(items);
            }
        }
    }
    /// Find the unique receipt for the exact client input across complete history.
    pub fn exact_start(
        &mut self,
        thread: &str,
        message: &str,
    ) -> Result<Option<Value>, HistoryError> {
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut found: Option<Value> = None;
        loop {
            let mut params =
                json!({"threadId":thread,"limit":10,"sortDirection":"desc","itemsView":"full"});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let page = self.turn_page(params, thread)?;
            for turn in data(&page)? {
                if matches!(
                    turn["startOutcome"].as_str(),
                    Some("preparing" | "not_applied")
                ) {
                    continue;
                }
                let matches = turn["clientUserMessageId"] == message
                    || turn["items"].as_array().is_some_and(|items| {
                        items.iter().any(|item| {
                            item["type"] == "userMessage"
                                && item["clientId"] == message
                                && !matches!(
                                    item["deliveryStatus"].as_str(),
                                    Some("preparing" | "not_applied")
                                )
                        })
                    });
                if matches {
                    if found.as_ref().is_some_and(|previous| previous != turn) {
                        return Err(HistoryError::Identity);
                    }
                    found = Some(turn.clone());
                }
            }
            cursor = next_cursor(&page, &mut seen)?;
            if cursor.is_none() {
                return Ok(found);
            }
        }
    }
    fn turn_page(&mut self, params: Value, thread: &str) -> Result<Value, HistoryError> {
        let page = self.call(HistoryMethod::Turns, params)?;
        let mut ids = BTreeSet::new();
        for turn in data(&page)? {
            let id = nonempty(&turn["id"])?;
            if !ids.insert(id)
                || turn.get("threadId").is_some_and(|v| v != thread)
                || !turn["items"].is_array()
                || turn
                    .get("itemsView")
                    .is_some_and(|v| !matches!(v.as_str(), Some("full" | "summary" | "notLoaded")))
            {
                return Err(HistoryError::Invalid("turn"));
            }
        }
        Ok(page)
    }
}
fn nonempty(v: &Value) -> Result<&str, HistoryError> {
    v.as_str()
        .filter(|s| !s.is_empty())
        .ok_or(HistoryError::Invalid("identity"))
}
fn data(v: &Value) -> Result<&Vec<Value>, HistoryError> {
    v["data"].as_array().ok_or(HistoryError::Invalid("data"))
}
fn next_cursor(page: &Value, seen: &mut BTreeSet<String>) -> Result<Option<String>, HistoryError> {
    let Some(value) = page.get("nextCursor").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let cursor = nonempty(value)?.to_owned();
    if !seen.insert(cursor.clone()) {
        return Err(HistoryError::Invalid("nextCursor"));
    }
    Ok(Some(cursor))
}
fn checked_full(turn: &Value, thread: &str, id: &str) -> Result<Value, HistoryError> {
    if turn.get("itemsView").is_some_and(|v| v != "full") {
        return Err(HistoryError::Invalid("itemsView"));
    }
    let mut items = Vec::new();
    let mut known = BTreeMap::new();
    for item in turn["items"]
        .as_array()
        .ok_or(HistoryError::Invalid("items"))?
    {
        add_item(&mut items, &mut known, item, thread, id)?;
    }
    let mut result = turn.clone();
    result["items"] = json!(items);
    Ok(result)
}
fn add_item(
    items: &mut Vec<Value>,
    known: &mut BTreeMap<String, Value>,
    item: &Value,
    thread: &str,
    turn: &str,
) -> Result<(), HistoryError> {
    let id = nonempty(&item["id"])?;
    nonempty(&item["type"])?;
    if item.get("threadId").is_some_and(|v| v != thread)
        || item.get("turnId").is_some_and(|v| v != turn)
    {
        return Err(HistoryError::Identity);
    }
    if let Some(previous) = known.get(id) {
        return if previous == item {
            Ok(())
        } else {
            Err(HistoryError::Identity)
        };
    }
    if items.len() >= MAX_ITEMS {
        return Err(HistoryError::Capacity);
    }
    known.insert(id.into(), item.clone());
    items.push(item.clone());
    Ok(())
}
