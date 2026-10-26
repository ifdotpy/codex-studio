//! Microphone capture and recording playback for the Attar UI.
//! Audio stays on native threads. The UI receives 16-bit PCM chunks as data.
// The native paths here run only on macOS; other targets build the module for checks.
#![cfg_attr(
    not(target_os = "macos"),
    allow(dead_code, unused_imports, unused_variables)
)]
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub const DICTATION_RATE: f64 = 16_000.0;
const MICROPHONE_GRANT: Duration = Duration::from_secs(15);
const QUEUE: usize = 256;
/// Maximum native recording duration, including the final device flush. Unit: seconds.
const RECORDING_SECONDS: u64 = 1801;
/// A Stop waits at most this long for its native chunks to commit. Unit: seconds.
const RECORDING_STOP_SECONDS: u64 = 10;

#[derive(Clone)]
struct RecordingCapture {
    store: crate::delivery::StoreSender,
    completion: tokio::sync::watch::Receiver<Option<Result<Value, String>>>,
    synthetic: bool,
    request_id: String,
}
fn recordings() -> &'static Mutex<HashMap<String, RecordingCapture>> {
    static CAPTURES: OnceLock<Mutex<HashMap<String, RecordingCapture>>> = OnceLock::new();
    CAPTURES.get_or_init(Mutex::default)
}
fn recording_sequence() -> &'static tokio::sync::Mutex<()> {
    static SEQUENCE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    SEQUENCE.get_or_init(|| tokio::sync::Mutex::new(()))
}
async fn stop_device(id: &str, flush: bool) -> Result<bool, String> {
    #[cfg(target_os = "macos")]
    {
        let capture = id.to_owned();
        let stopped = mac::on_main(move || Ok(mac::stop(&capture, flush))).await?;
        service()
            .lock()
            .map_err(|_| "The audio service stopped.")?
            .active_captures
            .remove(id);
        Ok(stopped)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (id, flush);
        Ok(false)
    }
}
async fn record_pcm(
    store: &crate::delivery::StoreSender,
    id: &str,
    mut input: mpsc::Receiver<Value>,
) -> Result<(), String> {
    while let Some(value) = input.recv().await {
        if value["stopped"] == true {
            return Ok(());
        }
        let base64 = value["pcm"]
            .as_str()
            .ok_or("The native recording event is invalid.")?;
        crate::delivery::state(
            store,
            "dictation.captureAppend",
            json!({"id":id,"base64":base64}),
        )
        .await?;
    }
    Err("The final native audio chunk was not confirmed. Saved audio remains available.".into())
}
async fn recording_start(
    store: &crate::delivery::StoreSender,
    request: &crate::protocol::Request,
) -> Result<Value, String> {
    let _sequence = recording_sequence().lock().await;
    take_microphone(Instant::now())?;
    {
        let captures = recordings()
            .lock()
            .map_err(|_| "The native recording service stopped.")?;
        if !captures.is_empty() {
            return Err("Stop the existing recording before starting another.".into());
        }
        if captures.len() >= 32 {
            return Err("Close the completed recordings before starting another.".into());
        }
    }
    let (send, receive) = tokio::sync::oneshot::channel();
    store
        .send((
            crate::protocol::Request {
                id: request.id.clone(),
                method: "dictation.capturePrepare".into(),
                params: json!({"chatId":request.params["chatId"]}),
            },
            send,
        ))
        .await
        .map_err(|e| e.to_string())?;
    let response = receive.await.map_err(|e| e.to_string())?;
    let prepared = response.result.ok_or_else(|| {
        response
            .error
            .map(|error| error.message)
            .unwrap_or("The recording could not be saved.".into())
    })?;
    let row = prepared["row"].clone();
    let id = capture_id(&row["id"])?;
    if prepared["replayed"] == true {
        return Err(
            "This recording request already ran. Read its saved recording; it was not repeated."
                .into(),
        );
    }
    #[cfg(test)]
    let synthetic = service()
        .lock()
        .map_err(|_| "The audio service stopped.")?
        .recording_input
        .take();
    #[cfg(not(test))]
    let synthetic: Option<mpsc::Receiver<Value>> = None;
    let is_synthetic = synthetic.is_some();
    let input = if let Some(input) = synthetic {
        input
    } else {
        let input = attach(&json!(id))?;
        let response = dispatch(
            crate::protocol::Request {
                id: format!("{}:capture", request.id),
                method: "media.captureStart".into(),
                params: json!({"capture":id}),
            },
            store,
        );
        // Box the existing device command to break the recursive async dispatch type.
        let response = Box::pin(response).await;
        if let Some(error) = response.error {
            let _ = crate::delivery::state(
                store,
                "dictation.captureFinish",
                json!({"id":id,"error":error.message}),
            )
            .await;
            return Err(error.message);
        }
        input
    };
    let (complete, completion) = tokio::sync::watch::channel(None);
    recordings()
        .lock()
        .map_err(|_| "The native recording service stopped.")?
        .insert(
            id.clone(),
            RecordingCapture {
                store: store.clone(),
                completion,
                synthetic: is_synthetic,
                request_id: request.id.clone(),
            },
        );
    let store = store.clone();
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            Duration::from_secs(RECORDING_SECONDS),
            record_pcm(&store, &id, input),
        )
        .await
        .unwrap_or_else(|_| Err("The 30 minute recording limit was reached.".into()));
        let device_closed = if result.is_err() && !is_synthetic {
            stop_device(&id, false).await.is_ok()
        } else {
            true
        };
        let params = match &result {
            Ok(()) => json!({"id":id}),
            Err(error) => json!({"id":id,"error":error,"uncertain":!device_closed}),
        };
        let saved = crate::delivery::state(&store, "dictation.captureFinish", params).await;
        let outcome = match (result, saved) {
            (Ok(()), Ok(row)) => Ok(row),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(error),
        };
        let _ = complete.send(Some(outcome));
        if device_closed && let Ok(mut captures) = recordings().lock() {
            captures.remove(&id);
        }
    });
    Ok(row)
}
async fn recording_stop(store: &crate::delivery::StoreSender, id: &str) -> Result<Value, String> {
    let _sequence = recording_sequence().lock().await;
    let capture = {
        let captures = recordings()
            .lock()
            .map_err(|_| "The recording service stopped.")?;
        if captures
            .get(id)
            .is_some_and(|capture| !capture.store.same_channel(store))
        {
            return Err("The recording belongs to another server context.".into());
        }
        captures.get(id).cloned()
    };
    let Some(mut capture) = capture else {
        let row =
            crate::delivery::state(store, "dictation.captureSnapshot", json!({"id":id})).await?;
        if row["state"] == "ready" {
            return Ok(row);
        }
        return Err("The native recording result is not confirmed. Read its saved state.".into());
    };
    if !capture.synthetic {
        stop_device(id, true).await?;
    }
    let outcome = tokio::time::timeout(Duration::from_secs(RECORDING_STOP_SECONDS), async {
        loop {
            if let Some(result) = capture.completion.borrow().clone() {
                return result;
            }
            capture
                .completion
                .changed()
                .await
                .map_err(|_| "The native recording stopped before its receipt.")?;
        }
    })
    .await
    .map_err(|_| {
        "The final audio commit is not confirmed. Saved audio remains available.".to_string()
    })?;
    if let Err(error) = &outcome {
        // This Stop did confirm that the device is off, even if a previous
        // capture task could not prove its final PCM commit.
        crate::delivery::state(
            store,
            "dictation.captureFinish",
            json!({"id":id,"error":error}),
        )
        .await?;
        recordings()
            .lock()
            .map_err(|_| "The recording service stopped.")?
            .remove(id);
    }
    outcome
}

