//! C entry point for the Rust library packaged with the Attar UI.
use crate::{
    backend::Backend,
    protocol::{self, Request, Response},
};
use std::{io, os::unix::net::UnixStream, sync::Arc, thread};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Only the native host supplies this configuration. It never comes from IPC.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ConnectionConfig {
    pub backend_origin: String,
}

fn serve(channel: UnixStream, config: ConnectionConfig) -> Result<(), String> {
    // Electron verifies/starts the backend before opening the client. Rust must
    // never race that owner by launching a second backend.
    let backend = Arc::new(Backend::new(&config.backend_origin));
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
        let mut background = tokio::task::JoinSet::new();
        let (events, mut event_receiver) = tokio::sync::mpsc::channel::<serde_json::Value>(64);
        let mut views = crate::views::Views::new(
            backend.clone(),
            events,
        );
        let event_writer = writer.clone();
        background.spawn(async move {
            while let Some(event) = event_receiver.recv().await {
                if write_packet(&event_writer, event).await.is_err() {
                    break;
                }
            }
        });
        let mut assembler = protocol::Assembler::default();
        // Audio adapters currently expose one process-wide playback/voice service.
        // Retain exclusive ownership until teardown completes, including error paths.
        let mut audio_lease = None;
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
                Desktop(Request),
                Speech(Request),
                Media(Request),
                Voice(Request),
                Immediate(Response),
            }
            let work = match serde_json::from_slice::<Request>(&bytes) {
                Ok(request) if audio_request(&request) && !claim_audio(&mut audio_lease) => {
                    Work::Immediate(Response::failure(
                        request.id, "audio_busy", "Another client connection owns audio. Close that connection before using audio here.",
                    ))
                }
                // Pure computations never wait behind database writes: the UI
                // calls several of them synchronously.
                Ok(request) if request.method == "markdown.parse" => Work::Immediate(compute(&request)),
                Ok(request) if request.method.starts_with("speech.") => Work::Speech(request),
                Ok(request) if request.method.starts_with("media.") => Work::Media(request),
                Ok(request) if request.method.starts_with("voice.") => Work::Voice(request),
                Ok(request) if request.method.starts_with("desktop.") => Work::Desktop(request),
                Ok(request) if request.method.starts_with("view.") => {
                    Work::Immediate(views.command(&request))
                }
                Ok(request) if request.method == "api.request" => Work::Network(request),
                Ok(request) => Work::Immediate(Response::failure(
                    request.id,
                    "unknown_method",
                    "Unknown native method.",
                )),
                Err(error) => {
                    Work::Immediate(Response::failure(String::new(), "invalid_request", error))
                }
            };
            let (writer, backend) = (writer.clone(), backend.clone());
            // Completed tasks must leave the set; the permit bounds active work.
            while requests.try_join_next().is_some() {}
            requests.spawn(async move {
                let _permit = permit;
                let response = match work {
                    Work::Desktop(request) => {
                        crate::desktop::dispatch(
                            request,
                            backend.as_ref().as_ref().ok(),
                        )
                        .await
                    }
                    Work::Speech(request) => crate::speech::dispatch(request).await,
                    Work::Media(request) => crate::media::dispatch(request).await,
                    Work::Voice(request) => crate::voice::dispatch(request).await,
                    Work::Network(request) => match backend.as_ref() {
                        Ok(backend) => backend.dispatch(request).await,
                        Err(error) => Response::failure(request.id, "backend_config", error),
                    },
                    Work::Immediate(response) => response,
                };
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
        if audio_lease.is_some() {
            crate::media::close_all();
            crate::voice::close_all().await;
        }
        drop(audio_lease);
        result
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    #[cfg(target_os = "macos")]
    dispatch2::DispatchQueue::main().exec_async(crate::quick_look::close);
    result
}

fn audio_request(request: &Request) -> bool {
    request.method.starts_with("media.")
        || request.method.starts_with("voice.")
        || matches!(request.method.as_str(), "desktop.requestMicrophone")
        || (request.method == "view.subscribe"
            && matches!(
                request.params["topic"].as_str(),
                Some("microphone" | "voice")
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

fn compute(request: &Request) -> Response {
    if request.id.is_empty() || request.id.len() > 128 {
        return Response::failure(
            request.id.clone(),
            "invalid_request",
            "Invalid request identity.",
        );
    }
    crate::markdown::dispatch(request)
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
        // The object check above succeeded and only removed an object member.
        packet
            .as_object_mut()
            .expect("response object")
            .remove("result");
        packet["error"] = serde_json::to_value(crate::backend::error(
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

// SAFETY: setsockopt borrows a live socket and an initialized scalar for this call.
#[allow(unsafe_code)]
pub fn connect(config: ConnectionConfig) -> io::Result<UnixStream> {
    let (ui, core) = UnixStream::pair()?;
    ui.set_nonblocking(true)?;
    #[cfg(target_os = "macos")]
    {
        let enabled: libc::c_int = 1;
        if unsafe {
            libc::setsockopt(
                std::os::fd::AsRawFd::as_raw_fd(&ui),
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
    dispatch2::DispatchQueue::main().exec_async(crate::notifications::mac::install);
    thread::Builder::new()
        .name("studio-core".into())
        .spawn(move || {
            if let Err(error) = serve(core, config) {
                eprintln!("STUDIO_CORE_CHANNEL_CLOSED {error}");
            }
        })?;
    Ok(ui)
}

#[cfg(test)]
mod audio_ownership_tests {
    use super::*;

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
    fn unrelated_disconnect_keeps_capture_and_malformed_owner_disconnect_releases_it() {
        use std::io::{Read, Write};
        let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
        let open = || {
            let (host, core) = UnixStream::pair().unwrap();
            host.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let config = ConnectionConfig {
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
        let (mut first, first_worker) = open();
        assert!(call(&mut first, "first").error.is_none());
        let (mut other, other_worker) = open();
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
        let (mut next, next_worker) = open();
        assert!(call(&mut next, "next").error.is_none());
        drop(next);
        next_worker.join().unwrap().unwrap();
    }

    #[test]
    fn all_audio_entry_points_claim_the_same_owner() {
        for method in [
            "media.captureStart",
            "media.captureStop",
            "voice.create",
            "voice.answer",
            "voice.close",
            "desktop.requestMicrophone",
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
    }
}
