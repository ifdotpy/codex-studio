//! Paste mode and the pasted redirect (docs/vm-browser-bridge.md, section 6.3).

use super::{BrowserAnswerContext, BrowserAnswerError, answer};
use crate::linux_vm_guest::{GuestError, GuestRpc};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Respond = Box<dyn Fn(&str, &Value) -> Result<Value, GuestError> + Send + Sync>;

struct ScriptGuest {
    calls: Mutex<Vec<(String, Value, String)>>,
    respond: Respond,
}

impl ScriptGuest {
    fn new(
        respond: impl Fn(&str, &Value) -> Result<Value, GuestError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            respond: Box::new(respond),
        })
    }

    fn methods(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("test step")
            .iter()
            .map(|call| call.0.clone())
            .collect()
    }
}

impl GuestRpc for ScriptGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        _timeout: Duration,
        _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        self.calls.lock().expect("test step").push((
            method.to_owned(),
            params.clone(),
            request_id.unwrap_or_default().to_owned(),
        ));
        (self.respond)(method, params)
    }
}

const BRIDGE: &str = "0123456789abcdef0123456789abcdef";
const AGENT: &str = "browser-paste-agent";
const LINK: &str = "https://id.example.com/auth?client_id=kube&redirect_uri=http%3A%2F%2Flocalhost%3A8000&response_type=code&scope=openid&state=S1";

fn approve_then(
    callback: impl Fn(&Value) -> Result<Value, GuestError> + Send + Sync + 'static,
) -> Arc<ScriptGuest> {
    ScriptGuest::new(move |method, params| match method {
        "browser.answer" => Ok(
            json!({"state":"approved","lease":{"host":"localhost","port":8000,"path":"/","expiresAt":700.0}}),
        ),
        "browser.callback" => callback(params),
        "browser.close" => Ok(json!({"state":"closed"})),
        _ => Err(GuestError::InvalidRequest),
    })
}

fn signed_in(_: &Value) -> Result<Value, GuestError> {
    Ok(json!({"status":200,"contentType":"text/html","body":"PGgxPm9rPC9oMT4=","done":true}))
}

async fn store(
    url: &str,
    redirect: Value,
) -> (
    tempfile::TempDir,
    studio_runtime_store::RuntimeStore,
    String,
) {
    let dir = tempfile::tempdir().expect("temporary runtime state");
    let store = studio_runtime_store::RuntimeStore::open(Some(dir.path()))
        .await
        .expect("runtime store opens");
    let id = "browser-paste-card".to_owned();
    let record = json!({
        "id": id, "agent": AGENT, "method": "browser/open", "status": "pending",
        "params": {"bridgeId": BRIDGE, "url": url, "host": "id.example.com", "redirect": redirect,
                   "source": "vm", "expiresAt": 600.0}
    });
    let request_id = id.clone();
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                [
                    AGENT,
                    &json!({"id":AGENT,"rootId":AGENT,"epoch":1,"status":"running","autoWake":true})
                        .to_string(),
                ],
            )
            .map_err(studio_runtime_store::StoreError::from)?;
            studio_runtime_store::QuestionsWriteRepository::put_request(
                tx,
                &request_id,
                &studio_runtime_store::python_json_utf8(&record),
            )
        })
        .await
        .expect("browser request is seeded");
    (dir, store, id)
}

fn context(
    guest: Arc<ScriptGuest>,
    local: bool,
) -> BrowserAnswerContext<impl Fn(&Value) -> bool + Send + Sync + 'static> {
    BrowserAnswerContext {
        local_window: local,
        guest,
        local_forwarder: None,
        now: 100.0,
        validate: |_: &Value| true,
        http_providers: Default::default(),
    }
}

fn record(store: &studio_runtime_store::RuntimeStore, id: &str) -> Value {
    let raw = store
        .test_read_transaction(|tx| studio_runtime_store::QuestionsWriteRepository::request(tx, id))
        .expect("request reads")
        .expect("request exists");
    serde_json::from_str(&raw).expect("request JSON")
}

