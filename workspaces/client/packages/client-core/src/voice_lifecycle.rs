//! Native session identity, signaling, and deadlines. No renderer SDP or timers.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, Notify, watch};

/// Maximum time from the native start claim to session.started. Unit: seconds.
const START_SECONDS: u64 = 90;
/// Maximum time with a disconnected peer before native capture ends. Unit: seconds.
const RECONNECT_SECONDS: u64 = 12;

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS client_voice_attempts(id TEXT PRIMARY KEY,agent TEXT NOT NULL,muted INTEGER NOT NULL,document TEXT NOT NULL)")
}
fn text<'a>(p: &'a Value, field: &str) -> Result<&'a str, String> {
    p[field]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .ok_or_else(|| format!("Invalid voice {field}."))
}
pub fn stored(db: &mut Connection, request: &Request) -> Response {
    let result: Result<Value, String> = (|| {
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let p = &request.params;
        let result = if request.method == "voice_journal.begin" {
            let agent = text(p, "agent")?;
            let muted = p["muted"].as_bool().ok_or("Invalid voice mute state.")?;
            let previous: Option<(String, bool, String)> = tx
                .query_row(
                    "SELECT agent,muted,document FROM client_voice_attempts WHERE id=?1",
                    [&request.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some((owner, old_muted, document)) = previous {
                if owner != agent || old_muted != muted {
                    return Err("The voice request identity has different content.".into());
                }
                json!({"fresh":false,"row":serde_json::from_str::<Value>(&document).map_err(|e|e.to_string())?})
            } else {
                let row = json!({"requestId":request.id,"sessionId":crate::file_save::uuid()?,"agent":agent,"muted":muted,"phase":"claimed"});
                tx.execute(
                    "INSERT INTO client_voice_attempts VALUES(?1,?2,?3,?4)",
                    params![request.id, agent, muted, row.to_string()],
                )
                .map_err(|e| e.to_string())?;
                json!({"fresh":true,"row":row})
            }
        } else if request.method == "voice_journal.settle" {
            let id = text(p, "requestId")?;
            let saved: String = tx
                .query_row(
                    "SELECT document FROM client_voice_attempts WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            let mut row: Value = serde_json::from_str(&saved).map_err(|e| e.to_string())?;
            if row["sessionId"] != p["sessionId"] || row["agent"] != p["agent"] {
                return Err("The voice settlement belongs to another session.".into());
            }
            let phase = p["phase"]
                .as_str()
                .filter(|s| matches!(*s, "submitted" | "running" | "ended" | "unknown"))
                .ok_or("Invalid voice phase.")?;
            if row["phase"] == "ended" && phase != "ended" {
                return Err("The voice session already ended.".into());
            }
            row["phase"] = json!(phase);
            if let Some(error) = p.get("error") {
                row["error"] = error.clone();
            }
            tx.execute(
                "UPDATE client_voice_attempts SET document=?2 WHERE id=?1",
                params![id, row.to_string()],
            )
            .map_err(|e| e.to_string())?;
            row
        } else {
            return Err("Unknown native voice journal command.".into());
        };
        tx.commit().map_err(|e| e.to_string())?;
        Ok(result)
    })();
    match result {
        Ok(value) => Response::success(request.id.clone(), value),
        Err(error) => Response::failure(request.id.clone(), "voice", error),
    }
}

struct Session {
    store: StoreSender,
    backend: Backend,
    claim: Value,
    view: watch::Sender<Value>,
    closed: AtomicBool,
    submitted: AtomicBool,
    remote_set: AtomicBool,
    ready: AtomicBool,
    muted: AtomicBool,
    reconnect: AtomicU64,
    done: Notify,
    signaling: Mutex<()>,
}
fn session_owner() -> &'static Mutex<Option<Arc<Session>>> {
    static OWNER: OnceLock<Mutex<Option<Arc<Session>>>> = OnceLock::new();
    OWNER.get_or_init(Mutex::default)
}

/// Synthetic lifecycle state for stream fixtures. This owns no device or peer.
#[cfg(test)]
pub(crate) struct TestOwnedSession {
    session: Arc<Session>,
}
#[cfg(test)]
impl TestOwnedSession {
    pub(crate) fn session_id(&self) -> &str {
        self.session.id()
    }
    pub(crate) fn state(&self) -> Value {
        self.session.view.borrow().clone()
    }
    pub(crate) fn closed(&self) -> bool {
        self.session.closed.load(Ordering::Acquire)
    }
    pub(crate) async fn clear(self) {
        self.session.closed.store(true, Ordering::Release);
        self.session.done.notify_waiters();
        let mut owner = session_owner().lock().await;
        if owner
            .as_ref()
            .is_some_and(|owner| Arc::ptr_eq(owner, &self.session))
        {
            *owner = None;
        }
    }
}
#[cfg(test)]
impl Drop for TestOwnedSession {
    fn drop(&mut self) {
        self.session.closed.store(true, Ordering::Release);
        self.session.done.notify_waiters();
        if let Ok(mut owner) = session_owner().try_lock() {
            if owner
                .as_ref()
                .is_some_and(|owner| Arc::ptr_eq(owner, &self.session))
            {
                *owner = None;
            }
        } else if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let session = self.session.clone();
            runtime.spawn(async move {
                let mut owner = session_owner().lock().await;
                if owner
                    .as_ref()
                    .is_some_and(|owner| Arc::ptr_eq(owner, &session))
                {
                    *owner = None;
                }
            });
        }
    }
}
#[cfg(test)]
pub(crate) async fn test_owned_session(
    backend: &Backend,
    store: &StoreSender,
    agent: &str,
    request_id: &str,
    submitted: bool,
) -> Result<TestOwnedSession, String> {
    let (reply, receive) = tokio::sync::oneshot::channel();
    store
        .send((
            Request {
                id: request_id.into(),
                method: "voice_journal.begin".into(),
                params: json!({"agent":agent,"muted":false}),
            },
            reply,
        ))
        .await
        .map_err(|e| e.to_string())?;
    let response = receive.await.map_err(|e| e.to_string())?;
    let value = response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or("Missing fixture voice claim.".into())
    })?;
    if value["fresh"] != true {
        return Err("The fixture voice attempt already exists.".into());
    }
    let claim = value["row"].clone();
    let (view, _) = watch::channel(
        json!({"active":true,"sessionId":claim["sessionId"],"status":"Listening","muted":false,"partial":""}),
    );
    let session = Arc::new(Session {
        store: store.clone(),
        backend: backend.clone(),
        claim,
        view,
        closed: AtomicBool::new(false),
        submitted: AtomicBool::new(submitted),
        remote_set: AtomicBool::new(false),
        ready: AtomicBool::new(true),
        muted: AtomicBool::new(false),
        reconnect: AtomicU64::new(0),
        done: Notify::new(),
        signaling: Mutex::new(()),
    });
    let mut owner = session_owner().lock().await;
    if owner
        .as_ref()
        .is_some_and(|session| !session.closed.load(Ordering::Acquire))
    {
        return Err("Another fixture owns the voice lifecycle.".into());
    }
    *owner = Some(session.clone());
    Ok(TestOwnedSession { session })
}