/// Drain only this connection's capture before the core drops its Store worker.
pub async fn close_recordings(store: &crate::delivery::StoreSender) {
    let ids = recordings()
        .lock()
        .map(|captures| {
            captures
                .iter()
                .filter(|(_, capture)| capture.store.same_channel(store))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for id in ids {
        let _ = recording_stop(store, &id).await;
    }
}

#[cfg(test)]
pub(crate) static AUDIO_TEST_MUTEX: Mutex<()> = Mutex::new(());

/// Band-limited conversion from the device rate and channels to mono output.
pub struct Resampler {
    step: f64,
    half: usize,
    cutoff: f64,
    buffer: Vec<f32>,
    position: f64,
    // Input samples received, counted from the zero padding at the start.
    end: usize,
}
impl Resampler {
    pub fn new(input_rate: f64, output_rate: f64) -> Self {
        let step = input_rate / output_rate;
        let half = (8.0 * step.max(1.0)).ceil() as usize;
        Self {
            step,
            half,
            // Cycles per input sample. Keeps 90% of the output band.
            cutoff: 0.45 * output_rate.min(input_rate) / input_rate,
            buffer: vec![0.0; half],
            position: half as f64,
            end: half,
        }
    }
    fn sample(&self, at: f64) -> f32 {
        let center = at.floor() as isize;
        let (mut sum, mut weights) = (0.0f64, 0.0f64);
        for k in center - self.half as isize + 1..=center + self.half as isize {
            let d = at - k as f64;
            let sinc = if d == 0.0 {
                2.0 * self.cutoff
            } else {
                (2.0 * std::f64::consts::PI * self.cutoff * d).sin() / (std::f64::consts::PI * d)
            };
            let x = std::f64::consts::PI * d / self.half as f64;
            let window = 0.42 + 0.5 * x.cos() + 0.08 * (2.0 * x).cos();
            let weight = sinc * window;
            sum += self.buffer[k as usize] as f64 * weight;
            weights += weight;
        }
        (sum / weights) as f32
    }
    fn drain(&mut self, limit: f64) -> Vec<i16> {
        let mut output = vec![];
        while self.position + self.half as f64 <= (self.buffer.len() - 1) as f64
            && self.position < limit
        {
            let value = self.sample(self.position);
            output.push((value.clamp(-1.0, 1.0) * 32767.0).round() as i16);
            self.position += self.step;
        }
        let keep = (self.position.floor() as usize).saturating_sub(self.half);
        self.buffer.drain(..keep);
        self.position -= keep as f64;
        self.end -= keep;
        output
    }
    pub fn process(&mut self, input: &[f32]) -> Vec<i16> {
        self.buffer.extend_from_slice(input);
        self.end += input.len();
        self.drain(f64::INFINITY)
    }
    /// Emits the samples that remain before the end of the received input.
    pub fn finish(&mut self) -> Vec<i16> {
        let end = self.end as f64;
        self.buffer.extend(std::iter::repeat_n(0.0, self.half + 1));
        let output = self.drain(end);
        self.end = self.position.floor() as usize;
        output
    }
}

pub fn encode_pcm(samples: &[i16]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(
        samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<_>>(),
    )
}

const RESERVATION: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Service {
    microphone_until: Option<Instant>,
    // A gesture reserves one later microphone request, before any system prompt.
    reservations: HashMap<String, Instant>,
    subscribers: HashMap<String, mpsc::Sender<Value>>,
    active_captures: HashSet<String>,
    playback_active: bool,
    #[cfg(test)]
    recording_input: Option<mpsc::Receiver<Value>>,
}
fn service() -> &'static Mutex<Service> {
    static VALUE: OnceLock<Mutex<Service>> = OnceLock::new();
    VALUE.get_or_init(Mutex::default)
}

/// Recordings and realtime voice share one physical microphone owner.
pub(crate) fn claim_capture(id: &str) -> Result<(), String> {
    let mut service = service().lock().map_err(|_| "The audio service stopped.")?;
    if !service.active_captures.is_empty() {
        return Err("Stop the existing microphone capture first.".into());
    }
    service.active_captures.insert(id.to_owned());
    Ok(())
}
pub(crate) fn release_capture(id: &str) {
    if let Ok(mut service) = service().lock() {
        service.active_captures.remove(id);
    }
}

/// This proof reads native state only. A poisoned owner is never idle.
pub(crate) fn audio_idle() -> bool {
    recordings()
        .lock()
        .is_ok_and(|captures| captures.is_empty())
        && service()
            .lock()
            .is_ok_and(|service| service.active_captures.is_empty() && !service.playback_active)
}

fn capture_id(value: &Value) -> Result<String, String> {
    value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .map(str::to_owned)
        .ok_or("Invalid capture identity.".into())
}

/// A view subscription receives one capture's audio. Dropping it stops capture.
pub fn attach(id: &Value) -> Result<mpsc::Receiver<Value>, String> {
    let id = capture_id(id)?;
    let (sender, receiver) = mpsc::channel(QUEUE);
    let mut service = service().lock().map_err(|_| "The audio service stopped.")?;
    service.subscribers.retain(|_, s| !s.is_closed());
    if service.subscribers.len() >= 4 || service.subscribers.contains_key(&id) {
        return Err("The capture already exists or too many captures are open.".into());
    }
    service.subscribers.insert(id, sender);
    Ok(receiver)
}

pub(crate) fn take_microphone(now: Instant) -> Result<(), String> {
    let service = service().lock().map_err(|_| "The audio service stopped.")?;
    match service.microphone_until {
        Some(until) if now < until => Ok(()),
        _ => Err("Use the Record button again to allow the microphone.".into()),
    }
}

/// Called after the native gesture. The UI may check other conditions before asking macOS.
pub fn reserve_microphone(now: Instant) -> Result<Value, String> {
    let token = crate::file_save::uuid()?;
    let mut service = service().lock().map_err(|_| "The audio service stopped.")?;
    service.reservations.retain(|_, expires| *expires > now);
    if service.reservations.len() >= 4 {
        return Err("Too many microphone requests are waiting.".into());
    }
    service
        .reservations
        .insert(token.clone(), now + RESERVATION);
    Ok(Value::String(token))
}

fn take_reservation(token: &Value, now: Instant) -> Result<(), String> {
    let mut service = service().lock().map_err(|_| "The audio service stopped.")?;
    match token.as_str().and_then(|t| service.reservations.remove(t)) {
        Some(expires) if expires > now => Ok(()),
        _ => Err("Use the button again to allow the microphone.".into()),
    }
}

/// Called after the native gesture for Record or Voice.
pub async fn request_microphone() -> Result<Value, String> {
    if crate::platform::hidden_check() {
        return Err("Hidden checks do not use the microphone.".into());
    }
    #[cfg(target_os = "macos")]
    mac::authorize().await?;
    #[cfg(not(target_os = "macos"))]
    return Err("Microphone capture requires macOS.".into());
    #[cfg(target_os = "macos")]
    {
        service()
            .lock()
            .map_err(|_| "The audio service stopped.")?
            .microphone_until = Some(Instant::now() + MICROPHONE_GRANT);
        Ok(Value::Bool(true))
    }
}

#[path = "media_playback.rs"]
mod playback;
use crate::protocol::{PlaybackReceipt, PlaybackStatus};
use playback::{Playback, PlaybackPlayer};

trait PlaybackDevice: Sync {
    fn play(
        &self,
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        directory: tempfile::TempDir,
    ) -> impl std::future::Future<Output = Result<PlaybackReceipt, String>> + Send;
    fn preview(
        &self,
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        name: String,
        directory: tempfile::TempDir,
    ) -> impl std::future::Future<Output = Result<PlaybackReceipt, String>> + Send;
    fn stop(
        &self,
        owner: crate::delivery::StoreSender,
        token: Option<String>,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}
struct NativePlayback;
impl PlaybackDevice for NativePlayback {
    async fn play(
        &self,
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        directory: tempfile::TempDir,
    ) -> Result<PlaybackReceipt, String> {
        #[cfg(test)]
        {
            return match playback_tests::current_preview_mock() {
                Some(device) => device.play(owner, token, path, directory).await,
                None => Err("Automated checks do not play audio.".into()),
            };
        }
        #[cfg(all(target_os = "macos", not(test)))]
        {
            mac::on_main(move || mac::play(owner, token, path, directory)).await
        }
        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            let _ = (owner, token, path, directory);
            Err("Playback is not available on this device.".into())
        }
    }
    async fn preview(
        &self,
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        name: String,
        directory: tempfile::TempDir,
    ) -> Result<PlaybackReceipt, String> {
        #[cfg(test)]
        {
            return match playback_tests::current_preview_mock() {
                Some(device) => device.preview(owner, token, path, name, directory).await,
                None => Err("Automated checks do not open media.".into()),
            };
        }
        #[cfg(all(target_os = "macos", not(test)))]
        {
            mac::preview(owner, token, path, name, directory).await
        }
        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            let _ = (owner, token, path, name, directory);
            Err("Media preview is not available on this device.".into())
        }
    }
    async fn stop(
        &self,
        owner: crate::delivery::StoreSender,
        token: Option<String>,
    ) -> Result<(), String> {
        #[cfg(test)]
        {
            return match playback_tests::current_preview_mock() {
                Some(device) => device.stop(owner, token).await,
                None => Ok(()),
            };
        }
        #[cfg(all(target_os = "macos", not(test)))]
        {
            mac::on_main(move || mac::stop_playback_for(&owner, token.as_deref())).await
        }
        #[cfg(all(not(target_os = "macos"), not(test)))]
        {
            playback::cancel_pending(&owner, token.as_deref())
        }
    }
}

