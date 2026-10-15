//! On-device transcription of saved dictation. The native helper is the same
//! Speech framework program that the Electron app packages.
use crate::{
    delivery::{StoreSender, state},
    protocol::{Request, Response},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::{io::AsyncBufReadExt, sync::broadcast};

const PERMIT_LIFETIME: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(75 * 60);
pub const CANCELLED: &str = "Transcription canceled. Your recording is saved.";

#[derive(Default)]
struct Service {
    permits: HashMap<String, Instant>,
    cancelled: HashMap<String, Instant>,
    running: Option<(String, tokio::sync::watch::Sender<bool>)>,
}
fn service() -> &'static Mutex<Service> {
    static VALUE: OnceLock<Mutex<Service>> = OnceLock::new();
    VALUE.get_or_init(Mutex::default)
}
pub fn progress() -> &'static broadcast::Sender<Value> {
    static VALUE: OnceLock<broadcast::Sender<Value>> = OnceLock::new();
    VALUE.get_or_init(|| broadcast::channel(64).0)
}

/// Called after the native gesture for the Transcribe or Stop button.
pub fn prepare(now: Instant) -> Result<Value, String> {
    let token = crate::file_save::uuid()?;
    let mut service = service()
        .lock()
        .map_err(|_| "The speech service stopped.")?;
    service.permits.retain(|_, expires| *expires > now);
    if service.permits.len() >= 16 {
        return Err("Too many transcription requests are waiting.".into());
    }
    service.permits.insert(token.clone(), now + PERMIT_LIFETIME);
    Ok(Value::String(token))
}

/// Called after the native gesture for Cancel transcription.
pub fn cancel(id: &Value) -> Result<Value, String> {
    let id = id
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128)
        .ok_or("Invalid transcription identity.")?;
    let mut service = service()
        .lock()
        .map_err(|_| "The speech service stopped.")?;
    let now = Instant::now();
    service.cancelled.retain(|_, expires| *expires > now);
    if service.cancelled.len() >= 128 && !service.cancelled.contains_key(id) {
        return Err("Too many transcription cancellations are pending.".into());
    }
    // Cancellation can reach the worker before the queued transcription starts.
    service
        .cancelled
        .insert(id.to_owned(), now + PERMIT_LIFETIME);
    Ok(Value::Bool(match &service.running {
        Some((running, stop)) if running == id => {
            let _ = stop.send(true);
            true
        }
        _ => false,
    }))
}

// Speech uploads have a separate capability pool: a transcription permit must
// never authorize Save As. The browser keeps the durable original recording.
fn uploads() -> &'static Mutex<crate::file_save::Transfers> {
    static POOL: OnceLock<Mutex<crate::file_save::Transfers>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}
fn upload(method: &str, params: &Value) -> Result<Value, String> {
    if method == "speech.uploadBegin" {
        let service = service()
            .lock()
            .map_err(|_| "The speech service stopped.")?;
        let permit = params["permit"].as_str().unwrap_or("");
        if !service
            .permits
            .get(permit)
            .is_some_and(|expires| *expires > Instant::now())
        {
            return Err("Use the Transcribe button again.".into());
        }
    }
    let mut pool = uploads().lock().map_err(|_| "The speech upload stopped.")?;
    match method {
        "speech.uploadBegin" => pool.begin(
            &json!({"name":"recording.wav", "size":params["size"]}),
            Instant::now(),
        ),
        "speech.uploadChunk" => pool.chunk(params, Instant::now()),
        "speech.uploadCancel" => pool.cancel(params),
        _ => Err("Unknown speech upload command.".into()),
    }
}