fn inactive() -> Value {
    json!({"active":false,"status":"","partial":"","muted":false})
}
impl Session {
    fn id(&self) -> &str {
        self.claim["sessionId"].as_str().unwrap()
    }
    fn agent(&self) -> &str {
        self.claim["agent"].as_str().unwrap()
    }
    fn publish(&self, status: &str, partial: Option<&str>, error: Option<&str>) {
        self.view.send_modify(|view| {
            view["active"] = json!(!self.closed.load(Ordering::Acquire));
            view["status"] = json!(status);
            view["muted"] = json!(self.muted.load(Ordering::Acquire));
            if let Some(partial) = partial {
                view["partial"] = json!(partial);
            }
            if let Some(error) = error {
                view["error"] = json!(error);
            }
        });
    }
    async fn settle(&self, phase: &str, error: Option<&str>) -> Result<(), String> {
        state(&self.store,"voice_journal.settle",json!({"requestId":self.claim["requestId"],"sessionId":self.id(),"agent":self.agent(),"phase":phase,"error":error})).await.map(|_|())
    }
    async fn http(&self, path: &str, body: Value, suffix: &str) -> Result<Value, String> {
        self.backend.validate_identity()?;
        let workspace = state(&self.store, "identity.get", Value::Null).await?;
        let response = self.backend.dispatch(Request {
            id:format!("{}:{suffix}",self.id()),method:"api.request".into(),
            params:json!({"path":path,"body":body,"workspaceId":workspace["workspaceId"],"timeoutMs":15000}),
        }).await;
        response.result.ok_or_else(|| {
            response
                .error
                .map(|e| e.message)
                .unwrap_or("The voice response was not confirmed.".into())
        })
    }
}