struct PendingPlayback(String);
impl Drop for PendingPlayback {
    fn drop(&mut self) {
        playback::cancel_native_pending(&self.0);
    }
}
struct PlaybackJob {
    owner: tokio::sync::mpsc::WeakSender<crate::delivery::StoreCommand>,
    token: String,
    task: tokio::task::AbortHandle,
}
fn playback_jobs() -> &'static Mutex<Vec<PlaybackJob>> {
    static JOBS: OnceLock<Mutex<Vec<PlaybackJob>>> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
fn cancel_stale_playback_jobs() {
    if let Ok(mut jobs) = playback_jobs().lock() {
        jobs.retain(|job| {
            let retained = !job.task.is_finished() && playback::preparing_token(&job.token);
            if !retained {
                job.task.abort();
            }
            retained
        });
    }
}
fn cancel_owner_playback_jobs(owner: &crate::delivery::StoreSender) {
    if let Ok(mut jobs) = playback_jobs().lock() {
        jobs.retain(|job| {
            if playback::same_owner(&job.owner, owner) {
                job.task.abort();
                false
            } else {
                true
            }
        });
    }
}
pub fn playback_state(
    owner: &crate::delivery::StoreSender,
    request: &str,
) -> Result<PlaybackReceipt, String> {
    playback::state(owner, request)
}
pub fn watch_playback(
    owner: &crate::delivery::StoreSender,
    request: &str,
) -> Result<tokio::sync::watch::Receiver<PlaybackReceipt>, String> {
    if request.is_empty() {
        return Err("A native playback request identity is required.".into());
    }
    playback::watch(owner, Some(request))
}
pub(crate) fn watch_playback_owner(
    owner: &crate::delivery::StoreSender,
) -> Result<tokio::sync::watch::Receiver<PlaybackReceipt>, String> {
    playback::watch(owner, None)
}