fn valid_locale(value: &str) -> bool {
    let mut parts = value.split(['-', '_']);
    let first = parts.next().unwrap_or("");
    let rest: Vec<_> = parts.collect();
    (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_alphabetic())
        && rest.len() <= 3
        && rest
            .iter()
            .all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Same WAV contract as the Electron helper: mono 16-bit PCM, at most 30 minutes.
pub fn validate_wav(data: &[u8]) -> Result<(), String> {
    let u16_at = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
    let u32_at = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
    if data.len() < 46 || data.len() > 64 * 1024 * 1024 {
        return Err("Expected a WAV recording of at most 64 MB.".into());
    }
    if &data[0..4] != b"RIFF"
        || &data[8..12] != b"WAVE"
        || &data[12..16] != b"fmt "
        || u32_at(16) != 16
        || u16_at(20) != 1
        || u16_at(22) != 1
        || u16_at(34) != 16
        || &data[36..40] != b"data"
        || u32_at(40) as usize != data.len() - 44
    {
        return Err("Expected a mono 16-bit PCM WAV recording.".into());
    }
    let rate = u32_at(24);
    if !(8000..=192_000).contains(&rate)
        || u32_at(28) != rate * 2
        || u16_at(32) != 2
        || u32_at(4) as usize != data.len() - 8
        || (data.len() - 44) % 2 != 0
        || (data.len() - 44) as u64 / (rate as u64 * 2) > 1801
    {
        return Err("Invalid WAV recording or duration exceeds 30 minutes.".into());
    }
    Ok(())
}

/// Only increasing, bounded progress lines are events. Other output is diagnostic.
fn progress_line(line: &str, previous: &mut i64) -> Option<(i64, i64)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let (completed, total) = (value["completed"].as_i64()?, value["total"].as_i64()?);
    if value["type"] != "progress"
        || !(1..=37).contains(&total)
        || completed < 0
        || completed > total
        || completed <= *previous
    {
        return None;
    }
    *previous = completed;
    Some((completed, total))
}

fn helper() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    Ok(executable
        .parent()
        .ok_or("Missing application path.")?
        .join("../Resources/helpers/studio-speech"))
}

struct Running(String);
impl Drop for Running {
    fn drop(&mut self) {
        if let Ok(mut service) = service().lock() {
            if service
                .running
                .as_ref()
                .is_some_and(|(id, _)| *id == self.0)
            {
                service.running = None;
            }
        }
    }
}

async fn run(
    helper: &Path,
    file: &Path,
    locale: &str,
    id: &str,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> Result<Value, String> {
    if tokio::fs::metadata(helper).await.is_err() {
        return Err("The native speech helper is missing. Install the updated Codex Studio app. Your recording is saved.".into());
    }
    let mut child = tokio::process::Command::new(helper)
        .arg(file)
        .arg(locale)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let id = id.to_owned();
    let lines = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut previous = -1;
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some((completed, total)) = progress_line(&line, &mut previous) {
                let _ = progress().send(json!({"id":id,"completed":completed,"total":total}));
            }
        }
    });
    let output = async {
        use tokio::io::AsyncReadExt;
        let mut bytes = vec![];
        (&mut stdout)
            .take(2 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .await
            .map_err(|e| e.to_string())?;
        let status = child.wait().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((status, bytes))
    };
    let result = tokio::select! {
        result = output => result,
        _ = stop.wait_for(|stopped| *stopped) => Err(CANCELLED.to_owned()),
        _ = tokio::time::sleep(TIMEOUT) => Err("Transcription timed out. Your recording is saved; retry transcription.".to_owned()),
    };
    let (status, bytes) = match result {
        Ok(value) => {
            // Deliver the final progress lines written just before exit.
            let _ = tokio::time::timeout(Duration::from_secs(1), lines).await;
            value
        }
        Err(error) => {
            lines.abort();
            return Err(error);
        }
    };
    let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
    if !status.success() {
        return Err(parsed
            .as_ref()
            .and_then(|v| v["error"].as_str())
            .map(str::to_owned)
            .unwrap_or(
                "Native transcription failed. Your recording is saved; retry transcription.".into(),
            ));
    }
    let value = parsed
        .ok_or("Native transcription failed. Your recording is saved; retry transcription.")?;
    match value["text"].as_str() {
        Some(text) if !text.trim().is_empty() => Ok(json!({
            "text": text,
            "provider": value["provider"].as_str().unwrap_or("macOS Speech"),
            "onDevice": value["onDevice"].as_bool().unwrap_or(true),
        })),
        _ => Err(value["error"]
            .as_str()
            .unwrap_or("No speech was recognized. Your recording is saved.")
            .to_owned()),
    }
}

