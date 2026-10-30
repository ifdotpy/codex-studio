//! Commands delivered to the native state worker.
use crate::protocol::{Request, Response};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

pub enum StoreCommand {
    Request(Request, oneshot::Sender<Response>),
    PutDocuments {
        workspace: String,
        scope: String,
        documents: Vec<crate::projections::CheckedDocument>,
        reply: oneshot::Sender<Response>,
    },
    ApplyEntities {
        workspace: String,
        changes: Vec<studio_client_domain::sync_entity::EntityChange>,
        replace: bool,
        checkpoint: Option<u64>,
        expected_revision: Option<u64>,
        reply: oneshot::Sender<Response>,
    },
}
pub type StoreSender = mpsc::Sender<StoreCommand>;

impl StoreCommand {
    pub(crate) fn execute(self, store: &mut crate::store::Store) {
        let (reply, response) = match self {
            Self::Request(request, reply) => (reply, store.dispatch(request)),
            Self::PutDocuments {
                workspace,
                scope,
                documents,
                reply,
            } => {
                let result = crate::projections::put_documents(
                    &store.connection,
                    &workspace,
                    &scope,
                    &documents,
                );
                (reply, projection_response(result))
            }
            Self::ApplyEntities {
                workspace,
                changes,
                replace,
                checkpoint,
                expected_revision,
                reply,
            } => {
                let result = crate::projections::apply_entity_batch(
                    &store.connection,
                    &workspace,
                    &changes,
                    replace,
                    checkpoint,
                    expected_revision,
                );
                (reply, projection_response(result))
            }
        };
        drop(reply.send(response));
    }
    pub(crate) fn unavailable(self, error: &str) {
        let (id, reply) = match self {
            Self::Request(request, reply) => (request.id, reply),
            Self::PutDocuments { reply, .. } | Self::ApplyEntities { reply, .. } => {
                ("core-delivery".into(), reply)
            }
        };
        drop(reply.send(Response::failure(id, "client_state_unavailable", error)));
    }
}
fn projection_response(result: Result<Value, String>) -> Response {
    match result {
        Ok(value) => Response::success("core-delivery".into(), value),
        Err(error) => Response::failure("core-delivery".into(), "projection_error", error),
    }
}
async fn typed_state(
    sender: &StoreSender,
    command: StoreCommand,
    receive: oneshot::Receiver<Response>,
) -> Result<Value, String> {
    sender
        .send(command)
        .await
        .map_err(|_| "The state worker stopped.")?;
    let response = receive.await.map_err(|_| "The state worker stopped.")?;
    response.result.ok_or_else(|| {
        response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "Missing state response.".into())
    })
}
pub(crate) async fn put_documents(
    sender: &StoreSender,
    workspace: &str,
    scope: &str,
    documents: Vec<crate::projections::CheckedDocument>,
) -> Result<Value, String> {
    let (reply, receive) = oneshot::channel();
    typed_state(
        sender,
        StoreCommand::PutDocuments {
            workspace: workspace.into(),
            scope: scope.into(),
            documents,
            reply,
        },
        receive,
    )
    .await
}
pub(crate) async fn apply_entities(
    sender: &StoreSender,
    workspace: &str,
    changes: Vec<studio_client_domain::sync_entity::EntityChange>,
    replace: bool,
    checkpoint: Option<u64>,
    expected_revision: Option<u64>,
) -> Result<Value, String> {
    let (reply, receive) = oneshot::channel();
    typed_state(
        sender,
        StoreCommand::ApplyEntities {
            workspace: workspace.into(),
            changes,
            replace,
            checkpoint,
            expected_revision,
            reply,
        },
        receive,
    )
    .await
}

pub async fn state(sender: &StoreSender, method: &str, params: Value) -> Result<Value, String> {
    let (reply, receive) = oneshot::channel();
    sender
        .send(StoreCommand::Request(
            Request {
                id: "core-delivery".into(),
                method: method.into(),
                params,
            },
            reply,
        ))
        .await
        .map_err(|_| "The state worker stopped.")?;
    let response = receive.await.map_err(|_| "The state worker stopped.")?;
    response.result.ok_or_else(|| {
        response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "Missing state response.".into())
    })
}