/// The permission and exact cached handle are checked before this call.
pub(crate) fn prepare_preview(
    owner: &crate::delivery::StoreSender,
    request: &str,
    handle: &str,
    name: String,
    bytes: std::sync::Arc<Vec<u8>>,
) -> Result<PlaybackReceipt, String> {
    if crate::platform::hidden_check() {
        return Err("Hidden checks do not play media.".into());
    }
    let reservation = playback::reserve(owner, request, &format!("preview:{handle}"))?;
    if !reservation.fresh {
        return Ok(reservation.receipt);
    }
    cancel_stale_playback_jobs();
    let token = reservation
        .receipt
        .playback_id
        .clone()
        .expect("reserved native playback token");
    let mut jobs = match playback_jobs().lock() {
        Ok(jobs) => jobs,
        Err(_) => {
            playback::cancel_native_pending(&token);
            return Err("The native playback service stopped.".into());
        }
    };
    let (actor, job_token) = (owner.clone(), token.clone());
    let task = tokio::spawn(async move {
        let _pending = PendingPlayback(job_token.clone());
        let stage_token = job_token.clone();
        let staged = tokio::task::spawn_blocking(move || {
            let directory = tempfile::Builder::new()
                .prefix("studio-media-")
                .tempdir()
                .map_err(|e| e.to_string())?;
            let extension = std::path::Path::new(&name)
                .extension()
                .and_then(|s| s.to_str())
                .filter(|s| s.len() <= 16 && s.bytes().all(|c| c.is_ascii_alphanumeric()))
                .unwrap_or("media");
            let path = directory.path().join(format!("media.{extension}"));
            crate::file_save::write_atomic_if(&path, &bytes, || {
                playback::preparing_token(&stage_token)
            })?;
            Ok::<_, String>((path.to_string_lossy().into_owned(), name, directory))
        })
        .await
        .map_err(|e| e.to_string())
        .and_then(|value| value);
        let outcome = match staged {
            Ok((path, name, directory)) if playback::preparing(&actor, &job_token) => {
                NativePlayback
                    .preview(actor.clone(), job_token.clone(), path, name, directory)
                    .await
            }
            Ok(_) => return,
            Err(error) => Err(error),
        };
        if let Err(error) = outcome {
            let _ = playback::fail(&actor, &job_token, error);
        }
    });
    jobs.push(PlaybackJob {
        owner: owner.downgrade(),
        token,
        task: task.abort_handle(),
    });
    Ok(reservation.receipt)
}

