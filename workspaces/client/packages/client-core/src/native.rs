//! C entry point for the Rust library packaged with the Attar UI.
use crate::{
    backend::Backend,
    protocol::{self, Request, Response},
    store::Store,
};
use std::{
    collections::HashSet,
    env,
    fs::{self, File, OpenOptions},
    io,
    os::{
        fd::AsRawFd,
        unix::{fs::PermissionsExt, net::UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Only the native host supplies this configuration. It never comes from IPC.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConnectionConfig {
    pub state_directory: PathBuf,
    pub backend_origin: String,
}

fn open_store(state: &std::path::Path, migration_held: bool) -> Result<(Store, File), String> {
    if !state.is_absolute() {
        return Err("The client state directory must be absolute.".into());
    }
    if !state.exists() {
        fs::create_dir_all(state).map_err(|e| e.to_string())?;
        fs::set_permissions(state, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    // This lock belongs to the native client, not the Python server.
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(state.join("studio-native-client.lock"))
        .map_err(|e| e.to_string())?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("Another native client owns this local client state.".into());
    }
    Ok((
        if migration_held {
            Store::open_for_migration(&state.join("studio-client.sqlite3"))
        } else {
            Store::open(&state.join("studio-client.sqlite3"))
        }
        .map_err(|e| e.to_string())?,
        lock,
    ))
}

fn serve(channel: UnixStream, config: ConnectionConfig) -> Result<(), String> {
    let backend = Backend::new(&config.backend_origin);
    serve_backend(channel, config.state_directory, backend, false)
}

fn serve_backend(
    channel: UnixStream,
    state_directory: PathBuf,
    backend: Result<Backend, String>,
    migration_held: bool,
) -> Result<(), String> {
    let (changes, changed) = tokio::sync::watch::channel(0u64);
    let (store_sender, mut store_receiver) =
        tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
    let store_worker = thread::Builder::new()
        .name("studio-state".into())
        .spawn(move || {
            let mut state = open_store(&state_directory, migration_held);
            while let Some((request, reply)) = store_receiver.blocking_recv() {
                let mutation = matches!(
                    request.method.as_str(),
                    "drafts.import"
                        | "drafts.edit"
                        | "drafts.dismiss"
                        | "drafts.receive"
                        | "drafts.acknowledge"
                        | "drafts.sync_status"
                        | "draft.set"
                        | "preferences.set"
                        | "preferences.import"
                        | "preferences.raw_set"
                        | "preferences.raw_remove"
                        | "preferences.raw_compare_exchange"
                        | "preferences.edit"
                        | "preferences.adopt_fields"
                        | "preference_sync.network_ready"
                        | "preference_sync.ack"
                        | "preference_sync.failure"
                        | "uploads.queue"
                        | "uploads.cancel"
                        | "uploads.finish"
                        | "uploads.receipt"
                        | "identity.set"
                        | "projection.put"
                        | "projection.applyEntities"
                        | "projection.applySubscription"
                        | "projection.stageSubscriptionFragment"
                        | "outbox.enqueue"
                        | "outbox.change"
                        | "outbox.stop_removed"
                        | "outbox.display"
                        | "outbox.acknowledge"
                        | "outbox.acknowledge_many"
                        | "outbox.claim"
                        | "outbox.receipt"
                        | "outbox.failure"
                        | "dictation.save"
                        | "dictation.chunk"
                        | "dictation.delete"
                        | "dictation.update"
                        | "dictation.begin"
                        | "dictation.trash"
                        | "dictation.restore"
                        | "files.importCommit"
                        | "dictation.capturePrepare"
                        | "dictation.captureAppend"
                        | "dictation.captureFinish"
                        | "dictation.claimTranscription"
                        | "dictation.settleTranscription"
                );
                let response = match &mut state {
                    Ok((store, _lock)) => store.dispatch(request),
                    Err(error) => Response::failure(request.id, "client_state_unavailable", error),
                };
                if mutation && response.error.is_none() {
                    changes.send_modify(|version| *version = version.wrapping_add(1));
                }
                let _ = reply.send(response);
            }
        })
        .map_err(|error| error.to_string())?;
    // Electron verifies/starts the backend before opening the client. Rust must
    // never race that owner by launching a second backend.
    let backend = Arc::new(backend);
    channel.set_nonblocking(true).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("studio-io")
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let result = runtime.block_on(async move {
        let channel = tokio::net::UnixStream::from_std(channel).map_err(|e| e.to_string())?;
        let (mut reader, writer) = channel.into_split();
        let writer = Arc::new(tokio::sync::Mutex::new(writer));
        let permits = Arc::new(tokio::sync::Semaphore::new(64));
        let mut requests = tokio::task::JoinSet::new();
        let startup = Arc::new(tokio::sync::Mutex::new(None::<serde_json::Value>));
        let active = Arc::new(AtomicBool::new(false));
        let migration_held = Arc::new(AtomicBool::new(migration_held));
        let previews = crate::file_preview::Previews::default();
        let terminals = crate::terminal::Terminals::default();
        let mut background = tokio::task::JoinSet::new();
        let (events, mut event_receiver) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
        let transcript_views = crate::transcript_views::TranscriptViews::new(backend.clone(),store_sender.clone());
        let mut views = crate::views::Views::new(
            backend.clone(),
            store_sender.clone(),
            changed,
            events,
            terminals.clone(),
            transcript_views.clone(),
        );
        let event_writer = writer.clone();
        background.spawn(async move {
            while let Some(event) = event_receiver.recv().await {
                if write_packet(&event_writer, event).await.is_err() {
                    break;
                }
            }
        });
        let (queue_backend, queue_store, queue_active) =
            (backend.clone(), store_sender.clone(), active.clone());
        let queue_held=migration_held.clone();
        background.spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if !queue_held.load(Ordering::Acquire) && queue_active.load(Ordering::Relaxed)
                    && let Ok(backend) = queue_backend.as_ref() {
                        crate::delivery::drain(backend, &queue_store).await;
                    }
            }
        });
        let mut assembler = protocol::Assembler::default();
        // Audio adapters share a process-wide device. A new request fences any
        // older completion before that completion can release the device.
        let audio = Arc::new(tokio::sync::Mutex::new(AudioOwnership::default()));
        let mut playback_changes=crate::media::watch_playback_owner(&store_sender)?;
        let playback_audio=audio.clone();
        background.spawn(async move {
            loop {
                let receipt={playback_changes.borrow_and_update().clone()};
                if receipt.status.terminal() {
                    let mut owner=playback_audio.lock().await;
                    owner.media_terminal=true;
                    if owner.inflight.is_empty() && audio_idle().await {
                        owner.lease.take();owner.media_terminal=false;
                    }
                }
                if playback_changes.changed().await.is_err() {break;}
            }
        });
        let result = async {
        loop {
            let mut header = [0; 4];
            if reader
                .read(&mut header[..1])
                .await
                .map_err(|e| e.to_string())?
                == 0
            {
                break;
            }
            reader
                .read_exact(&mut header[1..])
                .await
                .map_err(|e| e.to_string())?;
            let length = u32::from_le_bytes(header) as usize;
            if length == 0 || length > protocol::MAX_FRAME {
                return Err("Invalid frame size.".into());
            }
            let mut bytes = vec![0; length];
            reader
                .read_exact(&mut bytes)
                .await
                .map_err(|e| e.to_string())?;
            let Some(bytes) = assembler.push(&bytes).map_err(|e| e.to_string())? else {
                continue;
            };
            let permit = permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|e| e.to_string())?;
            enum Work {
                Network(Request),
                Action(Request),
                Upload(Request),
                Desktop(Request),
                Preview(Request),
                RichDocument(Request),
                Startup(Request),
                Delivery(Request),
                Sync(Request),
                Subscriptions(Request),
                TranscriptView(Request),
                DraftSync(Request),
                Speech(Request),
                Media(Request),
                Notify(Request),
                Voice(Request),
                Admission(String,tokio::sync::oneshot::Receiver<Response>),
                Stored(String, tokio::sync::oneshot::Receiver<Response>),
                Immediate(Response),
            }
            // Queue state commands before spawning their response tasks. This
            // preserves edit order even when network replies arrive out of order.
            let parsed = serde_json::from_slice::<Request>(&bytes);
            let migration_blocked = migration_held.load(Ordering::Acquire) && parsed.as_ref().is_ok_and(|request|!migration_allowed(&request.method));
            let mut audio_generation = None;
            let mut audio_busy = false;
            let audio_method = parsed.as_ref().ok().map(|request|request.method.clone());
            if let Ok(request) = &parsed && !migration_blocked && audio_request(request) {
                let mut owner = audio.lock().await;
                // These handlers check the exact Store. Cleanup cannot grant a new lease.
                let store_cleanup = matches!(request.method.as_str(), "voice.sessionEnd" | "media.recordingStop" | "media.stopPlayback");
                let owns_audio = if store_cleanup {
                    owner.lease.is_some()
                } else {
                    claim_audio(&mut owner.lease)
                };
                if owns_audio {
                    owner.generation = owner.generation.wrapping_add(1);
                    audio_generation = Some(owner.generation);
                    let generation=owner.generation;
                    owner.inflight.insert(generation);
                } else if !store_cleanup { audio_busy = true; }
            }
            let work = match parsed {
                Ok(request) if migration_blocked => Work::Immediate(Response::failure(request.id,"migration_held","The native runtime requires its durable private host proof before user effects.")),
                Ok(request) if request.method == crate::mutation_journal::migration_policy()["runtimeStateMethod"].as_str().unwrap() => {
                    if request.params.as_object().is_none_or(|params|!params.is_empty()) {
                        Work::Immediate(Response::failure(request.id,"invalid_request","The migration runtime state does not accept selectors."))
                    } else { Work::Immediate(Response::success(request.id,serde_json::json!({"held":migration_held.load(Ordering::Acquire)}))) }
                }
                Ok(request) if request.method == crate::mutation_journal::migration_policy()["admitMethod"].as_str().unwrap() => {
                    let id=request.id.clone();
                    let (reply,receive)=tokio::sync::oneshot::channel();
                    store_sender.send((request,reply)).await.map_err(|_|"The state worker stopped.")?;
                    Work::Admission(id,receive)
                }
                Ok(request) if audio_busy => {
                    Work::Immediate(Response::failure(
                        request.id, "audio_busy", "Another client connection owns audio. Close that connection before using audio here.",
                    ))
                }
                // Pure computations never wait behind database writes: the UI
                // calls several of them synchronously.
                Ok(request) if pure(&request.method) => Work::Immediate(compute(&request)),
                // Only Rust services export recordings to private files.
                Ok(request) if request.method == "dictation.wav" => Work::Immediate(
                    Response::failure(request.id, "invalid_request", "Unknown command."),
                ),
                Ok(request)
                    if matches!(
                        request.method.as_str(),
                        "document.render" | "document.diagram"
                    ) =>
                {
                    Work::RichDocument(request)
                }
                Ok(request) if request.method.starts_with("terminal.") => {
                    let id = request.id.clone();
                    Work::Stored(id, terminals.queue(backend.as_ref(), request))
                }
                Ok(request) if request.method.starts_with("preview.") => Work::Preview(request),
                Ok(request) if request.method.starts_with("speech.") => Work::Speech(request),
                Ok(request) if request.method.starts_with("media.") => Work::Media(request),
                Ok(request) if request.method == "notifications.show" => Work::Notify(request),
                Ok(request) if request.method.starts_with("voice.") => Work::Voice(request),
                Ok(request) if request.method.starts_with("desktop.") => Work::Desktop(request),
                Ok(request) if request.method == "system.start" => Work::Startup(request),
                Ok(request) if request.method.starts_with("view.") => {
                    Work::Immediate(views.command(&request))
                }
                Ok(request) if matches!(request.method.as_str(),"drafts.refresh"|"drafts.push") => Work::DraftSync(request),
                Ok(request) if request.method.starts_with("sync.") => Work::Sync(request),
                Ok(request) if request.method.starts_with("subscriptions.") => Work::Subscriptions(request),
                Ok(request) if request.method.starts_with("transcript_view.") => Work::TranscriptView(request),
                Ok(request)
                    if matches!(
                        request.method.as_str(),
                        "uploads.pending" | "uploads.enqueue" | "uploads.deliver" | "uploads.download"
                    ) =>
                {
                    Work::Upload(request)
                }
                Ok(request) if request.method == "actions.submit" => Work::Action(request),
                Ok(request) if matches!(request.method.as_str(), "api.request" | "api.prepareSession") => Work::Network(request),
                Ok(request) if request.method == "outbox.deliver" => Work::Delivery(request),
                Ok(request) if request.method == "outbox.activate" => {
                    active.store(true, Ordering::Relaxed);
                    Work::Immediate(Response::success(
                        request.id,
                        serde_json::json!({"active":true}),
                    ))
                }
                Ok(request) => {
                    let id = request.id.clone();
                    let (reply, receive) = tokio::sync::oneshot::channel();
                    store_sender
                        .send((request, reply))
                        .await
                        .map_err(|_| "The state worker stopped.")?;
                    Work::Stored(id, receive)
                }
                Err(error) => {
                    Work::Immediate(Response::failure(String::new(), "invalid_request", error))
                }
            };
            let (writer, backend, sender) = (writer.clone(), backend.clone(), store_sender.clone());
            // Completed tasks must leave the set; the permit bounds active work.
            while requests.try_join_next().is_some() {}
            let startup = startup.clone();
            let previews = previews.clone();
            let transcript_views = transcript_views.clone();
            let audio = audio.clone();
            let migration_held=migration_held.clone();
            requests.spawn(async move {
                let _permit = permit;
                let response = match work {
                    Work::Admission(id,receive) => {
                        let response=receive.await.unwrap_or_else(|error|Response::failure(id,"core_failed",error));
                        if response.error.is_none() && response.result.as_ref()==Some(&serde_json::json!({"admitted":true})) {
                            let was_held=migration_held.swap(false,Ordering::AcqRel);
                            #[cfg(target_os="macos")]
                            if was_held { dispatch2::DispatchQueue::main().exec_async(crate::notifications::mac::install); }
                            #[cfg(not(target_os="macos"))]
                            let _=was_held;
                        }
                        response
                    }
                    Work::Startup(request) => {
                        let mut ready = startup.lock().await;
                        let result = if let Some(value) = ready.as_ref() {
                            Ok(value.clone())
                        } else {
                            backend
                                .as_ref()
                                .as_ref()
                                .map(|_| serde_json::json!({"ready": true}))
                                .map_err(Clone::clone)
                        };
                        match result {
                            Ok(value) => {
                                *ready = Some(value.clone());
                                Response::success(request.id, value)
                            }
                            Err(error) => Response::failure(request.id, "backend_startup", error),
                        }
                    }
                    Work::RichDocument(request) => crate::rich_document::dispatch(request).await,
                    Work::Preview(request) => {
                        previews
                            .dispatch(backend.as_ref().as_ref().ok(), request)
                            .await
                    }
                    Work::Desktop(request) => {
                        crate::desktop::dispatch(
                            request,
                            Some(&sender),
                            backend.as_ref().as_ref().ok(),
                            Some(&previews),
                        )
                        .await
                    }
                    Work::Speech(request) => crate::speech::dispatch(&sender, request).await,
                    Work::Media(request) => crate::media::dispatch(request, &sender).await,
                    Work::Voice(request) => match backend.as_ref() {
                        Ok(backend) => crate::voice::dispatch_with_backend(backend,&sender,request).await,
                        Err(error) => Response::failure(request.id,"backend_config",error),
                    },
                    Work::Notify(request) => {
                        match crate::notifications::notify(&request.params["value"]).await {
                            Ok(value) => Response::success(request.id, value),
                            Err(error) => Response::failure(request.id, "notification", error),
                        }
                    }
                    Work::Upload(request) => match backend.as_ref() {
                        Ok(backend) => crate::uploads::dispatch(backend, &sender, request).await,
                        Err(error) => Response::failure(request.id, "backend_config", error),
                    },
                    Work::Action(request) => match backend.as_ref() {
                        Ok(backend) => crate::actions::submit(backend, &sender, request).await,
                        Err(error) => Response::failure(request.id, "backend_config", error),
                    },
                    Work::Network(request) => match backend.as_ref() {
                        Ok(backend) => backend.dispatch(request).await,
                        Err(error) => Response::failure(request.id, "backend_config", error),
                    },
                    Work::DraftSync(request) => match backend.as_ref() {
                        Ok(backend) => {
                            crate::draft_replication::dispatch(backend, &sender, request).await
                        }
                        Err(error) => Response::failure(request.id, "backend_config", error),
                    },
                    Work::Sync(request) => match backend.as_ref() {
                        Ok(backend) => crate::sync::dispatch(backend, &sender, request).await,
                        Err(error) => Response::failure(request.id, "backend_config", error),
                    },
                    Work::Subscriptions(request) => match backend.as_ref() {
                        Ok(backend) => crate::subscriptions::dispatch(backend,&sender,request).await,
                        Err(error) => Response::failure(request.id,"backend_config",error),
                    },
                    Work::TranscriptView(request) => transcript_views.dispatch(request).await,
                    Work::Delivery(request) => match backend.as_ref() {
                        Ok(backend) => crate::delivery::dispatch(backend, &sender, request).await,
                        Err(error) => Response::failure(request.id, "backend_config", error),
                    },
                    Work::Stored(id, receive) => receive
                        .await
                        .unwrap_or_else(|error| Response::failure(id, "core_failed", error)),
                    Work::Immediate(response) => response,
                };
                if let Some(generation) = audio_generation {
                    let mut owner = audio.lock().await;
                    owner.inflight.remove(&generation);
                    let eligible=owner.media_terminal || (owner.generation==generation && audio_method.as_deref().is_some_and(|method|audio_release_candidate(method,response.error.is_none())));
                    if eligible && owner.inflight.is_empty() && audio_idle().await {owner.lease.take();owner.media_terminal=false;}
                }
                write_packet(
                    &writer,
                    serde_json::to_value(response).expect("response serialization"),
                )
                .await
            });
        }
        assembler.finish().map_err(|e| e.to_string())?;
        Ok::<(), String>(())
        }.await;
        // Disconnect cancels pending HTTP waits. It does not prove that a server mutation was cancelled.
        drop(views);
        requests.abort_all();
        while requests.join_next().await.is_some() {}
        background.abort_all();
        while background.join_next().await.is_some() {}
        // Stop this owner's work before releasing the lease. Read-only connections
        // must never stop another connection's microphone, playback or voice.
        let mut owner = audio.lock().await;
        if owner.lease.is_some() {
            crate::media::close_recordings(&store_sender).await;
            if let Ok(backend) = backend.as_ref() { let _ = crate::voice::close_context(backend,&store_sender).await; }
            let _ = crate::media::close_audio(&store_sender).await;
            crate::voice::close_all().await;
            // An unconfirmed device stop cannot authorize a different owner.
            if !audio_idle().await && let Some(lease) = owner.lease.take() { lease.forget(); }
        }
        owner.lease.take();
        result
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    #[cfg(target_os = "macos")]
    dispatch2::DispatchQueue::main().exec_async(crate::quick_look::close);
    store_worker
        .join()
        .map_err(|_| "The state worker panicked.")?;
    result
}

fn audio_request(request: &Request) -> bool {
    !matches!(
        request.method.as_str(),
        "media.recordingState" | "media.playbackState" | "voice.sessionState"
    ) && (request.method.starts_with("media.")
        || request.method.starts_with("voice.")
        || matches!(
            request.method.as_str(),
            "desktop.requestMicrophone" | "desktop.reserveMicrophone" | "desktop.previewMedia"
        )
        || (request.method == "view.subscribe"
            && matches!(
                request.params["topic"].as_str(),
                Some("microphone" | "voice")
            )))
}

#[cfg(test)]
#[path = "preference_replication_tests.rs"]
mod preference_replication_tests;

#[derive(Default)]
struct AudioOwnership {
    lease: Option<tokio::sync::OwnedSemaphorePermit>,
    generation: u64,
    inflight: HashSet<u64>,
    media_terminal: bool,
}

async fn audio_idle() -> bool {
    crate::media::audio_idle() && crate::speech::audio_idle() && crate::voice::audio_idle().await
}

fn audio_release_candidate(method: &str, success: bool) -> bool {
    (success
        && matches!(
            method,
            "media.recordingStop"
                | "media.captureStop"
                | "media.stopPlayback"
                | "voice.sessionEnd"
                | "voice.close"
        ))
        || (!success
            && matches!(
                method,
                "media.recordingStart"
                    | "media.captureStart"
                    | "voice.sessionStart"
                    | "voice.create"
                    | "media.playRecording"
                    | "desktop.previewMedia"
            ))
}

fn claim_audio(lease: &mut Option<tokio::sync::OwnedSemaphorePermit>) -> bool {
    static OWNER: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    claim_audio_from(
        OWNER.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1))),
        lease,
    )
}