pub async fn watch_session(store: &StoreSender, agent: &str) -> watch::Receiver<Value> {
    let (send, receive) = watch::channel(inactive());
    {
        let mut waiting = watchers().lock().await;
        waiting.retain(|row| row.output.receiver_count() > 0);
        waiting.push(Watcher {
            store: store.downgrade(),
            agent: agent.to_owned(),
            output: send,
            session: None,
        });
    }
    // Registration precedes the current-owner read, so neither side of a
    // concurrent Start can miss this view. The view also survives later Starts.
    if let Some(session) = current(store, agent).await {
        publish_waiters(&session).await;
    }
    receive
}
struct Watcher {
    store: tokio::sync::mpsc::WeakSender<crate::delivery::StoreCommand>,
    agent: String,
    output: watch::Sender<Value>,
    session: Option<String>,
}
fn watchers() -> &'static Mutex<Vec<Watcher>> {
    static WATCHERS: OnceLock<Mutex<Vec<Watcher>>> = OnceLock::new();
    WATCHERS.get_or_init(Mutex::default)
}

/// Receipt after a fixture forwarder processes a frame, including a discarded stale frame.
#[cfg(test)]
fn test_forwarder_receipts() -> &'static tokio::sync::broadcast::Sender<(String, Value)> {
    static RECEIPTS: OnceLock<tokio::sync::broadcast::Sender<(String, Value)>> = OnceLock::new();
    RECEIPTS.get_or_init(|| tokio::sync::broadcast::channel(16).0)
}
async fn publish_waiters(session: &Arc<Session>) {
    let mut receiver = session.view.subscribe();
    let outputs = {
        // Bind under the same owner lock that replaces a session. A delayed
        // registration for an old session cannot reclaim a replacement view.
        let owner = session_owner().lock().await;
        if !owner
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, session))
        {
            return;
        }
        let mut waiting = watchers().lock().await;
        let mut outputs = vec![];
        waiting.retain_mut(|row| {
            if row.output.receiver_count() == 0 {
                return false;
            }
            if row.agent == session.agent()
                && row
                    .store
                    .upgrade()
                    .is_some_and(|s| s.same_channel(&session.store))
                && row.session.as_deref() != Some(session.id())
            {
                outputs.push(row.output.clone());
                row.session = Some(session.id().to_owned());
            }
            true
        });
        outputs
    };
    if outputs.is_empty() {
        return;
    }
    #[cfg(test)]
    let fixture_session = session.id().to_owned();
    let source = Arc::downgrade(session);
    let session_id = session.id().to_owned();
    tokio::spawn(async move {
        loop {
            let value = receiver.borrow_and_update().clone();
            let active = value["active"] == true;
            let current = {
                // Keep replacement and watcher rebinding excluded until the
                // synchronous send finishes. No network or store I/O runs here.
                let owner = session_owner().lock().await;
                let current = owner
                    .as_ref()
                    .map_or(!active, |current| source.ptr_eq(&Arc::downgrade(current)));
                if current {
                    let waiting = watchers().lock().await;
                    for output in &outputs {
                        if waiting.iter().any(|row| {
                            row.session.as_deref() == Some(session_id.as_str())
                                && row.output.same_channel(output)
                        }) {
                            output.send_replace(value.clone());
                        }
                    }
                    true
                } else {
                    false
                }
            };
            #[cfg(test)]
            let _ignored = test_forwarder_receipts().send((fixture_session.clone(), value.clone()));
            if !current || !active || receiver.changed().await.is_err() {
                break;
            }
        }
    });
}