pub async fn dispatch(
    request: crate::protocol::Request,
    store: &crate::delivery::StoreSender,
) -> crate::protocol::Response {
    dispatch_with_playback(request, store, &NativePlayback).await
}

async fn dispatch_with_playback(
    request: crate::protocol::Request,
    store: &crate::delivery::StoreSender,
    playback: &impl PlaybackDevice,
) -> crate::protocol::Response {
    use crate::protocol::Response;
    let result = async {
        let p = &request.params;
        match request.method.as_str() {
            "media.recordingStart" => recording_start(store, &request).await,
            "media.recordingStop" => {
                let id = capture_id(&p["recording"])?;
                recording_stop(store, &id).await
            }
            "media.recordingState" => {
                let request_id = p.get("requestId").map(capture_id).transpose()?;
                let id = recordings()
                    .lock()
                    .map_err(|_| "The recording service stopped.")?
                    .iter()
                    .find(|(_, capture)| {
                        capture.store.same_channel(store)
                            && request_id
                                .as_ref()
                                .is_none_or(|id| capture.request_id == *id)
                    })
                    .map(|(id, _)| id.clone());
                if let Some(id) = id {
                    crate::delivery::state(store, "dictation.captureSnapshot", json!({"id":id}))
                        .await
                } else {
                    Ok(Value::Null)
                }
            }
            "media.requestMicrophone" => {
                take_reservation(&p["reservation"], Instant::now())?;
                request_microphone().await
            }
            "media.captureStart" => {
                let id = capture_id(&p["capture"])?;
                take_microphone(Instant::now())?;
                let sender = service()
                    .lock()
                    .map_err(|_| "The audio service stopped.")?
                    .subscribers
                    .remove(&id)
                    .filter(|s| !s.is_closed())
                    .ok_or("Subscribe to the capture before it starts.")?;
                #[cfg(target_os = "macos")]
                {
                    // Track the reservation before the device await. A context
                    // cannot release its microphone lease during startup.
                    claim_capture(&id)?;
                    let capture = id.clone();
                    if let Err(error) = mac::on_main(move || mac::start(capture, sender)).await {
                        release_capture(&id);
                        return Err(error);
                    }
                    Ok(json!({"sampleRate": DICTATION_RATE}))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = sender;
                    Err("Microphone capture requires macOS.".to_owned())
                }
            }
            "media.captureStop" => {
                let id = capture_id(&p["capture"])?;
                stop_device(&id, true).await.map(Value::Bool)
            }
            "media.playRecording" => {
                let recording = capture_id(&p["recording"])?;
                if crate::platform::hidden_check() {return Err("Hidden checks do not play audio.".to_owned());}
                let reserved=playback::reserve(store,&request.id,&format!("recording:{recording}"))?;
                if !reserved.fresh {
                    let receipt=playback::state(store,&request.id)?;
                    return receipt.duration.map(|duration|json!({"duration":duration}))
                        .ok_or_else(||receipt.error.unwrap_or_else(||"The original playback is not confirmed. Read its saved receipt.".into()));
                }
                cancel_stale_playback_jobs();
                let token=reserved.receipt.playback_id.expect("reserved native playback token");
                let _pending=PendingPlayback(token.clone());
                let outcome=async {
                    let directory=tempfile::Builder::new().prefix("codex-studio-playback-").tempdir().map_err(|e|e.to_string())?;
                    let exported=crate::delivery::state(store,"dictation.wav",json!({"id":recording,"directory":directory.path()})).await?;
                    let path=exported["path"].as_str().ok_or("The recording was not exported.")?.to_owned();
                    playback.play(store.clone(),token.clone(),path,directory).await
                }.await;
                match outcome {
                    Ok(receipt) if receipt.status==PlaybackStatus::Playing => receipt.duration.map(|duration|json!({"duration":duration})).ok_or("The native player returned no duration.".into()),
                    Ok(receipt) => Err(receipt.error.unwrap_or_else(||"Playback stopped before it opened.".into())),
                    Err(error) => {playback::fail(store,&token,error.clone())?;Err(error)}
                }
            }
            "media.playbackState" => {
                let request_id=p["requestId"].as_str().filter(|id|!id.is_empty()).ok_or("A native playback request identity is required.")?;
                serde_json::to_value(playback::state(store,request_id)?).map_err(|_|"The native playback receipt could not be encoded.".into())
            }
            "media.stopPlayback" => {
                let token=match p.get("playback") {
                    None=>None,
                    Some(Value::String(token)) if !token.is_empty()=>Some(token.clone()),
                    _=>return Err("The native playback identity is invalid.".into()),
                };
                playback::cancel_pending(store,token.as_deref())?;
                cancel_stale_playback_jobs();
                playback.stop(store.clone(),token).await?;
                Ok(Value::Null)
            }
            _ => Err("Unknown audio command.".to_owned()),
        }
    }
    .await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "media", error),
    }
}

