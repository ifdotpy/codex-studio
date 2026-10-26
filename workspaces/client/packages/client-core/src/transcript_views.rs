//! Server-scoped conversation views. The renderer sends display commands only.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    sync::{Arc, Mutex, Weak},
};
use studio_runtime_schema::{
    first_error,
    shared_values::{
        MAX_SAFE_INTEGER, MAX_TRANSCRIPT_CACHE_ITEMS, MAX_TRANSCRIPT_CACHE_MEMORY_BYTES,
        MAX_TRANSCRIPT_CACHE_SCOPES, MAX_TRANSCRIPT_PAGE_ITEMS, MAX_TRANSCRIPT_PAGE_MEMORY_BYTES,
        SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS,
    },
};
use tokio::{
    sync::{mpsc, watch},
    task::AbortHandle,
};

// Concurrent conversation view owners per native connection, in views.
const MAX_OPEN_TRANSCRIPT_VIEWS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Source {
    workspace: String,
    id: Option<String>,
    kind: String,
    managed: bool,
    enabled: bool,
}
impl Source {
    fn parse(params: &Value) -> Result<Self, String> {
        let workspace = params["workspace"]
            .as_str()
            .filter(|id| crate::projections::valid_workspace(id))
            .ok_or("A verified conversation workspace is required.")?
            .to_owned();
        let kind = params["kind"]
            .as_str()
            .filter(|kind| matches!(*kind, "agent" | "room" | "legacy"))
            .ok_or("Unknown conversation source kind.")?
            .to_owned();
        let id = match &params["id"] {
            Value::Null => None,
            Value::String(id) if !id.is_empty() && id.len() <= 1024 => Some(id.clone()),
            _ => return Err("Invalid conversation identity.".into()),
        };
        let result = Self {
            workspace,
            id,
            kind,
            managed: params["managed"].as_bool().unwrap_or(false),
            enabled: params["enabled"].as_bool().unwrap_or(true),
        };
        if result.id.is_some() {
            validate("ResourceRef", &result.resource())?;
        }
        Ok(result)
    }
    fn key(&self) -> String {
        json!([self.workspace, self.kind, self.managed, self.id]).to_string()
    }
    fn resource(&self) -> Value {
        match self.kind.as_str() {
            "agent" => {
                json!({"kind":"transcript","agentId":self.id,"tail":SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS})
            }
            "legacy" => json!({"kind":"room","roomId":self.id,"legacy":true}),
            _ => json!({"kind":"room","roomId":self.id}),
        }
    }
}

#[derive(Clone, Default)]
struct Page {
    items: Vec<Value>,
    before: Value,
    after: Value,
    historical: bool,
    version: String,
    latest: Option<Value>,
    agent: Value,
    loaded: bool,
    notice: String,
    anchor: Option<String>,
    connection: String,
    loading: bool,
}
struct View {
    id: String,
    generation: u64,
    operation: u64,
    revision: u64,
    source: Option<Source>,
    pages: BTreeMap<String, Page>,
    lru: VecDeque<String>,
    output: watch::Sender<Value>,
    stream: Option<AbortHandle>,
    closed: bool,
}
impl Drop for View {
    fn drop(&mut self) {
        if let Some(stream) = self.stream.take() {
            stream.abort();
        }
    }
}
impl View {
    fn page(&self) -> Option<&Page> {
        self.source
            .as_ref()
            .and_then(|source| self.pages.get(&source.key()))
    }
    fn page_mut(&mut self) -> &mut Page {
        let key = self.source.as_ref().expect("selected conversation").key();
        self.pages.entry(key).or_default()
    }
    fn snapshot(&self) -> Value {
        let empty = Page::default();
        let page = self.page().unwrap_or(&empty);
        json!({"viewId":self.id,"generation":self.generation,"revision":self.revision,"closed":self.closed,
            "loaded":self.source.as_ref().is_some_and(|source|source.id.is_none()) || page.loaded,
            "items":page.items,"notice":page.notice,"before":page.before,
            "after":page.after.as_str().map(str::to_owned).or_else(||page.after.as_u64().map(|n|n.to_string())),
            "historical":page.historical,"historyVersion":page.version,"pageLoading":page.loading,
            "liveAgent":page.agent,"connection":page.connection})
    }
    fn publish(&mut self) {
        self.trim();
        self.revision += 1;
        self.output.send_replace(self.snapshot());
    }
    fn trim(&mut self) {
        loop {
            let count: usize = self.pages.values().map(|p| p.items.len()).sum();
            let bytes: usize = self
                .pages
                .values()
                .flat_map(|p| &p.items)
                .map(memory_bytes)
                .sum();
            if self.pages.len() <= MAX_TRANSCRIPT_CACHE_SCOPES
                && count <= MAX_TRANSCRIPT_CACHE_ITEMS
                && bytes <= MAX_TRANSCRIPT_CACHE_MEMORY_BYTES
            {
                break;
            }
            let active = self.source.as_ref().map(Source::key);
            let Some(index) = self.lru.iter().position(|key| Some(key) != active.as_ref()) else {
                break;
            };
            let key = self.lru.remove(index).unwrap();
            self.pages.remove(&key);
        }
    }
}
struct Inner {
    backend: Arc<Result<Backend, String>>,
    store: StoreSender,
    views: Mutex<HashMap<String, Arc<Mutex<View>>>>,
}

