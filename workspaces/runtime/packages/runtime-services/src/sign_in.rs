//! Sign-in links that an agent gives to the person (docs/vm-browser-bridge.md, section 7).
//!
//! The Rust port of `codex_browser_bridge.submit`, `sign_in` and `message_links`. Two paths
//! reach the sign-in card of section 6.2:
//!
//! - the managed tool `orchestration_sign_in` (`tool`), and
//! - a completed agent message with a sign-in link (`note_message`, `run_messages`). The rule
//!   is `browser_policy::sign_in_links`; the message text does not change.
//!
//! Both send the link to the browser bridge of the chat (`browser.submit` of the host bridge or
//! the VM guest) and write its card at once with `browser_cards::apply`, the one owner of the
//! card. The bridge gives one request to one link of a chat, so a retry, the tool call, the
//! message and the command's own `BROWSER` call make one card. The owner of the tool definition
//! and of its result texts is `vm-guest/sign_in_contract.json`; this module reads the texts,
//! and a test checks the bundled catalog against the definition.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;
use tokio::sync::{mpsc, watch};

use crate::browser_cards::{self, CardSource};

/// The managed tool name (`SIGN_IN_TOOL`).
pub const TOOL: &str = "orchestration_sign_in";
const CONTRACT: &str = include_str!("../../../apps/vm-guest/sign_in_contract.json");
/// Completed messages with sign-in links that wait for a card; more are dropped and logged.
const MESSAGE_QUEUE: usize = 64;
/// The waits for a noted message to commit, in milliseconds: the note comes from inside the
/// message transaction.
pub const COMMIT_WAITS_MS: [u64; 5] = [50, 200, 500, 1000, 2000];

fn contract() -> &'static Value {
    static VALUE: OnceLock<Value> = OnceLock::new();
    VALUE.get_or_init(|| serde_json::from_str(CONTRACT).unwrap_or(Value::Null))
}

/// The longest wait for the bridge to queue a link (contract `submitSeconds`).
fn submit_timeout() -> Duration {
    Duration::from_secs_f64(contract()["submitSeconds"].as_f64().unwrap_or(20.0))
}

/// The oldest message whose links get cards, in seconds (contract `messageAgeSeconds`).
fn message_age_seconds() -> f64 {
    contract()["messageAgeSeconds"].as_f64().unwrap_or(600.0)
}

fn text(state: &str) -> &'static str {
    let results = &contract()["results"];
    results
        .get(state)
        .or_else(|| results.get("closed"))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// The browser bridges that take an agent's links: the host bridge of native chats and, on a
/// Mac, the VM guest. They are the card sources of `browser_cards`.
#[derive(Clone, Default)]
pub struct SignInSources {
    sources: Vec<CardSource>,
}

impl SignInSources {
    pub fn new(sources: Vec<CardSource>) -> Self {
        Self { sources }
    }

    fn source(&self, vm: bool) -> Option<&CardSource> {
        self.sources
            .iter()
            .find(|source| (source.source == "vm") == vm)
    }
}

fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or_default()
}

/// Queue one link for a chat and write its card at once. Returns the bridge item and why the
/// link got no card, or a refusal. Every error is safe to retry: the same link of a chat
/// returns its earlier request.
pub async fn submit(
    store: &RuntimeStore,
    sources: &SignInSources,
    agent: &Value,
    url: &str,
    request_id: &str,
) -> Result<(Value, Option<String>), String> {
    let vm = agent.get("executionMode").and_then(Value::as_str) == Some("vm");
    let source = sources.source(vm).ok_or_else(|| {
        if vm {
            "Sign-in links of VM chats open only on the computer that runs the VM".to_owned()
        } else {
            "The browser bridge of native chats is not running".to_owned()
        }
    })?;
    let agent_id = agent
        .get("id")
        .and_then(Value::as_str)
        .ok_or("The chat has no ID")?;
    let item = browser_cards::guest_call(
        Arc::clone(&source.guest),
        "browser.submit",
        json!({"agentId": agent_id, "url": url}),
        Some(request_id.to_owned()),
        submit_timeout(),
    )
    .await
    .map_err(|error| match error.detail() {
        Some(detail) if Some(detail) == contract()["unsupportedMessage"].as_str() => {
            text("unsupported").to_owned()
        }
        Some(detail) => detail.to_owned(),
        None => error.to_string(),
    })?;
    let Some(bridge_id) = item.get("id").and_then(Value::as_str).map(str::to_owned) else {
        return Err("The browser bridge returned no sign-in request".into());
    };
    let applied = browser_cards::apply(store, source, item.clone(), now_seconds())
        .await
        .map_err(|error| error.to_string())?;
    if applied.refused.is_some() {
        browser_cards::close_guest(Arc::clone(&source.guest), &bridge_id, "refused").await;
    }
    Ok((item, applied.refused))
}