fn events(store: &studio_runtime_store::RuntimeStore) -> Vec<(String, String, String)> {
    let connection = store.test_reader().expect("read");
    let mut statement = connection
        .prepare("SELECT id,kind,text FROM runtime_events ORDER BY id")
        .expect("test step");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("test step")
        .collect::<Result<_, _>>()
        .expect("test step")
}

fn redirect() -> Value {
    json!({"host":"localhost","port":8000,"path":"/"})
}

async fn open_in_paste_mode(
    store: &studio_runtime_store::RuntimeStore,
    id: &str,
    guest: &Arc<ScriptGuest>,
) {
    let opened = answer(
        store,
        id,
        &json!({"decision":"accept","paste":true}),
        context(Arc::clone(guest), false),
    )
    .await
    .expect("paste mode approves")
    .expect("browser card");
    assert_eq!(opened.response["openUrl"], LINK);
}

fn refusal(result: Result<Option<super::BrowserAnswerResult>, BrowserAnswerError>) -> String {
    match result {
        Err(BrowserAnswerError::Refused(text)) => text,
        Err(other) => panic!("unexpected error: {other}"),
        Ok(value) => panic!("accepted: {:?}", value.map(|value| value.response)),
    }
}

#[tokio::test]
async fn a_remote_window_opens_in_paste_mode_and_the_card_waits() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    let guest = approve_then(signed_in);
    let refused = answer(
        &store,
        &id,
        &json!({"decision":"accept"}),
        context(Arc::clone(&guest), false),
    )
    .await;
    assert!(
        matches!(refused, Err(BrowserAnswerError::Invalid(_))),
        "a remote accept without paste stays refused"
    );
    open_in_paste_mode(&store, &id, &guest).await;
    let waiting = record(&store, &id);
    assert_eq!(waiting["status"], "pending");
    assert_eq!(waiting["params"]["phase"], "waiting");
    assert_eq!(waiting["params"]["waiting"]["expiresAt"], 700.0);
    let replay = answer(
        &store,
        &id,
        &json!({"decision":"accept","paste":true}),
        context(Arc::clone(&guest), false),
    )
    .await
    .expect("test step")
    .expect("test step");
    assert!(replay.replayed);
    assert_eq!(replay.response["openUrl"], LINK);
    assert_eq!(guest.methods(), vec!["browser.answer"]);
}

#[tokio::test]
async fn a_pasted_redirect_reaches_the_command_once() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    let guest = approve_then(signed_in);
    open_in_paste_mode(&store, &id, &guest).await;
    let pasted = json!({"decision":"answer","result":"localhost:8000/?code=fake-code&state=S1"});
    let first = answer(&store, &id, &pasted, context(Arc::clone(&guest), false))
        .await
        .expect("test step")
        .expect("test step");
    assert_eq!(first.response, json!({"status":"answered"}));
    let second = answer(&store, &id, &pasted, context(Arc::clone(&guest), false))
        .await
        .expect("test step")
        .expect("test step");
    assert!(second.replayed);
    assert_eq!(guest.methods(), vec!["browser.answer", "browser.callback"]);
    let calls = guest.calls.lock().expect("test step").clone();
    assert_eq!(
        calls[1].1,
        json!({"id":BRIDGE,"target":"/?code=fake-code&state=S1","headers":{}})
    );
    assert!(calls[1].2.starts_with("browser-callback:"));
    let done = record(&store, &id);
    assert_eq!(done["status"], "answered");
    assert_eq!(done["params"]["outcome"], "signed-in");
    assert_eq!(done["params"]["outcomeHttp"], 200);
    assert_eq!(done["decision"], "answer");
    assert!(
        !done.to_string().contains("fake-code"),
        "Studio stores no pasted address"
    );
    assert_eq!(
        events(&store),
        vec![(
            format!("browser-outcome:{BRIDGE}"),
            "browser_outcome".to_owned(),
            "The person signed in at id.example.com. Studio sent the result to the command on port 8000; the command answered HTTP 200.".to_owned()
        )]
    );
}