async fn end(
    session: Arc<Session>,
    error: Option<String>,
    notify_server: bool,
) -> Result<Value, String> {
    if session.closed.swap(true, Ordering::AcqRel) {
        return Ok(session.view.borrow().clone());
    }
    session.done.notify_waiters();
    let closed = super::dispatch(Request {
        id: format!("{}:native-close", session.id()),
        method: "voice.close".into(),
        params: json!({"session":session.id()}),
    })
    .await;
    if let Some(error) = closed.error {
        session.publish("Stopped", Some(""), Some(&error.message));
        return Err(error.message);
    }
    let mut failure = error;
    // Native capture stops first. Server End follows an in-flight Start.
    let _signaling = session.signaling.lock().await;
    if notify_server && session.submitted.load(Ordering::Acquire) {
        if let Err(error) = session
            .http(
                "/api/voice/end",
                json!({"agent":session.agent(),"session_id":session.id()}),
                "end",
            )
            .await
        {
            failure.get_or_insert(error);
        }
    }
    session
        .settle(
            if failure.is_some() {
                "unknown"
            } else {
                "ended"
            },
            failure.as_deref(),
        )
        .await?;
    session.publish("Stopped", Some(""), failure.as_deref());
    let mut owner = session_owner().lock().await;
    if owner
        .as_ref()
        .is_some_and(|owned| Arc::ptr_eq(owned, &session))
    {
        *owner = None;
    }
    Ok(session.view.borrow().clone())
}

pub async fn audio_idle() -> bool {
    session_owner()
        .lock()
        .await
        .as_ref()
        .is_none_or(|session| session.view.borrow()["active"] != true)
}
async fn current(store: &StoreSender, agent: &str) -> Option<Arc<Session>> {
    session_owner()
        .lock()
        .await
        .as_ref()
        .filter(|s| s.store.same_channel(store) && s.agent() == agent)
        .cloned()
}
pub async fn state_for(store: &StoreSender, agent: &str) -> Value {
    match current(store, agent).await {
        Some(s) => s.view.borrow().clone(),
        None => inactive(),
    }
}
pub async fn apply_session(
    backend: &Backend,
    store: &StoreSender,
    agent: &str,
    value: &Value,
) -> Result<(), String> {
    if let Err(error) = backend.validate_identity() {
        transport_closed(backend, store, agent).await?;
        return Err(error);
    }
    let Some(session) = current(store, agent).await.filter(|s| {
        s.id() == value["session_id"].as_str().unwrap_or("") && !s.closed.load(Ordering::Acquire)
    }) else {
        return Ok(());
    };
    let ended = value["ended"] == true || value["ended"].as_f64().is_some_and(f64::is_finite);
    if ended || value["error"].as_str().is_some_and(|s| !s.is_empty()) {
        end(session, value["error"].as_str().map(str::to_owned), false).await?;
        return Ok(());
    }
    if let Some(sdp) = value["sdp"].as_str().filter(|s| !s.is_empty()) {
        if !session.remote_set.swap(true, Ordering::AcqRel) {
            let response = super::dispatch(Request {
                id: format!("{}:answer", session.id()),
                method: "voice.answer".into(),
                params: json!({"session":session.id(),"sdp":sdp}),
            })
            .await;
            if let Some(error) = response.error {
                end(session, Some(error.message.clone()), true).await?;
                return Err(error.message);
            }
        }
    }
    Ok(())
}
pub async fn transport_closed(
    _backend: &Backend,
    store: &StoreSender,
    agent: &str,
) -> Result<(), String> {
    // Native cleanup uses the original Store owner even after credential revoke.
    // The saved Backend still rejects all network I/O with changed authority.
    if let Some(session) = current(store, agent)
        .await
        .filter(|s| !s.closed.load(Ordering::Acquire))
    {
        end(session,Some("Studio connection lost. The microphone is off. Start voice again after the connection returns.".into()),true).await?;
    }
    Ok(())
}
async fn peer_event(session: Arc<Session>, event: Value) {
    if session.closed.load(Ordering::Acquire) {
        return;
    }
    let failure = match event["type"].as_str().unwrap_or("") {
        "error" => Some(
            event["message"]
                .as_str()
                .unwrap_or("Voice failed.")
                .to_owned(),
        ),
        "microphone" if event["state"] == "ended" => Some("Microphone disconnected.".into()),
        "channel" if event["state"] == "closed" => {
            Some("Voice disconnected. The saved transcript remains.".into())
        }
        "state" => match event["state"].as_str().unwrap_or("") {
            "failed" => Some("Voice disconnected. The saved transcript remains.".into()),
            "connected" => {
                session.reconnect.fetch_add(1, Ordering::AcqRel);
                let _=super::dispatch(Request{id:format!("{}:unpause",session.id()),method:"voice.mute".into(),params:json!({"session":session.id(),"muted":session.muted.load(Ordering::Acquire)})}).await;
                session.publish(
                    if session.ready.load(Ordering::Acquire) {
                        "Listening"
                    } else {
                        "Connecting"
                    },
                    None,
                    None,
                );
                None
            }
            "disconnected" => {
                let generation = session.reconnect.fetch_add(1, Ordering::AcqRel) + 1;
                let _ = super::dispatch(Request {
                    id: format!("{}:pause", session.id()),
                    method: "voice.mute".into(),
                    params: json!({"session":session.id(),"muted":true}),
                })
                .await;
                session.publish("Reconnecting. Microphone paused", None, None);
                let pending = session.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(RECONNECT_SECONDS)).await;
                    if pending.reconnect.load(Ordering::Acquire) == generation
                        && !pending.closed.load(Ordering::Acquire)
                    {
                        let _ = end(
                            pending,
                            Some("Voice could not reconnect. The microphone is off.".into()),
                            true,
                        )
                        .await;
                    }
                });
                None
            }
            _ => None,
        },
        "message" => {
            let message = event["data"]
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or(Value::Null);
            match message["type"].as_str().unwrap_or("") {
                "session.started" => {
                    session.ready.store(true, Ordering::Release);
                    session.publish("Listening", None, None);
                    let _ = session.settle("running", None).await;
                    None
                }
                "input_transcript.added" | "output_transcript.added" => {
                    session.publish(
                        if message["type"] == "input_transcript.added" {
                            "Listening"
                        } else {
                            "Speaking"
                        },
                        Some(message["item"]["text"].as_str().unwrap_or("")),
                        None,
                    );
                    None
                }
                "delegation.created" => {
                    session.publish("Working", Some(""), None);
                    None
                }
                "turn.done" => {
                    session.publish("Listening", Some(""), None);
                    None
                }
                "error" => Some(
                    message["error"]["message"]
                        .as_str()
                        .unwrap_or("Voice failed.")
                        .to_owned(),
                ),
                _ => None,
            }
        }
        _ => None,
    };
    if let Some(error) = failure {
        let _ = end(session, Some(error), true).await;
    }
}