fn claim_audio_from(
    owner: &Arc<tokio::sync::Semaphore>,
    lease: &mut Option<tokio::sync::OwnedSemaphorePermit>,
) -> bool {
    if lease.is_none() {
        *lease = owner.clone().try_acquire_owned().ok();
    }
    lease.is_some()
}

fn pure(method: &str) -> bool {
    matches!(
        method,
        "markdown.parse"
            | "system.uuid"
            | "system.uuidBatch"
            | "system.sha256"
            | "text.sentences"
            | "locale.language_name"
    ) || method.starts_with("url.")
        || method.starts_with("file_link.")
}

fn migration_allowed(method: &str) -> bool {
    let policy = crate::mutation_journal::migration_policy();
    pure(method)
        || [
            "runtimeStateMethod",
            "admitMethod",
            "statusMethod",
            "finishMethod",
            "ownerInventoryMethod",
            "nativeSourceMethod",
            "nativeImportMethod",
        ]
        .iter()
        .any(|key| policy[*key].as_str() == Some(method))
        || matches!(
            method,
            "preferences.raw_all"
                | "identity.get"
                | "identity.set"
                | "client_storage.import"
                | "client_storage.status"
                | "client_storage.export_preferences"
                | "client_storage.replay_preferences"
                | "files.importBegin"
                | "files.importChunk"
                | "files.importCommit"
                | "files.importReadback"
                | "files.hashBegin"
                | "files.hashChunk"
                | "files.hashFinish"
        )
}
fn compute(request: &Request) -> Response {
    if request.id.is_empty() || request.id.len() > 128 {
        return Response::failure(
            request.id.clone(),
            "invalid_request",
            "Invalid request identity.",
        );
    }
    match request.method.as_str() {
        "markdown.parse" => crate::markdown::dispatch(request),
        method if method.starts_with("file_link.") => crate::file_links::dispatch(request),
        _ => crate::platform::dispatch(request),
    }
}