#[tokio::test]
async fn a_result_of_another_sign_in_is_refused_and_nothing_is_relayed() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    let guest = approve_then(signed_in);
    open_in_paste_mode(&store, &id, &guest).await;
    for (text, message) in [
        (
            "http://localhost:8000/?code=x&state=OTHER",
            "The address belongs to another sign-in: its state value is different",
        ),
        (
            "http://localhost:9000/?code=x&state=S1",
            "The address is not for this sign-in: it must use port 8000 and the path /",
        ),
        (
            "http://localhost:8000/?state=S1",
            "The address has no sign-in result. Paste it after the sign-in ends",
        ),
    ] {
        let refused = answer(
            &store,
            &id,
            &json!({"decision":"answer","result":text}),
            context(Arc::clone(&guest), false),
        )
        .await;
        assert_eq!(refusal(refused), message);
    }
    assert_eq!(guest.methods(), vec!["browser.answer"]);
    let waiting = record(&store, &id);
    assert_eq!(waiting["status"], "pending");
    assert!(waiting["params"].get("paste").is_none());
}

#[tokio::test]
async fn a_paste_before_the_approval_or_after_the_deadline_is_refused() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    let guest = approve_then(|_| {
        Err(GuestError::Rejected {
            code: Some("stale".into()),
            detail: Some("The sign-in forward is closed".into()),
            uncertain: false,
        })
    });
    let pasted = json!({"decision":"answer","result":"http://localhost:8000/?code=x&state=S1"});
    let early = answer(&store, &id, &pasted, context(Arc::clone(&guest), false)).await;
    assert_eq!(
        refusal(early),
        "This sign-in does not wait for a pasted address."
    );
    assert!(guest.methods().is_empty());
    open_in_paste_mode(&store, &id, &guest).await;
    let late = answer(&store, &id, &pasted, context(Arc::clone(&guest), false)).await;
    assert_eq!(
        refusal(late),
        "The sign-in command stopped waiting. Ask the agent to run it again."
    );
    let expired = record(&store, &id);
    assert_eq!(expired["status"], "expired");
    assert_eq!(expired["closeReason"], "stale");
    assert_eq!(expired["params"]["outcome"], "expired");
}

#[tokio::test]
async fn a_lost_relay_reply_is_unknown_and_never_sent_again() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    let guest = approve_then(|_| {
        Err(GuestError::Transport {
            message: "reply lost".into(),
            uncertain: true,
        })
    });
    open_in_paste_mode(&store, &id, &guest).await;
    let pasted = json!({"decision":"answer","result":"http://localhost:8000/?code=x&state=S1"});
    let lost = answer(&store, &id, &pasted, context(Arc::clone(&guest), false)).await;
    assert!(refusal(lost).starts_with("The result of the sign-in is unknown."));
    let unknown = record(&store, &id);
    assert_eq!(unknown["status"], "uncertain");
    assert_eq!(unknown["params"]["outcome"], "unknown");
    let replay = answer(&store, &id, &pasted, context(Arc::clone(&guest), false))
        .await
        .expect("test step")
        .expect("test step");
    assert!(replay.replayed);
    assert_eq!(
        guest.methods(),
        vec!["browser.answer", "browser.callback", "browser.close"]
    );
    assert_eq!(
        guest.calls.lock().expect("test step")[2].1,
        json!({"id":BRIDGE,"reason":"unknown"})
    );
}

