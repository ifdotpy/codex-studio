//! Microphone capture and recording playback for the Attar UI.
//! Audio stays on native threads. The UI receives 16-bit PCM chunks as data.
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub const DICTATION_RATE: f64 = 16_000.0;
#[cfg(any(target_os = "macos", test))]
const MICROPHONE_GRANT: Duration = Duration::from_secs(15);
const QUEUE: usize = 256;

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
}
fn service() -> &'static Mutex<Service> {
    static VALUE: OnceLock<Mutex<Service>> = OnceLock::new();
    VALUE.get_or_init(Mutex::default)
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

pub async fn dispatch(
    request: crate::protocol::Request,
    store: &crate::delivery::StoreSender,
) -> crate::protocol::Response {
    use crate::protocol::Response;
    let result = async {
        let p = &request.params;
        match request.method.as_str() {
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
                    mac::on_main(move || mac::start(id, sender)).await?;
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
                #[cfg(target_os = "macos")]
                {
                    Ok(Value::Bool(
                        mac::on_main(move || Ok(mac::stop(&id, true))).await?,
                    ))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = id;
                    Ok(Value::Bool(false))
                }
            }
            "media.playRecording" => {
                let recording = capture_id(&p["recording"])?;
                if crate::platform::hidden_check() {
                    return Err("Hidden checks do not play audio.".to_owned());
                }
                let directory = tempfile::Builder::new()
                    .prefix("codex-studio-playback-")
                    .tempdir()
                    .map_err(|e| e.to_string())?;
                let exported = crate::delivery::state(
                    store,
                    "dictation.wav",
                    json!({"id":recording,"directory":directory.path()}),
                )
                .await?;
                let path = exported["path"]
                    .as_str()
                    .ok_or("The recording was not exported.")?
                    .to_owned();
                #[cfg(target_os = "macos")]
                {
                    let duration = mac::on_main(move || mac::play(path, directory)).await?;
                    Ok(json!({"duration":duration}))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = (path, directory);
                    Err("Playback requires macOS.".to_owned())
                }
            }
            "media.stopPlayback" => {
                #[cfg(target_os = "macos")]
                mac::on_main(|| {
                    mac::stop_playback();
                    Ok(())
                })
                .await?;
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
        static PLAYER: RefCell<Option<(Retained<AVAudioPlayer>, tempfile::TempDir)>> = const { RefCell::new(None) };
    }

    pub fn start(id: String, sender: mpsc::Sender<Value>) -> Result<(), String> {
        let engine = unsafe { AVAudioEngine::new() };
        let input = unsafe { engine.inputNode() };
        let format = unsafe { input.outputFormatForBus(0) };
        let (rate, channels) = unsafe { (format.sampleRate(), format.channelCount() as usize) };
        if !(rate >= 8000.0) || channels == 0 {
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
        true
    }

    pub fn stop_all() {
        let ids: Vec<String> = SESSIONS.with_borrow(|s| s.keys().cloned().collect());
        for id in ids {
            stop(&id, false);
        }
    }

    pub fn play(path: String, directory: tempfile::TempDir) -> Result<f64, String> {
        stop_playback();
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path));
        let player =
            unsafe { AVAudioPlayer::initWithContentsOfURL_error(AVAudioPlayer::alloc(), &url) }
                .map_err(|e| e.localizedDescription().to_string())?;
        if !unsafe { player.play() } {
            return Err("The recording could not be played.".into());
        }
        let duration = unsafe { player.duration() };
        PLAYER.with_borrow_mut(|current| *current = Some((player, directory)));
        Ok(duration)
    }

    pub fn stop_playback() {
        if let Some((player, _directory)) = PLAYER.with_borrow_mut(Option::take) {
            unsafe { player.stop() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
