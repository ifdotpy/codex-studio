//! Item 1 of the publication audit: a safety retry receipt (POST /api/action
//! with `safety`) reaches the client view of the chat.

use crate::entity_publication::test_support::{chat_view, seed_chat, store};
use serde_json::json;

#[tokio::test]
async fn a_safety_retry_receipt_reaches_the_client() {
    let (_dir, store) = store().await;
    seed_chat(
        &store,
        json!({"id":"safety-chat","rootId":"safety-chat","epoch":1,"status":"failed",
            "name":"Safety chat","provider":"codex","model":"gpt-6","prompt":"Start",
            "cwd":"/fixture","accountKey":"default","isLead":true,"autoWake":false}),
    )
    .await;
    let mut operation = json!({"id":"safety-chat:turn-1","agent":"safety-chat",
        "stage":"intent","turnId":"turn-1","epoch":1,"accountKey":"default",
        "model":"gpt-6","created":1.0});
    super::persist_operation(&store, &mut operation)
        .await
        .expect("the receipt is stored");
    let view = chat_view(&store, "safety-chat");
    assert_eq!(view["nativeSafetyRetry"]["id"], "safety-chat:turn-1");
    assert_eq!(view["nativeSafetyRetry"]["stage"], "intent");
}