pub async fn dispatch(backend: &Backend, store: &StoreSender, request: Request) -> Response {
    let result:Result<Value,String>=async {
        let agent=text(&request.params,"agent")?;
        match request.method.as_str() {
            "voice.sessionState"=>{
                if let Some(request_id)=request.params.get("requestId") {
                    let request_id=request_id.as_str().filter(|id|!id.is_empty()&&id.len()<=128).ok_or("Invalid voice start identity.")?;
                    if current(store,agent).await.is_none_or(|session|session.claim["requestId"]!=request_id){return Ok(inactive());}
                }
                Ok(state_for(store,agent).await)
            },
            "voice.sessionEnd"=>{
                let Some(session)=current(store,agent).await else{return Ok(inactive());};
                if request.params["sessionId"]!=session.id(){return Err("The voice Stop belongs to another session.".into());}
                end(session,None,true).await
            },
            "voice.sessionMute"=>{
                let session=current(store,agent).await.filter(|s|s.id()==request.params["sessionId"].as_str().unwrap_or("")&&!s.closed.load(Ordering::Acquire)).ok_or("The voice session ended.")?;
                let muted=request.params["muted"].as_bool().ok_or("Invalid mute state.")?;
                let response=super::dispatch(Request{id:request.id.clone(),method:"voice.mute".into(),params:json!({"session":session.id(),"muted":muted})}).await;
                if let Some(error)=response.error{return Err(error.message);}
                session.muted.store(muted,Ordering::Release);
                let status=session.view.borrow()["status"].as_str().unwrap_or("Listening").to_owned();
                session.publish(&status,None,None);
                Ok(session.view.borrow().clone())
            },
            "voice.sessionStart"=>{
                backend.validate_identity()?;
                if state(store,"files.importPending",Value::Null).await?==true{return Err("Finish the byte import before starting voice.".into());}
                let muted=request.params["muted"].as_bool().unwrap_or(false);
                let identity=crate::sync::open(backend,store).await.map_err(|e|e.message)?;
                if !crate::projections::valid_workspace(identity["workspaceId"].as_str().unwrap_or("")){return Err("The voice workspace is not verified.".into());}
                let mut owner=session_owner().lock().await;
                if owner.as_ref().is_some_and(|s|!s.closed.load(Ordering::Acquire)){return Err("End the existing voice session first.".into());}
                let (reply,receive)=tokio::sync::oneshot::channel();
                store.send((Request{id:request.id.clone(),method:"voice_journal.begin".into(),params:json!({"agent":agent,"muted":muted})},reply)).await.map_err(|e|e.to_string())?;
                let response=receive.await.map_err(|e|e.to_string())?;
                let claim=response.result.ok_or_else(||response.error.map(|e|e.message).unwrap_or("The native voice claim was not confirmed.".into()))?;
                if claim["fresh"]!=true{return Err("This voice request already ran. Read its saved state. The microphone was not restarted.".into());}
                let (view,_)=watch::channel(json!({"active":true,"sessionId":claim["row"]["sessionId"],"status":"Connecting","partial":"","muted":muted}));
                let session=Arc::new(Session{store:store.clone(),backend:backend.clone(),claim:claim["row"].clone(),view,closed:AtomicBool::new(false),submitted:AtomicBool::new(false),remote_set:AtomicBool::new(false),ready:AtomicBool::new(false),muted:AtomicBool::new(muted),reconnect:AtomicU64::new(0),done:Notify::new(),signaling:Mutex::new(())});
                *owner=Some(session.clone());drop(owner);
                publish_waiters(&session).await;
                let starting=session.clone();tokio::spawn(async move{tokio::select!{_ = tokio::time::sleep(Duration::from_secs(START_SECONDS))=>{if !starting.ready.load(Ordering::Acquire){let _=end(starting,Some("Voice did not connect. Start again.".into()),true).await;}},_ = starting.done.notified()=>{}}});
                let start_result:Result<Value,String>=async {
                    session.http("/api/voice/status",json!({"agent":agent}),"status").await?;
                    if session.closed.load(Ordering::Acquire){return Err("Voice ended.".into());}
                    let mut events=super::attach(&json!(session.id()))?;
                    let listening=session.clone();tokio::spawn(async move{while let Some(event)=events.recv().await{peer_event(listening.clone(),event).await;}});
                    let created=super::dispatch(Request{id:format!("{}:create",session.id()),method:"voice.create".into(),params:json!({"session":session.id(),"muted":muted})}).await;
                    let offer=created.result.ok_or_else(||created.error.map(|e|e.message).unwrap_or("The native voice offer was not confirmed.".into()))?;
                    if session.closed.load(Ordering::Acquire){let _=super::dispatch(Request{id:format!("{}:late-close",session.id()),method:"voice.close".into(),params:json!({"session":session.id()})}).await;return Err("Voice ended.".into());}
                    let signaling=session.signaling.lock().await;
                    if session.closed.load(Ordering::Acquire){return Err("Voice ended.".into());}
                    session.settle("submitted",None).await?;
                    session.submitted.store(true,Ordering::Release);
                    let answer=session.http("/api/voice/start",json!({"agent":agent,"session_id":session.id(),"sdp":offer["sdp"]}),"start").await;
                    drop(signaling);
                    let answer=answer?;
                    apply_session(backend,store,agent,&answer).await?;
                    Ok(session.view.borrow().clone())
                }.await;
                if let Err(error)=&start_result{let _=end(session,Some(error.clone()),true).await;}
                start_result
            },_=>Err("Unknown native voice lifecycle command.".into())
        }
    }.await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "voice", error),
    }
}

pub async fn close_context(_backend: &Backend, store: &StoreSender) -> Result<(), String> {
    let session = session_owner()
        .lock()
        .await
        .as_ref()
        .filter(|s| s.store.same_channel(store))
        .cloned();
    if let Some(session) = session {
        end(session, None, true).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "voice_lifecycle_tests.rs"]
mod tests;