/// The tool result of one submitted link (`sign_in_result`).
fn result(item: &Value, refused: Option<String>) -> Value {
    let host = item.get("host").cloned().unwrap_or(Value::Null);
    if let Some(reason) = refused {
        return json!({"status": "refused", "host": host,
                      "message": text("refused").replace("{reason}", &reason)});
    }
    let state = item
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let known = contract()["results"].get(state).is_some() && state != "refused";
    let status = if known { state } else { "closed" };
    let bridge_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
    json!({"status": status, "host": host, "requestId": browser_cards::card_id(bridge_id),
           "message": text(status).replace("{host}", host.as_str().unwrap_or("None"))})
}

/// `orchestration_sign_in` (`codex_browser_bridge.sign_in`): the agent gives a link that the
/// person must open. A refusal is the text for the model.
pub async fn tool(
    store: &RuntimeStore,
    sources: &SignInSources,
    agent: &Value,
    url: Option<&str>,
    request_id: &str,
) -> Result<Value, String> {
    let Some(url) = url.filter(|url| !url.is_empty()) else {
        return Err("Supply url: the link that the person must open to sign in".into());
    };
    let (item, refused) = submit(
        store,
        sources,
        agent,
        url,
        &format!("sign-in-tool:{request_id}"),
    )
    .await?;
    Ok(result(&item, refused))
}

/// The sign-in links of one completed agent message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageLinks {
    pub agent_id: String,
    /// The transcript item: `<agent ID>:<item ID>`.
    pub item_key: String,
    pub links: Vec<String>,
}

static MESSAGES: OnceLock<mpsc::Sender<MessageLinks>> = OnceLock::new();

/// Note a completed agent message. The note comes from inside the message transaction, so the
/// consumer (`run_messages`) reads the committed message before it acts. Without a running
/// consumer, nothing happens.
pub fn note_message(agent_id: &str, item_key: &str, text: &str) {
    let Some(sender) = MESSAGES.get() else {
        return;
    };
    if !text.contains("://") {
        return;
    }
    let links = crate::browser_policy::sign_in_links(text);
    if links.is_empty() {
        return;
    }
    let note = MessageLinks {
        agent_id: agent_id.to_owned(),
        item_key: item_key.to_owned(),
        links,
    };
    if let Err(mpsc::error::TrySendError::Full(_)) = sender.try_send(note) {
        eprintln!("sign-in links of message {item_key} have no card: the queue is full");
    }
}