/// `STUDIO_CORE_TRACE=1` logs each packet size and time since start to stderr.
fn trace(packet: &serde_json::Value, size: usize) {
    static START: std::sync::OnceLock<Option<std::time::Instant>> = std::sync::OnceLock::new();
    let Some(start) =
        START.get_or_init(|| env::var_os("STUDIO_CORE_TRACE").map(|_| std::time::Instant::now()))
    else {
        return;
    };
    let kind = if packet.get("event").is_some() {
        let value = &packet["value"];
        if value.get("patch").is_some() {
            "patch"
        } else if value.get("document").is_some() {
            "document"
        } else {
            "event"
        }
    } else {
        "reply"
    };
    eprintln!(
        "STUDIO_CORE_TRACE t={:.3}s kind={kind} bytes={size}",
        start.elapsed().as_secs_f64()
    );
}

async fn write_packet(
    writer: &Arc<tokio::sync::Mutex<tokio::net::unix::OwnedWriteHalf>>,
    mut packet: serde_json::Value,
) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&packet)?;
    trace(&packet, bytes.len());
    if bytes.len() > protocol::MAX_MESSAGE {
        eprintln!(
            "STUDIO_CORE_RESPONSE_SIZE bytes={} id={} event={}",
            bytes.len(),
            packet["id"],
            packet["event"]
        );
        packet
            .as_object_mut()
            .expect("response object")
            .remove("value");
        packet.as_object_mut().unwrap().remove("result");
        packet["error"] = serde_json::to_value(crate::sync::error(
            "response_size",
            "The response exceeds the UI transport limit.",
        ))?;
        bytes = serde_json::to_vec(&packet)?;
    }
    let mut writer = writer.lock().await;
    for (flag, part) in protocol::fragments(&bytes)? {
        writer
            .write_all(&((part.len() + 1) as u32).to_le_bytes())
            .await?;
        writer.write_all(&[flag]).await?;
        writer.write_all(part).await?;
    }
    Ok(())
}

