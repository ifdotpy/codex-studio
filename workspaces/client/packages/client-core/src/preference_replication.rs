//! One native worker per admitted context. The existing mutation journal owns HTTP identity.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::Request,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::watch;

pub(crate) async fn run(
    backend: Arc<Result<Backend, String>>,
    store: StoreSender,
    mut changes: watch::Receiver<u64>,
) {
    let Ok(backend) = backend.as_ref() else {
        return;
    };
    let mut attempted = HashMap::<String, String>::new();
    loop {
        // Mark the wake before reading. A concurrent edit remains observable.
        changes.borrow_and_update();
        let next = state(
            &store,
            "preference_sync.next",
            json!({"attempted":attempted}),
        )
        .await;
        if let Ok(batch) = next
            && !batch.is_null()
        {
            let identity = batch["journal"]["requestId"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            let generation = batch["generation"].as_str().unwrap_or("").to_owned();
            if !identity.is_empty() && attempted.get(&identity) != Some(&generation) {
                attempted.insert(identity.clone(), generation);
                let body = batch["body"].clone();
                let scope = body["scope"].clone();
                let receipt = json!({"scope":scope,"requestId":identity,"payloadDigest":batch["journal"]["payloadDigest"]});
                // Uncertainty is durable before I/O. A crashed worker retains this
                // exact body and identity, including an accepted lost response.
                if state(&store, "preference_sync.dispatch", receipt.clone())
                    .await
                    .is_ok()
                {
                    let response=backend.dispatch(Request{id:identity.clone(),method:"api.request".into(),params:json!({"path":"/api/sync/preferences","method":"POST","body":body,"requestId":identity,"workspaceId":batch["workspaceId"],"timeoutMs":15000})}).await;
                    let outcome = if let Some(result) = response.result {
                        let mut params = receipt.clone();
                        params["fields"] = result["fields"].clone();
                        state(&store, "preference_sync.ack", params).await
                    } else {
                        Err(response.error.map(|e| e.message).unwrap_or_else(|| {
                            "The remote preference result is not confirmed.".into()
                        }))
                    };
                    if let Err(error) = outcome {
                        let mut params = receipt;
                        params["error"] = Value::String(error);
                        let _ = state(&store, "preference_sync.failure", params).await;
                        // The sibling scope can progress without retrying this
                        // uncertain batch on the same connection generation.
                        continue;
                    } else {
                        attempted.remove(&identity);
                        // An edit made during the request needs a separate batch.
                        continue;
                    }
                }
            }
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}