/// The committed message and its chat, when the message is complete.
async fn committed(store: &RuntimeStore, note: &MessageLinks) -> Option<Option<Value>> {
    let (item_key, agent_id) = (note.item_key.clone(), note.agent_id.clone());
    let rows = store
        .read_async("runtime-services/sign_in", move |tx| {
            let read = |sql: &str,
                        key: &str|
             -> Result<Option<String>, studio_runtime_store::StoreError> {
                match tx.query_row(sql, [key], |row| row.get::<_, String>(0)) {
                    Ok(raw) => Ok(Some(raw)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(error) => Err(error.into()),
                }
            };
            Ok::<_, studio_runtime_store::StoreError>((
                read("SELECT record FROM runtime_items WHERE id=?1", &item_key)?,
                read("SELECT record FROM runtime_agents WHERE id=?1", &agent_id)?,
            ))
        })
        .await
        .ok()?;
    let item: Value = serde_json::from_str(&rows.0?).ok()?;
    if item.get("streaming") != Some(&Value::Bool(false)) {
        return None;
    }
    // A message that history recovery writes again is older than any open sign-in.
    if item
        .get("at")
        .and_then(Value::as_f64)
        .is_some_and(|at| at < now_seconds() - message_age_seconds())
    {
        return Some(None);
    }
    let agent = rows
        .1
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    Some(agent.filter(|agent| {
        !studio_runtime_store::truthy(agent.get("deletedAt"))
            && agent.get("role").and_then(Value::as_str) != Some("reviewer")
    }))
}

/// Show the sign-in links of one noted message as cards (`codex_browser_bridge.message_links`).
/// `waits` are the pauses before each commit check. A message that does not commit, a deleted
/// chat and a reviewer get no card; a link that the URL rules refuse stays an ordinary link.
pub async fn handle_message(
    store: &RuntimeStore,
    sources: &SignInSources,
    note: &MessageLinks,
    waits: &[u64],
) {
    let mut chat = None;
    for wait in waits {
        tokio::time::sleep(Duration::from_millis(*wait)).await;
        if let Some(found) = committed(store, note).await {
            chat = Some(found);
            break;
        }
    }
    let Some(Some(agent)) = chat else {
        return;
    };
    for link in &note.links {
        let key = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("{}\n{link}", note.item_key).as_bytes(),
        );
        let refused = match submit(
            store,
            sources,
            &agent,
            link,
            &format!("sign-in-message:{key}"),
        )
        .await
        {
            Ok((_, refused)) => refused,
            Err(error) => Some(error),
        };
        if let Some(reason) = refused {
            // No query value reaches the log.
            let reason: String = reason.chars().take(200).collect();
            eprintln!(
                "sign-in link of message {} has no card: {reason}",
                note.item_key
            );
        }
    }
}

/// Take noted messages until shutdown. One consumer runs in a server process.
pub async fn run_messages(
    store: Arc<RuntimeStore>,
    sources: SignInSources,
    mut shutdown: watch::Receiver<bool>,
) {
    let (sender, mut receiver) = mpsc::channel(MESSAGE_QUEUE);
    if MESSAGES.set(sender).is_err() {
        return;
    }
    loop {
        let note = tokio::select! {
            _ = shutdown.wait_for(|stopped| *stopped) => return,
            note = receiver.recv() => note,
        };
        let Some(note) = note else {
            return;
        };
        tokio::select! {
            _ = shutdown.wait_for(|stopped| *stopped) => return,
            () = handle_message(&store, &sources, &note, &COMMIT_WAITS_MS) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_and_the_results_follow_the_contract() {
        let catalog: Value =
            serde_json::from_str(include_str!("account_worker/preparation-resources.json"))
                .expect("catalog");
        let tools: Vec<&Value> = catalog["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter(|tool| tool["name"] == TOOL)
            .collect();
        assert_eq!(tools, vec![&contract()["tool"]]);
        assert_eq!(submit_timeout(), Duration::from_secs(20));
        assert!((message_age_seconds() - 600.0).abs() < f64::EPSILON);
        for state in [
            "pending",
            "approved",
            "opened",
            "declined",
            "closed",
            "refused",
            "unsupported",
        ] {
            assert!(!text(state).is_empty(), "{state}");
        }
        let item = json!({"id": "b1", "state": "pending", "host": "login.acme.dev"});
        let value = result(&item, None);
        assert_eq!(value["status"], "pending");
        assert_eq!(value["requestId"], browser_cards::card_id("b1"));
        assert!(
            value["message"]
                .as_str()
                .expect("text")
                .contains("login.acme.dev")
        );
        let other = result(&json!({"id": "b2", "state": "replaced", "host": "h"}), None);
        assert_eq!(other["status"], "closed");
        let refused = result(&item, Some("The chat is not a native chat".into()));
        assert_eq!(refused["status"], "refused");
        assert!(refused.get("requestId").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_host_bridge_names_an_unknown_method_as_the_contract_says() {
        struct NoChats;
        impl crate::browser_host::ChatLookup for NoChats {
            fn chat<'a>(
                &'a self,
                _session: &'a str,
            ) -> futures_util::future::BoxFuture<'a, Option<crate::browser_host::ChatContext>>
            {
                Box::pin(async { None })
            }
        }
        let folder = tempfile::tempdir().expect("folder");
        let bridge = crate::browser_host::HostBridge::open(
            folder.path(),
            Arc::new(NoChats),
            Default::default(),
        )
        .expect("bridge");
        let error = bridge
            .call("browser.unknown", &json!({}))
            .await
            .expect_err("refused");
        assert_eq!(
            Some(error.message.as_str()),
            contract()["unsupportedMessage"].as_str()
        );
    }
}