/// Stops every capture and playback when the UI disconnects.
pub fn close_all() {
    playback::cancel_all_pending();
    if let Ok(mut jobs) = playback_jobs().lock() {
        for job in jobs.drain(..) {
            job.task.abort();
        }
    }
    if let Ok(mut service) = service().lock() {
        service.subscribers.clear();
        service.reservations.clear();
        service.microphone_until = None;
    }
    #[cfg(target_os = "macos")]
    dispatch2::DispatchQueue::main().exec_async(|| {
        mac::stop_all();
        mac::stop_playback();
    });
}

/// The host calls this only while it holds this connection's audio lease.
pub async fn close_audio(owner: &crate::delivery::StoreSender) -> Result<(), String> {
    {
        let mut service = service().lock().map_err(|_| "The audio service stopped.")?;
        service.subscribers.clear();
        service.reservations.clear();
        service.microphone_until = None;
    }
    playback::cancel_pending(owner, None)?;
    cancel_owner_playback_jobs(owner);
    NativePlayback.stop(owner.clone(), None).await?;
    #[cfg(all(target_os = "macos", not(test)))]
    mac::on_main(|| {
        mac::stop_all();
        Ok(())
    })
    .await?;
    Ok(())
}

#[cfg(target_os = "macos")]
#[path = "native_media.rs"]
mod native_media;

