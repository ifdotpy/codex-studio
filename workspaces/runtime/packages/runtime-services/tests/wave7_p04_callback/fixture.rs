#![allow(clippy::unwrap_used)]
#[path = "../support/prepared_python.rs"]
mod prepared_python;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
use studio_runtime_services::{callback_runtime::*, provider_callbacks::*};
use studio_runtime_store::RuntimeStore;
pub fn template() -> &'static Value {
    static VALUE: OnceLock<Value> = OnceLock::new();
    VALUE.get_or_init(|| {
        let root = tempfile::tempdir().unwrap().keep();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let script =
            source.join("../../../packages/runtime-services/tests/wave7_p04_callback/oracle.py");
        let output = Command::new(prepared_python::prepared_python(&source))
            .arg(script)
            .arg(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    })
}
pub fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, CallbackContext) {
    let data = template();
    let root = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(data["template"].as_str().unwrap()).join(name),
            root.path().join(name),
        )
        .unwrap();
    }
    let store = Arc::new(RuntimeStore::open(Some(root.path())).unwrap());
    let context = CallbackContext {
        agent: data["agents"][0]["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: 3,
        thread: "thread-0".into(),
        turn: "turn-0".into(),
    };
    (root, store, context)
}
pub fn card(store: &RuntimeStore, value: &Value) {
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
            studio_runtime_store::QuestionsWriteRepository::put_request(
                tx,
                value["id"].as_str().unwrap(),
                &value.to_string(),
            )
        })
        .unwrap();
}
pub fn stored(store: &RuntimeStore, id: &str) -> Value {
    let raw: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_requests WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}
#[derive(Default)]
pub struct Port {
    pub calls: Mutex<Vec<Reply>>,
    pub next: Mutex<Option<ReplyOutcome>>,
    pub foreign: Mutex<bool>,
}
impl CallbackReplyPort for Port {
    fn deliver(
        &self,
        reply: &Reply,
        deadline: std::time::Duration,
    ) -> Result<ReplyReceipt, CallbackError> {
        assert_eq!(deadline, REPLY_DEADLINE);
        self.calls.lock().unwrap().push(reply.clone());
        let mut identity = reply.identity.clone();
        if *self.foreign.lock().unwrap() {
            identity.attempt = "foreign-attempt".into();
        }
        Ok(ReplyReceipt {
            identity,
            outcome: self
                .next
                .lock()
                .unwrap()
                .take()
                .unwrap_or(ReplyOutcome::Written),
        })
    }
}
pub fn pending(store: &RuntimeStore, context: &CallbackContext) -> (String, Frame) {
    let frame = Frame::Request(RequestFrame {
        id: RequestId::Number(42),
        method: Method::ItemToolRequestUserInput,
        params: Some(
            json!({"threadId":context.thread,"turnId":context.turn,"questions":[{"id":"secret","question":"Value?","isSecret":true}]}),
        ),
        fields: Default::default(),
    });
    let decision = dispatch(store, context, &frame, 10.0).unwrap();
    bind_journal(store, &decision, context, &RequestId::Number(42), 1).unwrap();
    let CallbackDecision::Pending(id) = decision else {
        panic!("pending")
    };
    (id, frame)
}
pub fn checkpoint(store: &RuntimeStore, context: &CallbackContext, cursor: u64) -> usize {
    store.transaction_with::<_,CallbackError>(|tx| {
        tx.execute("INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'ack',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result",rusqlite::params![format!("account-worker:cursor:{}:{}:ack",context.account,context.generation),cursor.to_string()])?;
        retire_checkpointed(tx,&context.account,context.generation)
    }).unwrap()
}