#[derive(Clone)]
pub struct TranscriptViews {
    inner: Arc<Inner>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{StoreCommand, state};
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    fn source() -> Source {
        Source {
            workspace: "a".repeat(32),
            id: Some("lead".into()),
            kind: "agent".into(),
            managed: true,
            enabled: true,
        }
    }

    #[test]
    fn transcript_view_display_matches_shared_browser_vectors() {
        let fixture: Value = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/transcript-presentation.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let displayed = transcript_messages(
                case["source"].as_array().unwrap(),
                case["id"].as_str().unwrap(),
            );
            let selected: Vec<Value> = displayed
                .iter()
                .map(|row| {
                    Value::Object(
                        fixture["fields"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter_map(|field| {
                                let field = field.as_str().unwrap();
                                row.get(field)
                                    .map(|value| (field.to_owned(), value.clone()))
                            })
                            .collect(),
                    )
                })
                .collect();
            assert_eq!(json!(selected), case["expected"]);
        }
    }

    #[test]
    fn transcript_view_memory_uses_utf16_estimates_and_visible_message_bounds() {
        assert_eq!(memory_bytes(&json!({"text":"é🦀"})), 28);
        let inputs: Vec<Value> = (0..MAX_TRANSCRIPT_PAGE_ITEMS + 1)
            .map(|index| json!({"id":index.to_string(),"kind":"user","text":"é🦀"}))
            .collect();
        let rendered = transcript_messages(
            &[json!({"id":"batch","role":"user","text":"batch","inputs":inputs})],
            "lead",
        );
        assert_eq!(rendered.len(), MAX_TRANSCRIPT_PAGE_ITEMS + 1);
        let (bounded, before, _) = bound(&rendered, false, None);
        assert!(bounded.len() <= MAX_TRANSCRIPT_PAGE_ITEMS);
        assert!(
            bounded.iter().map(memory_bytes).sum::<usize>() <= MAX_TRANSCRIPT_PAGE_MEMORY_BYTES
        );
        assert!(before);
        let oversized = json!({"id":"oversized","role":"assistant","text":"x".repeat(MAX_TRANSCRIPT_PAGE_MEMORY_BYTES/2)});
        let (bounded, before, after) = bound(&[oversized], false, None);
        assert!(bounded.is_empty());
        assert!(!before);
        assert!(after);
    }

    #[test]
    fn transcript_view_focused_history_keeps_anchors_and_exact_opaque_cursors() {
        let mut page = Page::default();
        let source = source();
        apply_live(&mut page,&source,&json!({"items":[{"id":"live","role":"assistant","text":"tail","at":3}],"truncated":true,"nextCursor":"old","historyVersion":"v1"})).unwrap();
        apply_history(&mut page,&source,&json!({"around":"anchor"}),&json!({"items":[{"id":"anchor","role":"user","text":"old","at":1},{"id":"near","role":"assistant","text":"near","at":2}],"truncated":true,"nextCursor":"opaque-before","nextAfterCursor":"opaque-after","historyVersion":"v1"})).unwrap();
        assert!(page.historical);
        assert_eq!(page.anchor.as_deref(), Some("anchor"));
        assert_eq!(page.before, "opaque-before");
        assert_eq!(page.after, "opaque-after");
        apply_live(&mut page,&source,&json!({"items":[{"id":"anchor","role":"user","text":"updated","at":1},{"id":"newest","role":"assistant","text":"new","at":4}],"truncated":true,"historyVersion":"v1"})).unwrap();
        assert_eq!(
            page.items
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["anchor", "near"]
        );
        assert_eq!(page.items[0]["text"], "updated");
        let latest = page.latest.clone().unwrap();
        page.historical = false;
        page.items.clear();
        apply_live(&mut page, &source, &latest).unwrap();
        assert_eq!(page.items.last().unwrap()["id"], "newest");
        assert!(page.after.is_null());
    }

    #[test]
    fn transcript_view_refuses_changed_versions_and_unproven_live_overlap() {
        let source = source();
        let mut page = Page::default();
        apply_live(&mut page,&source,&json!({"items":[{"id":"old","role":"assistant","text":"old","at":1},{"id":"pending","role":"user","text":"pending","at":2,"pending":true,"materialized":false}],"truncated":true,"nextCursor":"old-cursor","historyVersion":"v1"})).unwrap();
        let old = page.items.clone();
        assert!(
            apply_history(
                &mut page,
                &source,
                &json!({"before":"old"}),
                &json!({"items":[],"truncated":false,"historyVersion":"v2"})
            )
            .is_err()
        );
        assert_eq!(page.items, old);
        apply_live(&mut page,&source,&json!({"items":[{"id":"pending","role":"user","text":"pending","at":2,"pending":true,"materialized":false},{"id":"fresh","role":"assistant","text":"fresh","at":3}],"truncated":true,"nextCursor":"fresh-cursor","historyVersion":"v1"})).unwrap();
        assert!(!page.items.iter().any(|row| row["id"] == "old"));
        assert_eq!(page.before, "fresh-cursor");
    }