async fn transcribe(store: &StoreSender, params: &Value) -> Result<Value, String> {
    let id = params["id"]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or("Invalid transcription identity.")?
        .to_owned();
    let locale = params["locale"]
        .as_str()
        .filter(|v| valid_locale(v))
        .ok_or("Invalid speech language.")?;
    let recording = params["recording"]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 128);
    if recording.is_none() && !params["transfer"].is_string() {
        return Err("Invalid recording.".into());
    }
    let (stop, stopped) = tokio::sync::watch::channel(false);
    {
        let mut service = service()
            .lock()
            .map_err(|_| "The speech service stopped.")?;
        let permit = params["permit"].as_str().unwrap_or("");
        match service.permits.remove(permit) {
            Some(expires) if expires > Instant::now() => {}
            _ => return Err("Use the Transcribe button again.".into()),
        }
        if service
            .cancelled
            .remove(&id)
            .is_some_and(|expires| expires > Instant::now())
        {
            return Err(CANCELLED.into());
        }
        if service.running.is_some() {
            return Err("Another recording is being transcribed. Retry when it finishes.".into());
        }
        service.running = Some((id.clone(), stop));
    }
    let _running = Running(id.clone());
    let directory = tempfile::Builder::new()
        .prefix("codex-studio-speech-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let file = if let Some(recording) = recording {
        let exported = state(
            store,
            "dictation.wav",
            json!({"id":recording,"directory":directory.path()}),
        )
        .await?;
        let file = PathBuf::from(
            exported["path"]
                .as_str()
                .ok_or("The recording was not exported.")?,
        );
        let bytes = tokio::fs::read(&file).await.map_err(|e| e.to_string())?;
        validate_wav(&bytes)?;
        file
    } else {
        let (_, bytes) = uploads()
            .lock()
            .map_err(|_| "The speech upload stopped.")?
            .take(params, Instant::now())?;
        validate_wav(&bytes)?;
        let file = directory.path().join("recording.wav");
        tokio::fs::write(&file, bytes)
            .await
            .map_err(|e| e.to_string())?;
        file
    };
    run(&helper()?, &file, locale, &id, stopped).await
}