pub fn connect(config: ConnectionConfig) -> io::Result<UnixStream> {
    connect_owned(true, move |core| serve(core, config))
}

/// Host-only entry for a verified local or paired remote service context.
pub fn connect_with_backend(state_directory: PathBuf, backend: Backend) -> io::Result<UnixStream> {
    connect_owned(true, move |core| {
        serve_backend(core, state_directory, Ok(backend), false)
    })
}

/// Private host entry. Schema creation is allowed; recovery and user effects
/// stay held until this same Store verifies a durable host capture receipt.
pub fn connect_for_migration(state_directory: PathBuf, backend: Backend) -> io::Result<UnixStream> {
    connect_owned(false, move |core| {
        serve_backend(core, state_directory, Ok(backend), true)
    })
}

fn connect_owned(
    install_notifications: bool,
    serve: impl FnOnce(UnixStream) -> Result<(), String> + Send + 'static,
) -> io::Result<UnixStream> {
    let (ui, core) = UnixStream::pair()?;
    ui.set_nonblocking(true)?;
    #[cfg(target_os = "macos")]
    {
        let enabled: libc::c_int = 1;
        if unsafe {
            libc::setsockopt(
                ui.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&enabled as *const libc::c_int).cast(),
                size_of_val(&enabled) as _,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(target_os = "macos")]
    if install_notifications {
        dispatch2::DispatchQueue::main().exec_async(crate::notifications::mac::install);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = install_notifications;
    thread::Builder::new()
        .name("studio-core".into())
        .spawn(move || {
            if let Err(error) = serve(core) {
                eprintln!("STUDIO_CORE_CHANNEL_CLOSED {error}");
            }
        })?;
    Ok(ui)
}

#[cfg(test)]
mod audio_ownership_tests {
    use super::*;

    struct AudioConnection {
        socket: Option<UnixStream>,
        worker: Option<thread::JoinHandle<Result<(), String>>>,
        events: Vec<serde_json::Value>,
    }

    impl AudioConnection {
        fn open(state_directory: PathBuf) -> Self {
            Self::open_with_origin(state_directory, "http://127.0.0.1:9".into())
        }

        fn open_with_origin(state_directory: PathBuf, backend_origin: String) -> Self {
            let (socket, core) = UnixStream::pair().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let worker = thread::spawn(move || {
                serve(
                    core,
                    ConnectionConfig {
                        state_directory,
                        backend_origin,
                    },
                )
            });
            Self {
                socket: Some(socket),
                worker: Some(worker),
                events: Vec::new(),
            }
        }

        fn request(&mut self, id: &str, method: &str, params: serde_json::Value) -> Response {
            use std::io::{Read, Write};
            let socket = self.socket.as_mut().unwrap();
            let request = Request {
                id: id.into(),
                method: method.into(),
                params,
            };
            let bytes = serde_json::to_vec(&request).unwrap();
            for (flag, bytes) in protocol::fragments(&bytes).unwrap() {
                socket
                    .write_all(&((bytes.len() + 1) as u32).to_le_bytes())
                    .unwrap();
                socket.write_all(&[flag]).unwrap();
                socket.write_all(bytes).unwrap();
            }
            let mut assembler = protocol::Assembler::default();
            loop {
                let mut header = [0; 4];
                socket.read_exact(&mut header).unwrap();
                let mut frame = vec![0; u32::from_le_bytes(header) as usize];
                socket.read_exact(&mut frame).unwrap();
                if let Some(bytes) = assembler.push(&frame).unwrap() {
                    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    if value.get("event").is_some() {
                        self.events.push(value);
                        continue;
                    }
                    let response: Response = serde_json::from_value(value).unwrap();
                    assert_eq!(response.id, id);
                    return response;
                }
            }
        }

        fn event(&mut self, subscription: &str) -> serde_json::Value {
            use std::io::Read;
            if let Some(index) = self
                .events
                .iter()
                .position(|event| event["event"] == subscription)
            {
                return self.events.remove(index);
            }
            let socket = self.socket.as_mut().unwrap();
            let mut assembler = protocol::Assembler::default();
            loop {
                let mut header = [0; 4];
                socket.read_exact(&mut header).unwrap();
                let mut frame = vec![0; u32::from_le_bytes(header) as usize];
                socket.read_exact(&mut frame).unwrap();
                if let Some(bytes) = assembler.push(&frame).unwrap() {
                    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert!(
                        value.get("event").is_some(),
                        "unexpected RPC reply while waiting for a native event"
                    );
                    if value["event"] == subscription {
                        return value;
                    }
                    self.events.push(value);
                }
            }
        }
    }

    impl Drop for AudioConnection {
        fn drop(&mut self) {
            self.socket.take();
            if let Some(worker) = self.worker.take() {
                let result = worker.join();
                if !thread::panicking() {
                    result.unwrap().unwrap();
                }
            }
        }
    }

    fn idle_cleanup_does_not_take_audio(method: &str, expected_code: &str) {
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut idle = AudioConnection::open(directory.path().join("idle"));
        let mut next = AudioConnection::open(directory.path().join("next"));
        let cleanup = idle.request("invalid-stop", method, serde_json::json!({}));
        // An absent agent fails before backend I/O, peer creation or microphone use.
        let start = next.request("invalid-start", "voice.sessionStart", serde_json::json!({}));
        // Release the erroneous lease before disconnect, without native audio cleanup.
        let release = idle.request(
            "fixture-release",
            "voice.sessionEnd",
            serde_json::json!({"agent":"fixture"}),
        );
        drop(next);
        drop(idle);
        drop(_serial);
        assert!(release.error.is_none());
        assert_eq!(cleanup.error.unwrap().code, expected_code);
        assert_eq!(
            start.error.unwrap().code,
            "voice",
            "idle cleanup must not claim the global audio lease"
        );
    }

    #[test]
    fn native_voice_stop_invalid_idle_end_does_not_claim_audio() {
        idle_cleanup_does_not_take_audio("voice.sessionEnd", "voice");
    }

    #[test]
    fn native_voice_stop_invalid_idle_recording_stop_does_not_claim_audio() {
        idle_cleanup_does_not_take_audio("media.recordingStop", "media");
    }

    #[test]
    fn native_voice_stop_preserves_the_existing_connection_lease() {
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut owner = AudioConnection::open(directory.path().join("owner"));
        let mut sibling = AudioConnection::open(directory.path().join("sibling"));
        let subscribed = owner.request(
            "own",
            "view.subscribe",
            serde_json::json!({
                "subscription":"own", "topic":"microphone", "capture":"lease-only"
            }),
        );
        let _ = sibling.request("sibling-stop", "voice.sessionEnd", serde_json::json!({}));
        let sibling_start =
            sibling.request("sibling-start", "voice.sessionStart", serde_json::json!({}));
        let invalid_owner_stop = owner.request(
            "invalid-owner-stop",
            "media.recordingStop",
            serde_json::json!({}),
        );
        let still_owned =
            sibling.request("still-owned", "voice.sessionStart", serde_json::json!({}));
        // No voice session exists. This exact owner's End is a harmless cleanup.
        let owner_end = owner.request(
            "owner-end",
            "voice.sessionEnd",
            serde_json::json!({"agent":"fixture"}),
        );
        let next_start = sibling.request("next-start", "voice.sessionStart", serde_json::json!({}));
        drop(sibling);
        drop(owner);
        drop(_serial);
        assert!(subscribed.error.is_none());
        assert_eq!(sibling_start.error.unwrap().code, "audio_busy");
        assert_eq!(invalid_owner_stop.error.unwrap().code, "media");
        assert_eq!(still_owned.error.unwrap().code, "audio_busy");
        assert!(owner_end.error.is_none());
        assert_eq!(next_start.error.unwrap().code, "voice");
    }

    #[test]
    fn only_the_audio_owner_holds_cleanup_authority_until_disconnect() {
        let pool = Arc::new(tokio::sync::Semaphore::new(1));
        let (mut first, mut other) = (None, None);
        assert!(claim_audio_from(&pool, &mut first));
        assert!(
            claim_audio_from(&pool, &mut first),
            "same owner can issue more commands"
        );
        assert!(!claim_audio_from(&pool, &mut other));
        assert!(
            other.is_none(),
            "rejected and read-only connections cannot clean up audio"
        );
        drop(other);
        assert_eq!(pool.available_permits(), 0);
        drop(first);
        let mut next = None;
        assert!(claim_audio_from(&pool, &mut next));
    }

    #[test]
    fn generic_media_preview_keeps_the_connection_lease_until_exact_owner_disconnect() {
        use std::io::{Read, Write};
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        let device = crate::media::playback_tests::install_preview_mock();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let http = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 4096);
            }
            assert!(
                String::from_utf8(request)
                    .unwrap()
                    .starts_with("GET /api/file?asset=clip ")
            );
            let body = serde_json::json!({"name":"clip.wav","mime":"audio/wav","base64":"bmF0aXZlLW93bmVkLW1lZGlh"}).to_string();
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",body.len(),body).unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut owner = AudioConnection::open_with_origin(directory.path().join("owner"), origin);
        let snapshot = owner.request(
            "open-file",
            "preview.open",
            serde_json::json!({"target":{"asset":"clip"}}),
        );
        assert!(snapshot.error.is_none(), "{snapshot:?}");
        let handle = snapshot.result.unwrap()["handle"].clone();
        let watch = owner.request("watch-original-media", "view.subscribe", serde_json::json!({"topic":"media-preview","subscription":"exact-media-watch","requestId":"original-media-open"}));
        assert!(watch.error.is_none(), "{watch:?}");
        let started = owner.request("original-media-open", "desktop.previewMedia", serde_json::json!({"permit":crate::desktop::test_native_permit(6),"value":{"handle":handle}}));
        assert!(started.error.is_none(), "{started:?}");
        let started = started.result.expect("preparing receipt");
        assert_eq!(started["status"], "preparing");
        loop {
            let event = owner.event("exact-media-watch");
            assert_eq!(event["value"]["requestId"], "original-media-open");
            if event["value"]["status"] == "playing" {
                assert_eq!(event["value"]["playbackId"], started["playbackId"]);
                break;
            }
            assert!(matches!(
                event["value"]["status"].as_str(),
                Some("unknown" | "preparing")
            ));
        }
        let mut unrelated = AudioConnection::open(directory.path().join("unrelated"));
        let blocked = unrelated.request("other-audio", "voice.sessionStart", serde_json::json!({}));
        assert_eq!(blocked.error.unwrap().code, "audio_busy");
        drop(unrelated);
        let current = owner.request(
            "original-receipt",
            "media.playbackState",
            serde_json::json!({"requestId":"original-media-open"}),
        );
        assert_eq!(current.result.unwrap()["status"], "playing");
        assert!(device.0.stopped().is_empty());
        drop(owner);
        assert_eq!(device.0.stopped(), vec![1]);
        let mut next = AudioConnection::open(directory.path().join("next"));
        let accepted = next.request("next-audio", "voice.sessionStart", serde_json::json!({}));
        assert_eq!(accepted.error.unwrap().code, "voice");
        drop(next);
        http.join().unwrap();
    }

    #[test]
    fn unrelated_disconnect_keeps_capture_and_malformed_owner_disconnect_releases_it() {
        use std::io::{Read, Write};
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let open = |name: &str| {
            let (host, core) = UnixStream::pair().unwrap();
            host.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let config = ConnectionConfig {
                state_directory: directory.path().join(name),
                backend_origin: "http://127.0.0.1:9".into(),
            };
            let worker = thread::spawn(move || serve(core, config));
            (host, worker)
        };
        let call = |socket: &mut UnixStream, id: &str| -> Response {
            let bytes = serde_json::to_vec(&Request { id: id.into(), method: "view.subscribe".into(), params: serde_json::json!({"subscription":id,"topic":"microphone","capture":"isolated-capture"}) }).unwrap();
            for (flag, bytes) in protocol::fragments(&bytes).unwrap() {
                socket
                    .write_all(&((bytes.len() + 1) as u32).to_le_bytes())
                    .unwrap();
                socket.write_all(&[flag]).unwrap();
                socket.write_all(bytes).unwrap();
            }
            let mut assembler = protocol::Assembler::default();
            loop {
                let mut header = [0; 4];
                socket.read_exact(&mut header).unwrap();
                let mut frame = vec![0; u32::from_le_bytes(header) as usize];
                socket.read_exact(&mut frame).unwrap();
                if let Some(bytes) = assembler.push(&frame).unwrap() {
                    return serde_json::from_slice(&bytes).unwrap();
                }
            }
        };
        let (mut first, first_worker) = open("first");
        assert!(call(&mut first, "first").error.is_none());
        let (mut other, other_worker) = open("other");
        assert_eq!(call(&mut other, "other").error.unwrap().code, "audio_busy");
        drop(other);
        other_worker.join().unwrap().unwrap();
        assert!(
            crate::media::attach(&serde_json::json!("isolated-capture")).is_err(),
            "unrelated disconnect must retain the owner's subscriber"
        );
        // A malformed packet must still run cleanup and release ownership.
        first.write_all(&0u32.to_le_bytes()).unwrap();
        assert!(first_worker.join().unwrap().is_err());
        drop(first);
        let (mut next, next_worker) = open("next");
        assert!(call(&mut next, "next").error.is_none());
        drop(next);
        next_worker.join().unwrap().unwrap();
    }

    #[test]
    fn all_audio_entry_points_claim_the_same_owner() {
        for method in [
            "media.captureStart",
            "media.captureStop",
            "media.requestMicrophone",
            "media.playRecording",
            "media.stopPlayback",
            "voice.create",
            "voice.answer",
            "voice.close",
            "desktop.requestMicrophone",
            "desktop.reserveMicrophone",
        ] {
            assert!(
                audio_request(&Request {
                    id: "test".into(),
                    method: method.into(),
                    params: serde_json::Value::Null
                }),
                "{method}"
            );
        }
        for topic in ["microphone", "voice"] {
            assert!(audio_request(&Request {
                id: "test".into(),
                method: "view.subscribe".into(),
                params: serde_json::json!({"topic":topic})
            }));
        }
        assert!(!audio_request(&Request {
            id: "test".into(),
            method: "preferences.raw_all".into(),
            params: serde_json::Value::Null
        }));
        for method in ["media.recordingState", "voice.sessionState"] {
            assert!(!audio_request(&Request {
                id: "read".into(),
                method: method.into(),
                params: serde_json::Value::Null
            }));
            assert!(!audio_release_candidate(method, true));
        }
        assert!(audio_release_candidate("media.recordingStop", true));
        assert!(!audio_release_candidate("media.recordingStop", false));
        assert!(audio_release_candidate("voice.sessionStart", false));
        assert!(!audio_release_candidate("voice.sessionStart", true));
    }
}