    #[test]
    fn transcript_view_cache_keeps_the_active_selection_within_shared_bounds() {
        let (output, _) = watch::channel(Value::Null);
        let active = source();
        let key = active.key();
        let mut view = View {
            id: "view".into(),
            generation: 1,
            operation: 0,
            revision: 0,
            source: Some(active),
            pages: BTreeMap::new(),
            lru: VecDeque::new(),
            output,
            stream: None,
            closed: false,
        };
        view.pages.insert(key.clone(), Page::default());
        view.lru.push_back(key.clone());
        for index in 0..MAX_TRANSCRIPT_CACHE_SCOPES {
            let name = format!("other-{index}");
            view.pages.insert(name.clone(), Page::default());
            view.lru.push_back(name);
        }
        view.publish();
        assert_eq!(view.pages.len(), MAX_TRANSCRIPT_CACHE_SCOPES);
        assert!(view.pages.contains_key(&key));
        assert!(!view.pages.contains_key("other-0"));
    }

    #[tokio::test]
    async fn transcript_view_uses_committed_stream_and_real_history_read() {
        let workspace = "a".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let hash = studio_runtime_schema::api_schema::schema_hash(
            &serde_json::from_slice(include_bytes!(
                "../../../../runtime/packages/runtime-http/openapi.json"
            ))
            .unwrap(),
        );
        let resource = json!({"kind":"transcript","agentId":"lead","tail":40});
        let frame = json!({"protocol":4,"workspaceId":workspace,"epoch":"first","revision":1,"reason":"initial","resources":[resource],"resourceVersions":[{"resource":resource,"revision":1,"cursor":{"workspaceId":workspace,"seq":1},"replyKind":"snapshot","transcriptSnapshot":{"items":[{"id":"live","role":"assistant","text":"new","at":2}],"truncated":true,"nextCursor":"older-live","historyVersion":"version-one"}}]});
        validate("ResourceChangeEvent", &frame).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut stream).await;
            assert!(headers.starts_with("GET /api/sync/stream?"), "{headers}");
            let body = format!(
                "event: session\ndata: {{\"sessionId\":\"00000000-0000-0000-0000-000000000001\"}}\n\nevent: api-schema\ndata: {{\"hash\":\"{hash}\"}}\n\nevent: resources\ndata: {frame}\n\n"
            );
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
            stream
                .write_all(format!("{:x}\r\n{body}\r\n", body.len()).as_bytes())
                .await
                .unwrap();
            let (mut page, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut page).await;
            assert!(
                headers.starts_with("GET /api/transcript/page?"),
                "{headers}"
            );
            assert!(headers.contains("before=older-live"), "{headers}");
            let body = json!({"items":[{"id":"older","role":"user","text":"saved","at":1}],"truncated":true,"nextCursor":"before-older","historyVersion":"version-one"}).to_string();
            page.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Studio-API-Schema: {hash}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            std::future::pending::<()>().await;
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut database = crate::store::Store::open(&path).unwrap();
        let (store, mut commands) = mpsc::channel::<StoreCommand>(16);
        let worker = tokio::spawn(async move {
            while let Some((request, receipt)) = commands.recv().await {
                let _ = receipt.send(database.dispatch(request));
            }
        });
        state(&store, "identity.set", json!({"workspaceId":workspace}))
            .await
            .unwrap();
        let owner = TranscriptViews::new(Arc::new(Ok(backend)), store.clone());
        let opened = owner
            .dispatch(Request {
                id: "open".into(),
                method: "transcript_view.open".into(),
                params: json!({}),
            })
            .await;
        assert!(opened.error.is_none(), "{:?}", opened.error);
        let view = opened.result.unwrap()["viewId"].clone();
        let (output, mut events) = mpsc::channel(16);
        let watching = owner.clone();
        let watch_view = view.clone();
        let watcher = tokio::spawn(async move {
            watching
                .run("watch".into(), json!({"viewId":watch_view}), output)
                .await;
        });
        let selected = owner.dispatch(Request{id:"select".into(),method:"transcript_view.select".into(),params:json!({"viewId":view,"generation":1,"workspace":workspace,"id":"lead","kind":"agent","managed":true,"enabled":true})}).await;
        assert!(selected.error.is_none(), "{:?}", selected.error);
        let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                assert!(event["error"].is_null(), "{event}");
                assert!(
                    event["value"]["notice"].as_str().unwrap_or("").is_empty(),
                    "{event}"
                );
                if event["value"]["loaded"] == true {
                    break event["value"].clone();
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(ready["items"][0]["id"], "live");
        let committed = state(
            &store,
            "projection.readSubscription",
            json!({"workspace":workspace,"resource":resource}),
        )
        .await
        .unwrap();
        assert_eq!(committed["data"]["items"][0]["id"], "live");
        let older = owner
            .dispatch(Request {
                id: "older".into(),
                method: "transcript_view.older".into(),
                params: json!({"viewId":view,"generation":1}),
            })
            .await;
        assert!(older.error.is_none(), "{:?}", older.error);
        let older = older.result.unwrap();
        assert_eq!(
            older["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["older", "live"]
        );
        assert_eq!(older["before"], "before-older");
        owner
            .dispatch(Request {
                id: "close".into(),
                method: "transcript_view.close".into(),
                params: json!({"viewId":view}),
            })
            .await;
        watcher.abort();
        let _ = watcher.await;
        server.abort();
        let _ = server.await;
        drop(owner);
        drop(store);
        worker.await.unwrap();
    }
}

impl TranscriptViews {
    pub fn new(backend: Arc<Result<Backend, String>>, store: StoreSender) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                store,
                views: Mutex::new(HashMap::new()),
            }),
        }
    }
    fn view(&self, id: &str) -> Result<Arc<Mutex<View>>, String> {
        self.inner
            .views
            .lock()
            .map_err(|_| "The conversation view owner failed.")?
            .get(id)
            .cloned()
            .ok_or_else(|| {
                "The conversation view is closed or belongs to another native context.".into()
            })
    }
    pub async fn dispatch(&self, request: Request) -> Response {
        let result = self.command(&request.method, &request.params).await;
        match result {
            Ok(value) => Response::success(request.id, value),
            Err(message) => Response::failure(request.id, "transcript_view_error", message),
        }
    }
    async fn command(&self, method: &str, params: &Value) -> Result<Value, String> {
        if method == "transcript_view.open" {
            let mut views = self
                .inner
                .views
                .lock()
                .map_err(|_| "The conversation view owner failed.")?;
            if views.len() >= MAX_OPEN_TRANSCRIPT_VIEWS {
                return Err("The native conversation view limit was reached.".into());
            }
            let id = crate::server_keys::random_id()?;
            let (output, _) = watch::channel(Value::Null);
            let mut view = View {
                id: id.clone(),
                generation: 0,
                operation: 0,
                revision: 0,
                source: None,
                pages: BTreeMap::new(),
                lru: VecDeque::new(),
                output,
                stream: None,
                closed: false,
            };
            view.publish();
            views.insert(id.clone(), Arc::new(Mutex::new(view)));
            return Ok(json!({"viewId":id}));
        }
        let id = params["viewId"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 128)
            .ok_or("A conversation view identity is required.")?;
        let handle = self.view(id)?;
        if method == "transcript_view.close" {
            {
                let mut view = handle
                    .lock()
                    .map_err(|_| "The conversation view owner failed.")?;
                view.closed = true;
                if let Some(task) = view.stream.take() {
                    task.abort();
                }
                view.publish();
            }
            self.inner
                .views
                .lock()
                .map_err(|_| "The conversation view owner failed.")?
                .remove(id);
            return Ok(json!({"closed":true}));
        }
        let generation = params["generation"]
            .as_u64()
            .filter(|n| *n > 0 && *n <= MAX_SAFE_INTEGER as u64)
            .ok_or("A valid conversation selection generation is required.")?;
        if method == "transcript_view.select" {
            let source = Source::parse(params)?;
            let identity = state(&self.inner.store, "identity.get", Value::Null).await?;
            if identity["workspaceId"] != source.workspace {
                return Err("The conversation belongs to another workspace.".into());
            }
            {
                let mut view = handle
                    .lock()
                    .map_err(|_| "The conversation view owner failed.")?;
                if generation <= view.generation {
                    return Ok(view.snapshot());
                }
                if let Some(task) = view.stream.take() {
                    task.abort();
                }
                view.generation = generation;
                view.operation += 1;
                view.source = Some(source.clone());
                let key = source.key();
                view.lru.retain(|value| value != &key);
                view.lru.push_back(key.clone());
                view.pages.entry(key).or_default().loading = false;
                view.publish();
            }
            if source.id.is_some() {
                let cached = state(
                    &self.inner.store,
                    "projection.readSubscription",
                    json!({"workspace":source.workspace,"resource":source.resource()}),
                )
                .await?;
                let mut view = handle
                    .lock()
                    .map_err(|_| "The conversation view owner failed.")?;
                if view.generation == generation && cached["status"] == "ready" {
                    apply_live(view.page_mut(), &source, &cached["data"])?;
                    view.publish();
                }
            }
            if source.id.is_some() && source.enabled {
                self.start_stream(id.to_owned(), generation, source)?;
            }
            return Ok(handle
                .lock()
                .map_err(|_| "The conversation view owner failed.")?
                .snapshot());
        }
        let (source, operation, query) = {
            let mut view = handle
                .lock()
                .map_err(|_| "The conversation view owner failed.")?;
            if view.generation != generation || view.closed {
                return Ok(view.snapshot());
            }
            let source = view
                .source
                .clone()
                .ok_or("The conversation is not selected.")?;
            if source.id.is_none() {
                return Ok(view.snapshot());
            }
            if method == "transcript_view.anchor" {
                let anchor = match &params["anchor"] {
                    Value::Null => None,
                    Value::String(value) if value.len() <= 1024 => Some(value.clone()),
                    _ => return Err("Invalid conversation anchor.".into()),
                };
                view.page_mut().anchor = anchor;
                return Ok(view.snapshot());
            }
            if method == "transcript_view.ensure"
                && view.page().unwrap().items.iter().any(|item| {
                    item["id"] == params["messageId"] || item["sourceId"] == params["messageId"]
                })
            {
                let mut result = view.snapshot();
                result["found"] = json!(true);
                return Ok(result);
            }
            if method == "transcript_view.ensure" && (source.kind != "agent" || !source.managed) {
                let mut result = view.snapshot();
                result["found"] = json!(false);
                return Ok(result);
            }
            if method == "transcript_view.latest" && source.kind == "agent" {
                let latest = view.page().unwrap().latest.clone();
                if !view.page().unwrap().historical {
                    return Ok(view.snapshot());
                }
                view.operation += 1;
                let page = view.page_mut();
                page.historical = false;
                page.after = Value::Null;
                page.items.clear();
                page.loading = false;
                if let Some(latest) = latest {
                    apply_live(page, &source, &latest)?;
                    view.publish();
                    return Ok(view.snapshot());
                }
            }
            let page = view.page().unwrap();
            let query = match method {
                "transcript_view.older"
                    if !page.before.is_null() && (source.kind != "agent" || source.managed) =>
                {
                    json!({"before":page.before})
                }
                "transcript_view.newer" if !page.after.is_null() => json!({"after":page.after}),
                "transcript_view.ensure" => {
                    let message = params["messageId"]
                        .as_str()
                        .filter(|v| !v.is_empty() && v.len() <= 1024)
                        .ok_or("Invalid conversation message identity.")?;
                    json!({"around":message})
                }
                "transcript_view.refresh" | "transcript_view.latest" => Value::Null,
                "transcript_view.older" | "transcript_view.newer" => return Ok(view.snapshot()),
                _ => return Err("Unknown conversation view command.".into()),
            };
            if page.loading && method != "transcript_view.ensure" && !query.is_null() {
                return Ok(view.snapshot());
            }
            view.operation += 1;
            view.page_mut().loading = true;
            view.publish();
            (source, view.operation, query)
        };
        let backend = self.inner.backend.as_ref().as_ref().map_err(Clone::clone)?;
        let loaded: Result<Value, String> = async {
            if query.is_null() {
                let response = crate::subscriptions::dispatch(
                    backend,
                    &self.inner.store,
                    Request {
                        id: "transcript-view-refresh".into(),
                        method: "subscriptions.snapshot".into(),
                        params: json!({"workspace":source.workspace,"resource":source.resource()}),
                    },
                )
                .await;
                let snapshot = response.result.ok_or_else(|| {
                    response
                        .error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "The conversation refresh is unconfirmed.".into())
                })?;
                Ok(snapshot["data"].clone())
            } else {
                read_history(backend, &source, &query).await
            }
        }
        .await;
        let mut view = handle
            .lock()
            .map_err(|_| "The conversation view owner failed.")?;
        if view.generation != generation || view.operation != operation || view.closed {
            return Ok(view.snapshot());
        }
        let loaded = match loaded {
            Ok(value) => value,
            Err(error) => {
                let page = view.page_mut();
                page.notice = error.clone();
                page.loading = false;
                view.publish();
                return Err(error);
            }
        };
        if query.is_null() {
            if method == "transcript_view.latest" {
                let page = view.page_mut();
                page.items.clear();
                page.historical = false;
                page.after = Value::Null;
            }
            apply_live(view.page_mut(), &source, &loaded)?;
        } else {
            apply_history(view.page_mut(), &source, &query, &loaded)?;
        }
        view.page_mut().loading = false;
        view.publish();
        let mut result = view.snapshot();
        if method == "transcript_view.ensure" {
            result["found"] = json!(
                view.page()
                    .unwrap()
                    .items
                    .iter()
                    .any(|row| row["id"] == params["messageId"]
                        || row["sourceId"] == params["messageId"])
            );
        }
        Ok(result)
    }
    fn start_stream(&self, id: String, generation: u64, source: Source) -> Result<(), String> {
        let handle = self.view(&id)?;
        let mut view = handle
            .lock()
            .map_err(|_| "The conversation view owner failed.")?;
        if view.generation != generation || view.closed {
            return Ok(());
        }
        let weak = Arc::downgrade(&self.inner);
        let backend = self.inner.backend.clone();
        let store = self.inner.store.clone();
        let task = tokio::spawn(async move {
            let (output, mut events) = mpsc::channel(8);
            let stream = tokio::spawn(crate::subscriptions::run(
                id.clone(),
                json!({"workspace":source.workspace,"resources":[source.resource()],"tokenRates":false}),
                backend,
                store,
                output,
            ));
            struct Cancel(AbortHandle);
            impl Drop for Cancel {
                fn drop(&mut self) {
                    self.0.abort();
                }
            }
            let _cancel = Cancel(stream.abort_handle());
            while let Some(event) = events.recv().await {
                let Some(inner) = Weak::upgrade(&weak) else {
                    break;
                };
                let Ok(views) = inner.views.lock() else {
                    break;
                };
                let Some(view) = views.get(&id).cloned() else {
                    break;
                };
                drop(views);
                let Ok(mut view) = view.lock() else {
                    break;
                };
                if view.generation != generation || view.closed {
                    break;
                }
                let page = view.page_mut();
                if let Some(error) = event.get("error") {
                    page.notice = error["message"]
                        .as_str()
                        .unwrap_or("Native conversation updates failed.")
                        .into();
                    page.connection = "reconnecting".into();
                }
                let value = &event["value"];
                if value["type"] == "resource" && value["snapshot"]["resource"] == source.resource()
                {
                    if value["snapshot"]["status"] == "ready" {
                        if let Err(error) = apply_live(page, &source, &value["snapshot"]["data"]) {
                            page.notice = error;
                        }
                    } else {
                        page.notice = "The conversation update is not confirmed.".into();
                    }
                } else if value["type"] == "connection" && source.kind == "agent" && source.managed
                {
                    page.connection = match value["status"].as_str() {
                        Some("live") => "live",
                        Some("offline" | "degraded") => "reconnecting",
                        _ => "",
                    }
                    .into();
                }
                view.publish();
            }
        });
        view.stream = Some(task.abort_handle());
        Ok(())
    }
    pub async fn run(&self, subscription: String, params: Value, output: mpsc::Sender<Value>) {
        let result: Result<(), String> = async {
            let view = self.view(
                params["viewId"]
                    .as_str()
                    .ok_or("A conversation view identity is required.")?,
            )?;
            let mut changes = view
                .lock()
                .map_err(|_| "The conversation view owner failed.")?
                .output
                .subscribe();
            loop {
                let value = changes.borrow_and_update().clone();
                let closed = value["closed"] == true;
                output
                    .send(json!({"event":subscription,"value":value}))
                    .await
                    .map_err(|_| "The conversation screen closed.")?;
                if closed || changes.changed().await.is_err() {
                    break;
                }
            }
            Ok(())
        }
        .await;
        if let Err(message) = result {
            let _=output.send(json!({"event":subscription,"error":{"code":"transcript_view_error","message":message}})).await;
        }
    }
}