#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use super::*;
    use block2::RcBlock;
    use objc2::{AnyThread, rc::Retained, runtime::Bool};
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};
    use objc2_avf_audio::{AVAudioEngine, AVAudioPCMBuffer, AVAudioPlayer, AVAudioTime};
    use objc2_foundation::{NSString, NSURL};
    use std::{cell::RefCell, ptr::NonNull, sync::Arc};

    pub async fn on_main<T: Send + 'static>(
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let (reply, receive) = tokio::sync::oneshot::channel();
        dispatch2::DispatchQueue::main().exec_async(move || {
            if !reply.is_closed() {
                let _ = reply.send(objc2::rc::autoreleasepool(|_| work()));
            }
        });
        receive
            .await
            .map_err(|_| "The audio action stopped without a result.".to_owned())?
    }

    pub async fn authorize() -> Result<(), String> {
        let denied =
            "Microphone access was denied. Allow Codex Studio in macOS Privacy & Security.";
        let media = unsafe { AVMediaTypeAudio }.ok_or("Audio capture is unavailable.")?;
        let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(media) };
        if status == AVAuthorizationStatus::Authorized {
            return Ok(());
        }
        if status != AVAuthorizationStatus::NotDetermined {
            return Err(denied.into());
        }
        let receive = {
            let (reply, receive) = tokio::sync::oneshot::channel();
            let reply = Mutex::new(Some(reply));
            let handler = RcBlock::new(move |granted: Bool| {
                if let Some(reply) = reply.lock().ok().and_then(|mut r| r.take()) {
                    let _ = reply.send(granted.as_bool());
                }
            });
            unsafe {
                AVCaptureDevice::requestAccessForMediaType_completionHandler(media, &handler)
            };
            receive
        };
        if receive.await.unwrap_or(false) {
            Ok(())
        } else {
            Err(denied.into())
        }
    }

    struct Shared {
        resampler: Resampler,
        sender: mpsc::Sender<Value>,
        failed: bool,
    }
    struct Session {
        engine: Retained<AVAudioEngine>,
        shared: Arc<Mutex<Shared>>,
    }
    thread_local! {
        static SESSIONS: RefCell<HashMap<String, Session>> = RefCell::new(HashMap::new());
        static PLAYER: RefCell<Playback<NativePlayer>> = const { RefCell::new(Playback::new()) };
    }

    pub fn start(id: String, sender: mpsc::Sender<Value>) -> Result<(), String> {
        let engine = unsafe { AVAudioEngine::new() };
        let input = unsafe { engine.inputNode() };
        let format = unsafe { input.outputFormatForBus(0) };
        let (rate, channels) = unsafe { (format.sampleRate(), format.channelCount() as usize) };
        if rate.is_nan() || rate < 8000.0 || channels == 0 {
            return Err("No microphone input is available.".into());
        }
        let shared = Arc::new(Mutex::new(Shared {
            resampler: Resampler::new(rate, DICTATION_RATE),
            sender,
            failed: false,
        }));
        let (tap, tap_id) = (shared.clone(), id.clone());
        let block = RcBlock::new(
            move |buffer: NonNull<AVAudioPCMBuffer>, _time: NonNull<AVAudioTime>| {
                let buffer = unsafe { buffer.as_ref() };
                let frames = unsafe { buffer.frameLength() } as usize;
                let data = unsafe { buffer.floatChannelData() };
                if data.is_null() || frames == 0 {
                    return;
                }
                let mut mono = vec![0f32; frames];
                for channel in 0..channels {
                    let samples = unsafe {
                        std::slice::from_raw_parts((*data.add(channel)).as_ptr(), frames)
                    };
                    for (out, value) in mono.iter_mut().zip(samples) {
                        *out += value / channels as f32;
                    }
                }
                let Ok(mut shared) = tap.lock() else { return };
                if shared.failed {
                    return;
                }
                let pcm = shared.resampler.process(&mono);
                if pcm.is_empty() {
                    return;
                }
                // A closed or stalled UI stops the microphone instead of buffering audio.
                if shared
                    .sender
                    .try_send(json!({"pcm": encode_pcm(&pcm)}))
                    .is_err()
                {
                    shared.failed = true;
                    let id = tap_id.clone();
                    dispatch2::DispatchQueue::main().exec_async(move || {
                        stop(&id, false);
                    });
                }
            },
        );
        unsafe {
            input.installTapOnBus_bufferSize_format_block(
                0,
                4096,
                Some(&format),
                RcBlock::as_ptr(&block),
            );
            engine.prepare();
        }
        if let Err(error) = unsafe { engine.startAndReturnError() } {
            unsafe { input.removeTapOnBus(0) };
            return Err(error.localizedDescription().to_string());
        }
        SESSIONS.with_borrow_mut(|sessions| sessions.insert(id, Session { engine, shared }));
        Ok(())
    }

    /// Stops the tap first, then sends the final samples and a stop marker.
    pub fn stop(id: &str, flush: bool) -> bool {
        let Some(session) = SESSIONS.with_borrow_mut(|sessions| sessions.remove(id)) else {
            return false;
        };
        unsafe {
            session.engine.inputNode().removeTapOnBus(0);
            session.engine.stop();
        }
        if let Ok(mut shared) = session.shared.lock() {
            if flush && !shared.failed {
                let pcm = shared.resampler.finish();
                if !pcm.is_empty() {
                    let _ = shared.sender.try_send(json!({"pcm": encode_pcm(&pcm)}));
                }
                let _ = shared.sender.try_send(json!({"stopped": true}));
            }
            shared.failed = true;
        }
        release_capture(id);
        true
    }

    pub fn stop_all() {
        let ids: Vec<String> = SESSIONS.with_borrow(|s| s.keys().cloned().collect());
        for id in ids {
            stop(&id, false);
        }
    }

    enum NativePlayer {
        Recording {
            player: Retained<AVAudioPlayer>,
            _directory: tempfile::TempDir,
        },
        Media(super::native_media::Player),
    }
    impl PlaybackPlayer for NativePlayer {
        fn stop(self) {
            match self {
                Self::Recording { player, .. } => unsafe { player.stop() },
                Self::Media(player) => player.stop(),
            }
        }
    }
    pub fn play(
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        directory: tempfile::TempDir,
    ) -> Result<PlaybackReceipt, String> {
        PLAYER.with_borrow_mut(|current| {
            current.finish(owner, &token, || {
                let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
                let player = unsafe {
                    AVAudioPlayer::initWithContentsOfURL_error(AVAudioPlayer::alloc(), &url)
                }
                .map_err(|_| "The recording could not be prepared.")?;
                if !unsafe { player.play() } {
                    return Err("The recording could not be played.".into());
                }
                let duration = unsafe { player.duration() };
                if !duration.is_finite() || duration < 0. {
                    unsafe { player.stop() };
                    return Err("The recording has no valid duration.".into());
                }
                Ok((
                    NativePlayer::Recording {
                        player,
                        _directory: directory,
                    },
                    Some(duration),
                ))
            })
        })
    }
    pub async fn preview(
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        name: String,
        directory: tempfile::TempDir,
    ) -> Result<PlaybackReceipt, String> {
        super::native_media::open(owner, token, path, name, directory).await
    }
    pub(super) fn finish_media(
        owner: crate::delivery::StoreSender,
        token: String,
        resources: super::native_media::Resources,
        name: String,
    ) -> Result<PlaybackReceipt, String> {
        PLAYER.with_borrow_mut(|current| {
            current.finish(owner, &token, || {
                let player = super::native_media::Player::create(resources, &name, &token)?;
                Ok((NativePlayer::Media(player), None))
            })
        })
    }
    pub(super) fn close_media_token(token: &str) {
        PLAYER.with_borrow_mut(|current| current.stop_native_token(token));
    }
    pub(super) fn fail_media_if_needed(token: &str) {
        let failed = PLAYER.with_borrow(|current| {
            current
                .with_player(
                    token,
                    |player| matches!(player,NativePlayer::Media(player) if player.failed()),
                )
                .unwrap_or(false)
        });
        if failed {
            PLAYER.with_borrow_mut(|current| {
                current.fail_token(
                    token,
                    "The native media player stopped with an error.".into(),
                )
            });
        }
    }
    pub fn stop_playback_for(
        owner: &crate::delivery::StoreSender,
        token: Option<&str>,
    ) -> Result<(), String> {
        super::native_media::cancel_owner(owner, token);
        PLAYER.with_borrow_mut(|current| current.stop_token(owner, token))
    }
    pub fn stop_playback() {
        super::native_media::cancel_all();
        PLAYER.with_borrow_mut(Playback::stop_all);
    }
}