#[tokio::test]
async fn a_relay_that_the_bridge_did_not_send_keeps_the_card_waiting_for_a_retry() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    let refused_once = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let first = Arc::clone(&refused_once);
    let guest = approve_then(move |params| {
        if first.swap(true, std::sync::atomic::Ordering::SeqCst) {
            signed_in(params)
        } else {
            // The host bridge could not read its listeners and forwarded nothing.
            Err(GuestError::Rejected {
                code: Some(crate::browser_host::NOT_SENT_CODE.into()),
                detail: Some("Studio could not read the listening ports of this computer".into()),
                uncertain: false,
            })
        }
    });
    open_in_paste_mode(&store, &id, &guest).await;
    let pasted = json!({"decision":"answer","result":"http://localhost:8000/?code=x&state=S1"});
    let refused = answer(&store, &id, &pasted, context(Arc::clone(&guest), false)).await;
    assert_eq!(
        refusal(refused),
        "Studio could not check the sign-in command now. Nothing was sent. Paste the address again."
    );
    let waiting = record(&store, &id);
    assert_eq!(waiting["status"], "pending");
    assert_eq!(waiting["params"]["phase"], "waiting");
    assert!(
        waiting["params"].get("paste").is_none(),
        "a retry may paste"
    );
    assert!(waiting["params"].get("outcome").is_none());
    let retried = answer(&store, &id, &pasted, context(Arc::clone(&guest), false))
        .await
        .expect("test step")
        .expect("test step");
    assert_eq!(retried.response, json!({"status":"answered"}));
    assert_eq!(record(&store, &id)["params"]["outcome"], "signed-in");
    assert_eq!(
        guest.methods(),
        vec!["browser.answer", "browser.callback", "browser.callback"],
        "the refused relay closes nothing"
    );
}

#[tokio::test]
async fn a_relay_that_failed_after_the_forward_started_stays_unknown() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    // The command did not answer after the bridge sent the request to it.
    let guest = approve_then(|_| {
        Err(GuestError::Rejected {
            code: Some("unavailable".into()),
            detail: Some("The sign-in command did not answer on port 8000".into()),
            uncertain: false,
        })
    });
    open_in_paste_mode(&store, &id, &guest).await;
    let pasted = json!({"decision":"answer","result":"http://localhost:8000/?code=x&state=S1"});
    let failed = answer(&store, &id, &pasted, context(Arc::clone(&guest), false)).await;
    assert!(refusal(failed).starts_with("The result of the sign-in is unknown."));
    let unknown = record(&store, &id);
    assert_eq!(unknown["status"], "uncertain");
    assert_eq!(unknown["params"]["outcome"], "unknown");
    assert_eq!(
        guest.methods(),
        vec!["browser.answer", "browser.callback", "browser.close"]
    );
}

#[tokio::test]
async fn decline_while_waiting_cancels_the_sign_in() {
    let (_dir, store, id) = store(LINK, redirect()).await;
    let guest = approve_then(signed_in);
    open_in_paste_mode(&store, &id, &guest).await;
    answer(
        &store,
        &id,
        &json!({"decision":"decline"}),
        context(Arc::clone(&guest), false),
    )
    .await
    .expect("test step")
    .expect("test step");
    let cancelled = record(&store, &id);
    assert_eq!(cancelled["status"], "answered");
    assert_eq!(cancelled["decision"], "decline");
    assert_eq!(cancelled["params"]["outcome"], "cancelled");
    assert_eq!(
        guest.calls.lock().expect("test step")[1].1,
        json!({"id":BRIDGE,"reason":"cancelled"})
    );
    let events = events(&store);
    assert_eq!(events[0].0, format!("browser-declined:{BRIDGE}"));
    assert_eq!(
        events[0].2,
        "The person cancelled the sign-in at id.example.com. The sign-in command keeps waiting until its own timeout. Stop it, or ask the person before you run it again."
    );
}

#[tokio::test]
async fn a_device_code_page_opens_from_any_window_without_a_forward() {
    let device = "https://device.sso.us-east-1.amazonaws.com/?user_code=ABCD-EFGH";
    let (_dir, store, id) = store(device, Value::Null).await;
    let guest = ScriptGuest::new(|method, _| match method {
        "browser.answer" => Ok(json!({"state":"opened","lease":null})),
        _ => Err(GuestError::InvalidRequest),
    });
    for _ in 0..2 {
        let opened = answer(
            &store,
            &id,
            &json!({"decision":"accept","paste":true}),
            context(Arc::clone(&guest), false),
        )
        .await
        .expect("test step")
        .expect("test step");
        assert_eq!(opened.response["openUrl"], device);
    }
    assert_eq!(record(&store, &id)["status"], "answered");
    assert_eq!(guest.methods(), vec!["browser.answer"]);
}
