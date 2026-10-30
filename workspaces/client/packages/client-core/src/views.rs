//! UI subscriptions contain values and errors. Rust owns refresh and retry policy.
use crate::{
    backend::Backend,
    protocol::{Request, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use studio_client_domain::json_object::JsonObject;
use tokio::{sync::mpsc, task::JoinHandle};

// Non-string optional fields historically behave like absent values.
#[derive(Deserialize)]
#[serde(untagged)]
enum TextField<'a> {
    Text(&'a str),
    Other(serde::de::IgnoredAny),
}
impl<'a> TextField<'a> {
    fn text(&self) -> Option<&'a str> {
        match self {
            Self::Text(value) => Some(value),
            Self::Other(_) => None,
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ViewInput<'a> {
    #[serde(borrow)]
    subscription: Option<TextField<'a>>,
    #[serde(borrow)]
    reason: Option<TextField<'a>>,
    #[serde(borrow)]
    credit: Option<TextField<'a>>,
    #[serde(borrow)]
    path: Option<TextField<'a>>,
    #[serde(borrow)]
    topic: Option<TextField<'a>>,
    #[serde(default)]
    flow_control: FlowControl,
}
#[derive(Default, Deserialize)]
#[serde(untagged)]
enum FlowControl {
    Enabled(bool),
    Other(serde::de::IgnoredAny),
    #[default]
    Missing,
}
fn decode<'a, T: Deserialize<'a>>(value: &'a Value) -> Option<T> {
    JsonObject::<T>::deserialize(value).ok().map(|v| v.0)
}
fn text<'a>(field: &Option<TextField<'a>>) -> &'a str {
    field.as_ref().and_then(TextField::text).unwrap_or("")
}

pub struct Views {
    tasks: HashMap<String, JoinHandle<()>>,
    flows: HashMap<String, Arc<crate::events::EventFlow>>,
    backend: Arc<Result<Backend, String>>,
    output: mpsc::Sender<Value>,
}

impl Views {
    pub fn new(backend: Arc<Result<Backend, String>>, output: mpsc::Sender<Value>) -> Self {
        Self {
            tasks: HashMap::new(),
            flows: HashMap::new(),
            backend,
            output,
        }
    }
    pub fn command(&mut self, request: &Request) -> Response {
        let Some((input, id)) = decode::<ViewInput>(&request.params).and_then(|input| {
            let id = input
                .subscription
                .as_ref()
                .and_then(TextField::text)
                .filter(|id| !id.is_empty() && id.len() <= 128)?;
            Some((input, id))
        }) else {
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "A subscription identity is required.",
            );
        };
        if request.method == "view.unsubscribe" {
            crate::stream_log::line(&format!(
                "unsubscribe {id} known={} views={} reason={}",
                self.tasks.contains_key(id),
                self.tasks.len(),
                text(&input.reason)
            ));
            self.flows.remove(id);
            if let Some(task) = self.tasks.remove(id) {
                task.abort();
            }
            return Response::success(request.id.clone(), json!({"closed":true}));
        }
        if request.method == "view.next" {
            return if self.flows.get(id).is_some_and(|flow| {
                input
                    .credit
                    .as_ref()
                    .and_then(TextField::text)
                    .is_some_and(|credit| flow.grant(credit))
            }) {
                Response::success(request.id.clone(), json!({"granted":true}))
            } else {
                crate::stream_log::line(&format!("invalid_credit {id}"));
                Response::failure(
                    request.id.clone(),
                    "invalid_credit",
                    "The event credit is no longer valid.",
                )
            };
        }
        if matches!(request.method.as_str(), "view.resume" | "view.restart") {
            let accepted = self.flows.get(id).is_some_and(|flow| {
                if request.method == "view.restart" {
                    let reason = text(&input.reason);
                    crate::stream_log::line(&format!("restart {id} reason={reason}"));
                    flow.restart(reason);
                    true
                } else {
                    input
                        .path
                        .as_ref()
                        .and_then(TextField::text)
                        .is_some_and(|path| flow.resume(path))
                }
            });
            return if accepted {
                Response::success(request.id.clone(), json!({"accepted":true}))
            } else {
                Response::failure(
                    request.id.clone(),
                    "invalid_subscription",
                    "The stream is not waiting for this request.",
                )
            };
        }
        if request.method != "view.subscribe"
            || self.tasks.contains_key(id)
            || self.tasks.len() >= 64
        {
            crate::stream_log::line(&format!(
                "refuse {id} method={} views={}",
                request.method,
                self.tasks.len()
            ));
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "The subscription already exists or the view limit was reached.",
            );
        }
        let topic = text(&input.topic);
        if !matches!(topic, "sse" | "microphone" | "voice" | "transcription") {
            return Response::failure(
                request.id.clone(),
                "invalid_subscription",
                "Unknown view topic.",
            );
        }
        let log_id = id.to_owned();
        let (id, topic, params) = (id.to_owned(), topic.to_owned(), request.params.clone());
        let flow = match input.flow_control {
            FlowControl::Missing | FlowControl::Enabled(false) => None,
            FlowControl::Enabled(true) if topic == "sse" => match crate::events::EventFlow::new() {
                Ok(flow) => Some(Arc::new(flow)),
                Err(error) => return Response::failure(request.id.clone(), "event_credit", error),
            },
            FlowControl::Enabled(_) | FlowControl::Other(_) => {
                return Response::failure(
                    request.id.clone(),
                    "invalid_subscription",
                    "Flow control requires a server event subscription.",
                );
            }
        };
        if let Some(flow) = &flow {
            self.flows.insert(id.clone(), flow.clone());
        }
        let (backend, output) = (self.backend.clone(), self.output.clone());
        let microphone = if topic == "microphone" || topic == "voice" {
            let attached = if topic == "voice" {
                crate::voice::attach(&params["session"])
            } else {
                crate::media::attach(&params["capture"])
            };
            match attached {
                Ok(value) => Some(value),
                Err(error) => return Response::failure(request.id.clone(), "media", error),
            }
        } else {
            None
        };
        self.tasks.insert(
            id.clone(),
            tokio::spawn(async move {
                if let Some(mut audio) = microphone {
                    while let Some(value) = audio.recv().await {
                        if output
                            .send(json!({"event":id,"value":value}))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                } else if topic == "transcription" {
                    let mut events = crate::speech::progress().subscribe();
                    loop {
                        match events.recv().await {
                            Ok(value) => {
                                if output
                                    .send(json!({"event":id,"value":value}))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                } else if topic == "sse" {
                    crate::events::run_controlled(id, params, backend, output, flow).await;
                }
            }),
        );
        crate::stream_log::line(&format!("subscribe {} views={}", log_id, self.tasks.len()));
        Response::success(request.id.clone(), json!({"subscribed":true}))
    }
}

impl Drop for Views {
    fn drop(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    fn request(method: &str, params: Value) -> Request {
        Request {
            id: "flow-test".into(),
            method: method.into(),
            params,
        }
    }
    fn views() -> Views {
        let (output, _) = mpsc::channel(16);
        Views::new(Arc::new(Err("No backend".into())), output)
    }
    #[tokio::test]
    async fn retired_topics_are_refused() {
        let mut owner = views();
        for topic in ["projection", "outbox", "drafts"] {
            let response = owner.command(&request(
                "view.subscribe",
                json!({"subscription":"retired","topic":topic}),
            ));
            assert_eq!(response.error.unwrap().code, "invalid_subscription");
            assert!(!owner.tasks.contains_key("retired"));
        }
    }
    #[tokio::test]
    async fn credits_are_one_use_and_bound_to_connection_and_subscription_incarnation() {
        let mut owner = views();
        let mut foreign = views();
        let subscribe = request(
            "view.subscribe",
            json!({"subscription":"one","topic":"sse","path":"/api/sync/stream","flowControl":true}),
        );
        assert!(owner.command(&subscribe).error.is_none());
        assert!(foreign.command(&subscribe).error.is_none());
        let token = owner.flows["one"].credit();
        let next = request("view.next", json!({"subscription":"one","credit":token}));
        assert!(foreign.command(&next).error.is_some());
        assert!(
            owner
                .command(&request(
                    "view.next",
                    json!({"subscription":"other","credit":token})
                ))
                .error
                .is_some()
        );
        assert!(owner.command(&next).error.is_none());
        assert!(owner.command(&next).error.is_some());
        assert!(
            owner
                .command(&request("view.unsubscribe", json!({"subscription":"one"})))
                .error
                .is_none()
        );
        assert!(owner.command(&next).error.is_some());
        assert!(owner.command(&subscribe).error.is_none());
        assert!(owner.command(&next).error.is_some());
    }
    #[tokio::test]
    async fn resume_and_restart_reach_only_this_connections_waiting_stream() {
        let mut owner = views();
        let mut foreign = views();
        let subscribe = request(
            "view.subscribe",
            json!({"subscription":"one","topic":"sse","path":"/api/sync/stream","flowControl":true}),
        );
        assert!(owner.command(&subscribe).error.is_none());
        let restart = request(
            "view.restart",
            json!({"subscription":"one","reason":"invalid-frame"}),
        );
        let resume = request(
            "view.resume",
            json!({"subscription":"one","path":"/api/sync/stream?next"}),
        );
        assert!(foreign.command(&restart).error.is_some());
        assert!(owner.command(&restart).error.is_none());
        // The stream has not asked for a path, so an early answer is refused.
        assert!(owner.command(&resume).error.is_some());
        owner.flows["one"].expect_path();
        assert!(foreign.command(&resume).error.is_some());
        assert!(owner.command(&resume).error.is_none());
        assert!(owner.command(&resume).error.is_some());
    }
    #[tokio::test]
    async fn malformed_commands_leave_subscription_and_credit_unchanged() {
        let mut owner = views();
        let subscribe = request(
            "view.subscribe",
            json!({"subscription":"one","topic":"sse","flowControl":true}),
        );
        assert!(owner.command(&subscribe).error.is_none());
        let token = owner.flows["one"].credit();
        for params in [
            json!({"subscription":"one","credit":false}),
            json!(["one", token]),
        ] {
            assert!(owner.command(&request("view.next", params)).error.is_some());
        }
        assert!(
            owner
                .command(&request(
                    "view.next",
                    json!({"subscription":"one","credit":token})
                ))
                .error
                .is_none()
        );
        assert!(
            owner
                .command(&request(
                    "view.unsubscribe",
                    json!({"subscription":"one","reason":false})
                ))
                .error
                .is_none()
        );
        for flow in [json!(null), json!(0), json!([])] {
            assert!(
                owner
                    .command(&request(
                        "view.subscribe",
                        json!({"subscription":"bad","topic":"sse","flowControl":flow})
                    ))
                    .error
                    .is_some()
            );
            assert!(!owner.tasks.contains_key("bad"));
        }
    }
    #[tokio::test]
    async fn push_views_remain_push_and_flow_control_is_sse_only() {
        let mut owner = views();
        assert!(
            owner
                .command(&request(
                    "view.subscribe",
                    json!({"subscription":"push","topic":"sse"})
                ))
                .error
                .is_none()
        );
        assert!(!owner.flows.contains_key("push"));
        assert!(
            owner
                .command(&request(
                    "view.next",
                    json!({"subscription":"push","credit":"guess"})
                ))
                .error
                .is_some()
        );
        for params in [
            json!({"subscription":"bad","topic":"outbox","flowControl":true}),
            json!({"subscription":"bad","topic":"sse","flowControl":"true"}),
        ] {
            assert!(
                owner
                    .command(&request("view.subscribe", params))
                    .error
                    .is_some()
            );
            assert!(!owner.tasks.contains_key("bad"));
        }
    }
}