fn validate(schema: &str, value: &Value) -> Result<(), String> {
    match first_error(schema, value).map_err(|error| error.to_string())? {
        Some((path, message)) => Err(format!("Invalid {schema} at {path}: {message}")),
        None => Ok(()),
    }
}
fn memory_bytes(value: &Value) -> usize {
    serde_json::to_string(value)
        .map(|text| text.encode_utf16().count().saturating_mul(2))
        .unwrap_or(usize::MAX)
}
fn stamp(value: &Value, room: bool) -> f64 {
    let value = if room {
        &value["seq"]
    } else {
        value
            .get("at")
            .filter(|v| !v.is_null())
            .unwrap_or(&value["created"])
    };
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0.0)
}
fn merge(earlier: &[Value], later: &[Value], room: bool) -> Vec<Value> {
    let mut positions = HashMap::new();
    let mut rows = Vec::new();
    for row in earlier.iter().chain(later) {
        let id = row["id"].as_str().unwrap_or("");
        if let Some(index) = positions.get(id).copied() {
            rows[index] = row.clone();
        } else {
            positions.insert(id.to_owned(), rows.len());
            rows.push(row.clone());
        }
    }
    rows.sort_by(|a, b| {
        stamp(a, room)
            .partial_cmp(&stamp(b, room))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}
fn bound(items: &[Value], older: bool, anchor: Option<&str>) -> (Vec<Value>, bool, bool) {
    let anchor = anchor.and_then(|anchor| {
        items
            .iter()
            .position(|row| row["id"] == anchor || row["sourceId"] == anchor)
    });
    let start = anchor
        .map(|index| {
            index
                .saturating_sub(MAX_TRANSCRIPT_PAGE_ITEMS / 2)
                .min(items.len().saturating_sub(MAX_TRANSCRIPT_PAGE_ITEMS))
        })
        .unwrap_or(if older {
            0
        } else {
            items.len().saturating_sub(MAX_TRANSCRIPT_PAGE_ITEMS)
        });
    let mut selected = Vec::new();
    let mut bytes = 0usize;
    let indexes: Box<dyn Iterator<Item = usize>> = if anchor.is_some() || older {
        Box::new(start..items.len())
    } else {
        Box::new((0..items.len()).rev())
    };
    for index in indexes {
        let size = memory_bytes(&items[index]);
        if selected.len() >= MAX_TRANSCRIPT_PAGE_ITEMS
            || bytes.saturating_add(size) > MAX_TRANSCRIPT_PAGE_MEMORY_BYTES
        {
            break;
        }
        bytes += size;
        selected.push(index);
    }
    selected.sort_unstable();
    let before = selected.first().is_some_and(|index| *index > 0);
    let after = selected
        .last()
        .map(|index| index + 1 < items.len())
        .unwrap_or(!items.is_empty());
    (
        selected
            .into_iter()
            .map(|index| items[index].clone())
            .collect(),
        before,
        after,
    )
}
fn cursor(row: Option<&Value>, room: bool) -> Value {
    row.map(|row| {
        if room {
            row["seq"].clone()
        } else {
            row["id"].clone()
        }
    })
    .unwrap_or(Value::Null)
}
fn render_rows(source: &Source, data: &Value) -> Result<Vec<Value>, String> {
    if source.kind == "agent" {
        validate("TranscriptPageResponse", data)?;
        if data["agent"]["id"]
            .as_str()
            .is_some_and(|id| Some(id) != source.id.as_deref())
        {
            return Err("The transcript belongs to another agent.".into());
        }
        return Ok(transcript_messages(
            data["items"]
                .as_array()
                .ok_or("Missing transcript messages.")?,
            source.id.as_deref().unwrap_or(""),
        ));
    }
    if source.kind == "legacy" {
        validate("MessageHistory", data)?;
        return Ok(data.as_array().unwrap().iter().map(|row|json!({"id":row["id"],"text":row["text"],"at":row["at"],"role":if row["author"] == "user" {"user"}else{"assistant"},"deliveryStatus":row["status"],"deliveryError":row["error"]})).collect());
    }
    validate("ChatRead", data)?;
    if data["room"]["id"] != source.id.clone().unwrap_or_default() {
        return Err("The room page belongs to another room.".into());
    }
    Ok(data["messages"].as_array().unwrap().iter().map(|row|json!({"id":row["id"],"text":row["text"],"role":"assistant","kind":"message","at":row["created"],"created":row["created"],"seq":row["seq"],"sender":row["sender"],"senderName":row["senderName"],"sourceId":row["sender"]})).collect())
}
fn apply_live(page: &mut Page, source: &Source, data: &Value) -> Result<(), String> {
    let live = render_rows(source, data)?;
    let version = data["historyVersion"].as_str().unwrap_or("");
    let room = source.kind == "room";
    let unavailable = data["unavailable"].as_str().filter(|text| !text.is_empty());
    if unavailable.is_some()
        || (!page.version.is_empty() && !version.is_empty() && page.version != version)
    {
        page.items.clear();
        page.historical = false;
        page.before = Value::Null;
        page.after = Value::Null;
    }
    if page.historical {
        let present: BTreeSet<_> = page.items.iter().filter_map(|v| v["id"].as_str()).collect();
        let changed: Vec<_> = live
            .iter()
            .filter(|v| v["id"].as_str().is_some_and(|id| present.contains(id)))
            .cloned()
            .collect();
        page.items = merge(&page.items, &changed, room);
    } else if source.kind == "agent" && data["truncated"] == true {
        let present: BTreeSet<_> = live
            .iter()
            .filter(|row| row["pending"] != true && row["materialized"] != false)
            .filter_map(|row| row["id"].as_str())
            .collect();
        let overlap = page.items.iter().any(|row| {
            row["pending"] != true
                && row["materialized"] != false
                && row["id"].as_str().is_some_and(|id| present.contains(id))
        });
        if overlap {
            let first = live
                .iter()
                .map(|row| stamp(row, false))
                .fold(f64::INFINITY, f64::min);
            let prior: Vec<_> = page
                .items
                .iter()
                .filter(|row| {
                    present.contains(row["id"].as_str().unwrap_or(""))
                        || (row["pending"] != true
                            && row["materialized"] != false
                            && stamp(row, false) < first)
                })
                .cloned()
                .collect();
            page.items = merge(&prior, &live, false);
        } else {
            page.items = live;
            page.before = data["nextCursor"].clone();
            if page.before.is_null() {
                page.before = cursor(page.items.first(), false);
            }
        }
    } else if room && !page.items.is_empty() {
        let present: BTreeSet<_> = live.iter().filter_map(|row| row["id"].as_str()).collect();
        let overlap = page
            .items
            .iter()
            .any(|row| row["id"].as_str().is_some_and(|id| present.contains(id)));
        page.items = if overlap {
            merge(&page.items, &live, true)
        } else {
            live
        };
        if !overlap {
            page.before = data["nextBefore"].clone();
        }
    } else {
        page.items = live;
        page.before = if room {
            data["nextBefore"].clone()
        } else {
            Value::Null
        };
    }
    let (items, dropped_before, dropped_after) = bound(
        &page.items,
        page.historical,
        page.anchor.as_deref().filter(|_| page.historical),
    );
    page.items = items;
    if dropped_before {
        page.before = cursor(page.items.first(), room);
    }
    if dropped_after && page.historical {
        page.after = cursor(page.items.last(), room);
    }
    if !page.historical {
        page.after = Value::Null;
    }
    page.loaded = true;
    page.agent = data["agent"].clone();
    page.latest = Some(data.clone());
    page.version = version.into();
    page.notice = unavailable
        .unwrap_or(if data["truncated"] == true && !source.managed {
            "Recent messages only."
        } else {
            ""
        })
        .into();
    Ok(())
}
fn apply_history(
    page: &mut Page,
    source: &Source,
    query: &Value,
    data: &Value,
) -> Result<(), String> {
    let rows = render_rows(source, data)?;
    let version = data["historyVersion"].as_str().unwrap_or("");
    if !page.version.is_empty() && !version.is_empty() && page.version != version {
        return Err("The conversation changed. Search or load its history again.".into());
    }
    let room = source.kind == "room";
    let older = query.get("before").is_some();
    let around = query["around"].as_str();
    let all = if around.is_some() {
        rows
    } else if older {
        merge(&rows, &page.items, room)
    } else {
        merge(&page.items, &rows, room)
    };
    let anchor = around.or(page.anchor.as_deref());
    let (items, dropped_before, dropped_after) = bound(
        &all,
        older,
        if !older && around.is_none() {
            None
        } else {
            anchor
        },
    );
    let before = if room {
        data["nextBefore"].clone()
    } else {
        data["nextCursor"].clone()
    };
    let after = if room {
        data["nextAfter"].clone()
    } else {
        data["nextAfterCursor"].clone()
    };
    if older || around.is_some() {
        page.before = before;
    }
    if dropped_before || (!room && !older && around.is_none() && !page.before.is_null()) {
        page.before = cursor(items.first(), room);
    }
    page.after = if older {
        if dropped_after || (!room && !page.after.is_null()) {
            cursor(items.last(), room)
        } else {
            Value::Null
        }
    } else if dropped_after || (!room && around.is_none() && !after.is_null()) {
        cursor(items.last(), room)
    } else {
        after
    };
    page.items = items;
    page.loaded = true;
    page.notice.clear();
    page.historical |= around.is_some();
    page.version = version.into();
    if let Some(anchor) = around {
        page.anchor = Some(anchor.into());
    }
    Ok(())
}
async fn read_history(backend: &Backend, source: &Source, query: &Value) -> Result<Value, String> {
    let mut query = query.clone();
    query[if source.kind == "agent" { "id" } else { "room" }] = json!(source.id);
    query["limit"] = json!(if source.kind == "agent" {
        SYNC_TRANSCRIPT_INITIAL_TAIL_ITEMS
    } else {
        100
    });
    let path = if source.kind == "agent" {
        "/api/transcript/page"
    } else if source.kind == "room" {
        "/api/agent-chat"
    } else {
        return Err("Legacy conversations do not expose history cursors.".into());
    };
    let response=backend.dispatch(Request{id:"transcript-view-history".into(),method:"api.request".into(),params:json!({"path":path,"query":query,"workspaceId":source.workspace,"apiSchema":crate::subscriptions::schema_hash(),"jsonResponse":true})}).await;
    let response = response.result.ok_or_else(|| {
        response
            .error
            .map(|error| error.message)
            .unwrap_or_else(|| "The conversation read returned no result.".into())
    })?;
    let status = response["status"]
        .as_u64()
        .ok_or("The native conversation HTTP response is invalid.")?;
    if !(200..300).contains(&status) {
        return Err(response["body"]["error"]
            .as_str()
            .unwrap_or("The server refused the conversation history read.")
            .into());
    }
    if response["headers"]["x-studio-api-schema-mismatch"] == "1"
        || response["headers"]["x-studio-api-schema"]
            .as_str()
            .is_some_and(|hash| hash != crate::subscriptions::schema_hash())
    {
        return Err("Studio must be updated before reading this conversation.".into());
    }
    let body = response["body"].clone();
    render_rows(source, &body)?;
    Ok(body)
}
fn transcript_messages(rows: &[Value], id: &str) -> Vec<Value> {
    let mut result = Vec::new();
    for row in rows {
        let Some(inputs) = row["inputs"].as_array() else {
            let mut value = row.clone();
            if value["threadId"].is_null() {
                value.as_object_mut().unwrap().remove("threadId");
            }
            result.push(value);
            continue;
        };
        for (index, input) in inputs.iter().enumerate() {
            let mut value = row.clone();
            let fields = value.as_object_mut().unwrap();
            fields.insert(
                "id".into(),
                json!(
                    input["id"]
                        .as_str()
                        .filter(|id| !id.is_empty())
                        .map(|input| format!("{id}:{input}"))
                        .unwrap_or_else(|| format!("{}:{index}", row["id"].as_str().unwrap_or("")))
                ),
            );
            fields.insert("sourceId".into(), row["id"].clone());
            for key in [
                "deliveryStatus",
                "requestedDelivery",
                "deliveryError",
                "pending",
                "truncated",
            ] {
                if let Some(value) = input
                    .get(key)
                    .filter(|value| key != "pending" || !value.is_null())
                {
                    fields.insert(key.into(), value.clone());
                } else {
                    fields.remove(key);
                }
            }
            for key in ["clientMessageId", "materialized"] {
                let value = if key == "clientMessageId" {
                    input
                        .get(key)
                        .filter(|value| value.as_str().is_some_and(|v| !v.is_empty()))
                        .or_else(|| {
                            input
                                .get("id")
                                .filter(|v| v.as_str().is_some_and(|v| !v.is_empty()))
                        })
                        .or_else(|| if index == 0 { row.get(key) } else { None })
                } else {
                    input
                        .get(key)
                        .filter(|v| !v.is_null())
                        .or_else(|| row.get(key))
                };
                if let Some(value) = value {
                    fields.insert(key.into(), value.clone());
                } else {
                    fields.remove(key);
                }
            }
            fields.insert(
                "assets".into(),
                input
                    .get("assets")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or_else(|| {
                        if index == 0 {
                            row.get("assets").cloned().unwrap_or_else(|| json!([]))
                        } else {
                            json!([])
                        }
                    }),
            );
            fields.insert(
                "role".into(),
                json!(if input["kind"] == "user" {
                    "user"
                } else {
                    "tool"
                }),
            );
            fields.insert("text".into(), input["text"].clone());
            let kind = input["kind"].as_str().unwrap_or("");
            let event = input["text"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok());
            let sender = event
                .as_ref()
                .and_then(|event| {
                    event["sender_name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .or_else(|| event["name"].as_str())
                })
                .map(str::trim)
                .filter(|name| !name.is_empty());
            let title = match (kind, sender) {
                ("user", _) => "You".into(),
                ("agent_message", Some(sender)) => format!("Message from {sender}"),
                ("child_result", Some(sender)) => format!("Result from {sender}"),
                _ => match kind {
                    "agent_message" => "Agent message received",
                    "child_result" => "Worker result received",
                    "monitor_exit" => "Command finished",
                    "monitor_cancelled" => "Command cancelled",
                    "complaint_response" => "Complaint response received",
                    "followup" => "Agent follow-up",
                    "complaint" => "Complaint requires a response",
                    "work_released" => "Task released from a failed worker",
                    _ => "Team activity",
                }
                .into(),
            };
            fields.insert("title".into(), json!(title));
            if fields.get("threadId").is_some_and(Value::is_null) {
                fields.remove("threadId");
            }
            result.push(value);
        }
    }
    result
}