#[cfg(test)]
#[path = "media_playback_tests.rs"]
pub(crate) mod playback_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_recording_capture_commits_pcm_without_a_renderer_byte_roundtrip() {
        let _serial = AUDIO_TEST_MUTEX.lock().unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let folder = tempfile::tempdir().unwrap();
            let path = folder.path().join("client.sqlite3");
            let (store, mut commands) = mpsc::channel::<crate::delivery::StoreCommand>(16);
            let worker = std::thread::spawn(move || {
                let mut db = crate::store::Store::open(&path).unwrap();
                while let Some((request, reply)) = commands.blocking_recv() {
                    let _ = reply.send(db.dispatch(request));
                }
            });
            let (input, receiver) = mpsc::channel(4);
            {
                let mut service = service().lock().unwrap();
                service.microphone_until = Some(Instant::now() + MICROPHONE_GRANT);
                service.recording_input = Some(receiver);
            }
            let request = |id: &str, method: &str, params| crate::protocol::Request {
                id: id.into(),
                method: method.into(),
                params,
            };
            let started = dispatch(
                request("record", "media.recordingStart", json!({"chatId":"chat"})),
                &store,
            )
            .await;
            assert!(started.error.is_none(), "{started:?}");
            let row = started.result.unwrap();
            let id = row["id"].as_str().unwrap();
            assert_eq!(row["captureRequestId"], "record");
            let owned = dispatch(
                request(
                    "read",
                    "media.recordingState",
                    json!({"requestId":"record"}),
                ),
                &store,
            )
            .await
            .result
            .unwrap();
            assert_eq!(owned["id"], id);
            assert!(
                dispatch(
                    request(
                        "wrong-read",
                        "media.recordingState",
                        json!({"requestId":"other-start"})
                    ),
                    &store
                )
                .await
                .result
                .unwrap()
                .is_null()
            );
            let (sibling, _receiver) = mpsc::channel(1);
            assert!(
                dispatch(
                    request(
                        "sibling-stop",
                        "media.recordingStop",
                        json!({"recording":id})
                    ),
                    &sibling
                )
                .await
                .error
                .is_some()
            );
            input.send(json!({"pcm":"AQD/fw=="})).await.unwrap();
            input.send(json!({"stopped":true})).await.unwrap();
            drop(input);
            let stopped = dispatch(
                request("stop", "media.recordingStop", json!({"recording":id})),
                &store,
            )
            .await;
            assert!(stopped.error.is_none(), "{stopped:?}");
            assert_eq!(stopped.result.as_ref().unwrap()["samples"], 2);
            assert_eq!(stopped.result.as_ref().unwrap()["state"], "ready");
            let rows = crate::delivery::state(&store, "dictation.list", json!({"chatId":"chat"}))
                .await
                .unwrap();
            assert_eq!(rows[0], stopped.result.unwrap());
            drop(store);
            worker.join().unwrap();
        });
    }
    fn tone(rate: f64, frequency: f64, seconds: f64) -> Vec<f32> {
        (0..(rate * seconds) as usize)
            .map(|n| {
                (0.5 * (2.0 * std::f64::consts::PI * frequency * n as f64 / rate).sin()) as f32
            })
            .collect()
    }
    fn rms(samples: &[i16]) -> f64 {
        (samples
            .iter()
            .map(|&s| (s as f64 / 32767.0).powi(2))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
    }
    #[test]
    fn resampler_keeps_speech_band_and_removes_aliases_in_chunks() {
        for rate in [48_000.0, 44_100.0, 16_000.0] {
            let input = tone(rate, 1000.0, 1.0);
            let mut resampler = Resampler::new(rate, DICTATION_RATE);
            let mut output = vec![];
            for chunk in input.chunks(1024) {
                output.extend(resampler.process(chunk));
            }
            output.extend(resampler.finish());
            assert!(
                (output.len() as i64 - 16_000).abs() <= 1,
                "{rate}: {}",
                output.len()
            );
            let middle = &output[2000..14000];
            assert!(
                (rms(middle) - 0.5 / 2f64.sqrt()).abs() < 0.01,
                "{rate}: {}",
                rms(middle)
            );
            let crossings = middle
                .windows(2)
                .filter(|w| (w[0] < 0) != (w[1] < 0))
                .count();
            assert!((crossings as i64 - 1500).abs() <= 2, "{rate}: {crossings}");
        }
        for rate in [48_000.0, 44_100.0] {
            let mut resampler = Resampler::new(rate, DICTATION_RATE);
            let output = resampler.process(&tone(rate, 12_000.0, 1.0));
            assert!(
                rms(&output[1000..]) < 0.005,
                "{rate}: {}",
                rms(&output[1000..])
            );
        }
        let mut resampler = Resampler::new(48_000.0, DICTATION_RATE);
        assert!(resampler.process(&[]).is_empty());
        assert_eq!(resampler.finish().len(), 0);
        assert_eq!(encode_pcm(&[1, -2]), "AQD+/w==");
    }
    #[test]
    fn capture_requires_a_fresh_microphone_grant_and_one_subscriber() {
        let _serial = AUDIO_TEST_MUTEX.lock().unwrap();
        let now = Instant::now();
        service().lock().unwrap().microphone_until = None;
        assert!(take_microphone(now).is_err());
        service().lock().unwrap().microphone_until = Some(now + MICROPHONE_GRANT);
        assert!(take_microphone(now).is_ok());
        assert!(take_microphone(now + MICROPHONE_GRANT).is_err());
        let reservation = reserve_microphone(now).unwrap();
        assert!(take_reservation(&json!("other"), now).is_err());
        assert!(
            take_reservation(&reservation, now + RESERVATION).is_err(),
            "expired"
        );
        let reservation = reserve_microphone(now).unwrap();
        take_reservation(&reservation, now).unwrap();
        assert!(take_reservation(&reservation, now).is_err(), "single use");
        let receiver = attach(&json!("capture-test")).unwrap();
        assert!(attach(&json!("capture-test")).is_err());
        drop(receiver);
        assert!(attach(&json!("capture-test")).is_ok());
        assert!(attach(&json!("")).is_err());
    }
}