pub async fn dispatch(store: &StoreSender, request: Request) -> Response {
    let result = match request.method.as_str() {
        "speech.transcribe" => transcribe(store, &request.params).await,
        "speech.cancel" => cancel(&request.params["id"]),
        "speech.uploadBegin" | "speech.uploadChunk" | "speech.uploadCancel" => {
            upload(&request.method, &request.params)
        }
        _ => Err("Unknown speech command.".into()),
    };
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "speech", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn speech_uploads_require_permission_preserve_bytes_and_cannot_authorize_saves() {
        use base64::Engine;
        assert!(upload("speech.uploadBegin", &json!({"permit":"forged", "size":4})).is_err());
        let permit = prepare(Instant::now()).unwrap();
        let value = upload("speech.uploadBegin", &json!({"permit":permit,"size":4})).unwrap();
        let transfer = value["transfer"].clone();
        let chunk = json!({"transfer":transfer,"offset":0,"base64":base64::engine::general_purpose::STANDARD.encode([0,128,255,127])});
        assert!(crate::file_save::command("desktop.saveChunk", &chunk).is_err());
        assert!(crate::file_save::finish(&json!({"transfer":transfer})).is_err());
        upload("speech.uploadChunk", &chunk).unwrap();
        upload("speech.uploadChunk", &chunk).unwrap();
        assert!(
            upload(
                "speech.uploadChunk",
                &json!({"transfer":transfer,"offset":0,"base64":"AAAAAA=="})
            )
            .is_err()
        );
        let (_, bytes) = uploads()
            .lock()
            .unwrap()
            .take(&json!({"transfer":transfer}), Instant::now())
            .unwrap();
        assert_eq!(bytes, [0, 128, 255, 127]);
        assert!(
            uploads()
                .lock()
                .unwrap()
                .take(&json!({"transfer":transfer}), Instant::now())
                .is_err()
        );
        service()
            .lock()
            .unwrap()
            .permits
            .remove(permit.as_str().unwrap());
    }

    #[tokio::test]
    async fn cancellation_before_dispatch_never_reads_the_saved_recording() {
        let id = "cancel-before-worker";
        let permit = prepare(Instant::now()).unwrap();
        assert_eq!(cancel(&json!(id)).unwrap(), false);
        let (store, mut receiver) = tokio::sync::mpsc::channel(1);
        let result = transcribe(
            &store,
            &json!({"id":id,"permit":permit,"locale":"en-US","recording":"saved"}),
        )
        .await;
        assert_eq!(result.unwrap_err(), CANCELLED);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn locale_wav_and_progress_rules_match_the_desktop_helper() {
        for good in ["en", "en-US", "ru_RU", "zh-Hant-TW", "sr-Latn-RS-x1"] {
            assert!(valid_locale(good), "{good}");
        }
        for bad in [
            "",
            "e",
            "english",
            "en-",
            "en-US-a",
            "en/US",
            "en-US-1-2-3-4",
        ] {
            assert!(!valid_locale(bad), "{bad}");
        }
        let mut wav = crate::dictation::wav_header(2, 16000).to_vec();
        wav.extend([1, 0, 2, 0]);
        validate_wav(&wav).unwrap();
        let mut odd = wav.clone();
        odd[22] = 2;
        assert!(validate_wav(&odd).is_err());
        assert!(validate_wav(&wav[..45]).is_err());
        let mut previous = -1;
        let lines = [
            r#"{"type":"progress","completed":0,"total":2}"#,
            r#"{"type":"progress","completed":0,"total":2}"#,
            "diagnostic",
            r#"{"type":"progress","completed":3,"total":2}"#,
            r#"{"type":"progress","completed":1,"total":38}"#,
            r#"{"type":"progress","completed":2,"total":2}"#,
        ];
        let events: Vec<_> = lines
            .iter()
            .filter_map(|l| progress_line(l, &mut previous))
            .collect();
        assert_eq!(events, [(0, 2), (2, 2)]);
    }

    #[tokio::test]
    async fn helper_output_progress_errors_and_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("helper");
        std::fs::write(
            &script,
            "#!/bin/sh\ncase \"$2\" in\n\
             en-US) echo '{\"type\":\"progress\",\"completed\":0,\"total\":1}' >&2; echo '{\"type\":\"progress\",\"completed\":1,\"total\":1}' >&2; echo '{\"text\":\"Привет 🦀\",\"provider\":\"macOS Speech\",\"onDevice\":true}';;\n\
             de-DE) echo '{\"error\":\"On-device speech is unavailable.\"}'; exit 1;;\n\
             fr-FR) echo 'not json'; exit 1;;\n\
             *) sleep 30;;\nesac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let file = directory.path().join("a.wav");
        let mut events = progress().subscribe();
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        let value = run(&script, &file, "en-US", "one", stopped.clone())
            .await
            .unwrap();
        assert_eq!(value["text"], "Привет 🦀");
        let mut seen = vec![];
        while let Ok(event) = events.try_recv() {
            if event["id"] == "one" {
                seen.push(event["completed"].as_i64().unwrap());
            }
        }
        assert_eq!(seen, [0, 1]);
        assert_eq!(
            run(&script, &file, "de-DE", "two", stopped.clone())
                .await
                .unwrap_err(),
            "On-device speech is unavailable."
        );
        assert!(
            run(&script, &file, "fr-FR", "three", stopped)
                .await
                .unwrap_err()
                .starts_with("Native transcription failed")
        );
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let started = Instant::now();
        let task = tokio::spawn({
            let (script, file) = (script.clone(), file.clone());
            async move { run(&script, &file, "ja-JP", "four", stopped).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.send(true).unwrap();
        assert_eq!(task.await.unwrap().unwrap_err(), CANCELLED);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(
            run(
                &directory.path().join("missing"),
                &file,
                "en-US",
                "five",
                tokio::sync::watch::channel(false).1
            )
            .await
            .unwrap_err()
            .contains("helper is missing")
        );
    }

    #[test]
    fn permits_are_single_use_and_cancel_matches_the_running_identity() {
        let now = Instant::now();
        let permit = prepare(now).unwrap();
        let mut guard = service().lock().unwrap();
        assert!(guard.permits.contains_key(permit.as_str().unwrap()));
        assert!(guard.permits.remove(permit.as_str().unwrap()).is_some());
        assert!(guard.permits.remove(permit.as_str().unwrap()).is_none());
        let (stop, stopped) = tokio::sync::watch::channel(false);
        guard.running = Some(("run".into(), stop));
        drop(guard);
        assert_eq!(cancel(&json!("other")).unwrap(), false);
        assert_eq!(cancel(&json!("run")).unwrap(), true);
        assert!(*stopped.borrow());
        drop(Running("run".into()));
        assert!(service().lock().unwrap().running.is_none());
    }
}
