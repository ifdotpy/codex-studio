//! Realtime voice peer. Rust owns WebRTC, Opus and the audio frames.
//! The UI sends start, answer, mute and end commands and receives events.
//! The server keeps signaling: the UI posts this offer to /api/voice/start.
use bytes::Bytes;
use rtc::media::Sample;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use webrtc::{
    data_channel::DataChannelEvent,
    media_stream::{
        MediaStreamTrack,
        track_local::{TrackLocal, static_sample::TrackLocalStaticSample},
        track_remote::{TrackRemote, TrackRemoteEvent},
    },
    peer_connection::{
        MediaEngine, PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler,
        RTCConfigurationBuilder, RTCIceGatheringState, RTCPeerConnectionState,
        RTCSessionDescription, Registry, register_default_interceptors,
    },
    rtp_transceiver::RtpSender,
};

pub const RATE: usize = 48_000;
pub const FRAME: usize = RATE / 50; // 20 ms
const GATHER_TIMEOUT: Duration = Duration::from_secs(5);
pub type Playback = Arc<dyn Fn(&[f32]) + Send + Sync>;

pub struct Encoder(*mut unsafe_libopus::OpusEncoder);
// The codec state has no thread affinity. One task owns it at a time.
unsafe impl Send for Encoder {}
impl Encoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let state = unsafe {
            unsafe_libopus::opus_encoder_create(
                RATE as i32,
                1,
                unsafe_libopus::OPUS_APPLICATION_VOIP,
                &mut error,
            )
        };
        if state.is_null() || error != 0 {
            return Err(format!("The Opus encoder failed ({error})."));
        }
        Ok(Self(state))
    }
    /// Encodes one 20 ms mono frame.
    pub fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>, String> {
        if frame.len() != FRAME {
            return Err("An Opus frame must contain 20 ms of audio.".into());
        }
        let mut packet = vec![0u8; 1500];
        let length = unsafe {
            unsafe_libopus::opus_encode(
                self.0,
                frame.as_ptr(),
                FRAME as i32,
                packet.as_mut_ptr(),
                packet.len() as i32,
            )
        };
        if length < 0 {
            return Err(format!("Opus encoding failed ({length})."));
        }
        packet.truncate(length as usize);
        Ok(packet)
    }
}
impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { unsafe_libopus::opus_encoder_destroy(self.0) };
    }
}

pub struct Decoder(*mut unsafe_libopus::OpusDecoder);
unsafe impl Send for Decoder {}
impl Decoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let state = unsafe { unsafe_libopus::opus_decoder_create(RATE as i32, 2, &mut error) };
        if state.is_null() || error != 0 {
            return Err(format!("The Opus decoder failed ({error})."));
        }
        Ok(Self(state))
    }
    /// Decodes one packet to mono samples. Stereo input is averaged.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<f32>, String> {
        const MAX: usize = RATE * 120 / 1000;
        let mut pcm = vec![0i16; MAX * 2];
        let frames = unsafe {
            unsafe_libopus::opus_decode(
                self.0,
                packet.as_ptr(),
                packet.len() as i32,
                pcm.as_mut_ptr(),
                MAX as i32,
                0,
            )
        };
        if frames < 0 {
            return Err(format!("Opus decoding failed ({frames})."));
        }
        Ok(pcm[..frames as usize * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| (pair[0] as f32 + pair[1] as f32) / 65_536.0)
            .collect())
    }
}
impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { unsafe_libopus::opus_decoder_destroy(self.0) };
    }
}

/// Splits 48 kHz mono audio into exact 20 ms frames of 16-bit samples.
#[derive(Default)]
pub struct Framer(Vec<i16>);
impl Framer {
    pub fn push(&mut self, samples: &[f32], muted: bool) -> Vec<Vec<i16>> {
        self.0.extend(samples.iter().map(|&s| {
            if muted {
                0
            } else {
                (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
            }
        }));
        let count = self.0.len() / FRAME;
        let frames = self
            .0
            .as_chunks::<FRAME>()
            .0
            .iter()
            .map(|f| f.to_vec())
            .collect();
        self.0.drain(..count * FRAME);
        frames
    }
}

#[derive(Clone)]
struct Handler {
    events: mpsc::Sender<Value>,
    gathered: mpsc::Sender<()>,
    playback: Playback,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gathered.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let state = match state {
            RTCPeerConnectionState::Connected => "connected",
            RTCPeerConnectionState::Disconnected => "disconnected",
            RTCPeerConnectionState::Failed => "failed",
            RTCPeerConnectionState::Closed => "closed",
            _ => "connecting",
        };
        let _ = self
            .events
            .send(json!({"type":"state","state":state}))
            .await;
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if track.kind().await != RtpCodecKind::Audio {
            return;
        }
        let (playback, events) = (self.playback.clone(), self.events.clone());
        tokio::spawn(async move {
            let mut decoder = match Decoder::new() {
                Ok(value) => value,
                Err(error) => {
                    let _ = events.send(json!({"type":"error","message":error})).await;
                    return;
                }
            };
            while let Some(event) = track.poll().await {
                if let TrackRemoteEvent::OnRtpPacket(packet) = event {
                    // A damaged packet is skipped; later packets still play.
                    if let Ok(samples) = decoder.decode(&packet.payload) {
                        playback(&samples);
                    }
                }
            }
        });
    }
}

pub struct Peer {
    connection: Arc<dyn PeerConnection>,
    tasks: Vec<JoinHandle<()>>,
}

async fn payload_type(sender: &Arc<dyn RtpSender>) -> Option<u8> {
    sender
        .get_parameters()
        .await
        .ok()?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
}

impl Peer {
    /// Builds the peer, starts sending microphone frames and returns the complete offer.
    /// `udp` is the local bind address; production uses every interface.
    pub async fn create(
        udp: &str,
        events: mpsc::Sender<Value>,
        mut microphone: mpsc::Receiver<Vec<f32>>,
        playback: Playback,
        muted: Arc<AtomicBool>,
    ) -> Result<(Self, String), String> {
        let error = |e: webrtc::error::Error| e.to_string();
        let mut media = MediaEngine::default();
        media.register_default_codecs().map_err(error)?;
        let registry = register_default_interceptors(Registry::new(), &mut media).map_err(error)?;
        let (gathered, mut gathered_receiver) = mpsc::channel(1);
        // Like the browser client, no STUN server: the realtime service answers with its address.
        let connection: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(RTCConfigurationBuilder::new().build())
                .with_media_engine(media)
                .with_interceptor_registry(registry)
                .with_handler(Arc::new(Handler {
                    events: events.clone(),
                    gathered,
                    playback,
                }))
                .with_udp_addrs(vec![udp.to_owned()])
                .build()
                .await
                .map_err(error)?,
        );
        let ssrc = rand_ssrc()?;
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "studio-voice".into(),
                    "studio-microphone".into(),
                    "Microphone".into(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters {
                            ssrc: Some(ssrc),
                            ..Default::default()
                        },
                        codec: RTCRtpCodec {
                            mime_type: "audio/opus".into(),
                            clock_rate: RATE as u32,
                            channels: 2,
                            sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
                            rtcp_feedback: vec![],
                        },
                        ..Default::default()
                    }],
                ),
            )
            .map_err(error)?,
        );
        let sender = connection
            .add_track(track.clone() as Arc<dyn TrackLocal>)
            .await
            .map_err(error)?;
        let channel = connection
            .create_data_channel("oai-events", None)
            .await
            .map_err(error)?;
        let mut tasks = vec![];
        let channel_events = events.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(event) = channel.poll().await {
                let value = match event {
                    DataChannelEvent::OnOpen => json!({"type":"channel","state":"open"}),
                    DataChannelEvent::OnMessage(message) => json!({
                        "type":"message",
                        "data": String::from_utf8_lossy(&message.data),
                    }),
                    DataChannelEvent::OnClose => {
                        let _ = channel_events
                            .send(json!({"type":"channel","state":"closed"}))
                            .await;
                        break;
                    }
                    _ => continue,
                };
                if channel_events.send(value).await.is_err() {
                    break;
                }
            }
        }));
        tasks.push(tokio::spawn(async move {
            let mut encoder = match Encoder::new() {
                Ok(value) => value,
                Err(error) => {
                    let _ = events.send(json!({"type":"error","message":error})).await;
                    return;
                }
            };
            let mut framer = Framer::default();
            let mut payload = None;
            while let Some(samples) = microphone.recv().await {
                for frame in framer.push(&samples, muted.load(Ordering::Relaxed)) {
                    // Frames before negotiation have no payload type and are dropped.
                    if payload.is_none() {
                        payload = payload_type(&sender).await;
                    }
                    let (Some(payload), Ok(packet)) = (payload, encoder.encode(&frame)) else {
                        continue;
                    };
                    let _ = track
                        .sample_writer(ssrc, payload)
                        .write_sample(&Sample {
                            data: Bytes::from(packet),
                            duration: Duration::from_millis(20),
                            ..Sample::new(Instant::now())
                        })
                        .await;
                }
            }
            // The microphone ended. The UI shows the reason and closes the session.
            let _ = events
                .send(json!({"type":"microphone","state":"ended"}))
                .await;
        }));
        let offer = connection.create_offer(None).await.map_err(error)?;
        connection
            .set_local_description(offer)
            .await
            .map_err(error)?;
        let _ = tokio::time::timeout(GATHER_TIMEOUT, gathered_receiver.recv()).await;
        let sdp = connection
            .local_description()
            .await
            .ok_or("The voice offer is missing.")?
            .sdp;
        Ok((Self { connection, tasks }, sdp))
    }

    pub async fn answer(&self, sdp: String) -> Result<(), String> {
        if !sdp.starts_with("v=0") || sdp.len() > 100_000 {
            return Err("Invalid WebRTC answer.".into());
        }
        let answer = RTCSessionDescription::answer(sdp).map_err(|e| e.to_string())?;
        self.connection
            .set_remote_description(answer)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn close(self) {
        for task in &self.tasks {
            task.abort();
        }
        let _ = self.connection.close().await;
    }
}

/// One voice session at a time. The UI subscribes to its events before creating it.
#[derive(Default)]
struct Service {
    active: Option<Active>,
}
type Subscribers = std::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<Value>>>;
fn subscribers() -> &'static Subscribers {
    static VALUE: std::sync::OnceLock<Subscribers> = std::sync::OnceLock::new();
    VALUE.get_or_init(Default::default)
}
struct Active {
    session: String,
    peer: Option<Peer>,
    muted: Arc<AtomicBool>,
}
fn service() -> &'static tokio::sync::Mutex<Service> {
    static VALUE: std::sync::OnceLock<tokio::sync::Mutex<Service>> = std::sync::OnceLock::new();
    VALUE.get_or_init(Default::default)
}
fn session_id(value: &Value) -> Result<String, String> {
    value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 120)
        .map(str::to_owned)
        .ok_or("Invalid voice session identity.".into())
}

/// A view subscription receives one session's events.
pub fn attach(session: &Value) -> Result<mpsc::Receiver<Value>, String> {
    let session = session_id(session)?;
    let (sender, receiver) = mpsc::channel(256);
    let mut subscribers = subscribers()
        .lock()
        .map_err(|_| "The voice service stopped.")?;
    subscribers.retain(|_, s| !s.is_closed());
    if subscribers.len() >= 4 || subscribers.contains_key(&session) {
        return Err("The voice session already has a subscriber.".into());
    }
    subscribers.insert(session, sender);
    Ok(receiver)
}

async fn stop(active: Active) {
    #[cfg(target_os = "macos")]
    {
        let session = active.session.clone();
        let _ =
            crate::media::mac::on_main(move || Ok(crate::voice_audio::mac::stop(&session))).await;
    }
    if let Some(peer) = active.peer {
        peer.close().await;
    }
}

async fn create(params: &Value) -> Result<Value, String> {
    let session = session_id(&params["session"])?;
    if crate::platform::hidden_check() {
        return Err("Hidden checks do not use the microphone.".into());
    }
    crate::media::take_microphone(Instant::now())?;
    let mut state = service().lock().await;
    if state.active.is_some() {
        return Err("End the existing voice session first.".into());
    }
    let events = subscribers()
        .lock()
        .map_err(|_| "The voice service stopped.")?
        .remove(&session)
        .filter(|s| !s.is_closed())
        .ok_or("Subscribe to the voice session before it starts.")?;
    let muted = Arc::new(AtomicBool::new(params["muted"].as_bool().unwrap_or(false)));
    // Reserve the session before any await, so a second start cannot pass.
    state.active = Some(Active {
        session: session.clone(),
        peer: None,
        muted: muted.clone(),
    });
    drop(state);
    let result = async {
        #[cfg(not(target_os = "macos"))]
        {
            drop(events);
            Err::<(Peer, String), String>("Voice requires macOS.".into())
        }
        #[cfg(target_os = "macos")]
        {
            let (microphone, microphone_receiver) = mpsc::channel(64);
            let playback =
                crate::voice_audio::Playback::new(crate::voice_audio::VOICE_RATE as usize * 2);
            let (id, device_playback, device_muted, ended) = (
                session.clone(),
                playback.clone(),
                muted.clone(),
                events.clone(),
            );
            crate::media::mac::on_main(move || {
                crate::voice_audio::mac::start(
                    id,
                    microphone,
                    device_playback,
                    device_muted,
                    Box::new(move |_| {
                        let _ = ended.try_send(json!({"type":"microphone","state":"ended"}));
                    }),
                )
            })
            .await?;
            Peer::create(
                "0.0.0.0:0",
                events,
                microphone_receiver,
                Arc::new(move |samples: &[f32]| playback.push(samples)),
                muted.clone(),
            )
            .await
        }
    }
    .await;
    let mut state = service().lock().await;
    match result {
        Ok((peer, sdp)) => match state.active.as_mut() {
            Some(active) if active.session == session => {
                active.peer = Some(peer);
                Ok(json!({"sdp": sdp}))
            }
            // The UI ended the session while the offer was prepared.
            _ => {
                drop(state);
                peer.close().await;
                Err("Voice ended.".into())
            }
        },
        Err(error) => {
            if let Some(active) = state.active.take_if(|a| a.session == session) {
                drop(state);
                stop(active).await;
            }
            Err(error)
        }
    }
}

pub async fn dispatch(request: crate::protocol::Request) -> crate::protocol::Response {
    use crate::protocol::Response;
    let result = async {
        let p = &request.params;
        match request.method.as_str() {
            "voice.create" => create(p).await,
            "voice.answer" => {
                let session = session_id(&p["session"])?;
                let sdp = p["sdp"]
                    .as_str()
                    .ok_or("Invalid WebRTC answer.")?
                    .to_owned();
                let state = service().lock().await;
                let peer = state
                    .active
                    .as_ref()
                    .filter(|a| a.session == session)
                    .and_then(|a| a.peer.as_ref())
                    .ok_or("The voice session ended.")?;
                peer.answer(sdp).await.map(|_| Value::Null)
            }
            "voice.mute" => {
                let session = session_id(&p["session"])?;
                let muted = p["muted"].as_bool().ok_or("Invalid mute state.")?;
                let state = service().lock().await;
                let active = state
                    .active
                    .as_ref()
                    .filter(|a| a.session == session)
                    .ok_or("The voice session ended.")?;
                active.muted.store(muted, Ordering::Relaxed);
                Ok(Value::Bool(muted))
            }
            "voice.close" => {
                let session = session_id(&p["session"])?;
                let active = service()
                    .lock()
                    .await
                    .active
                    .take_if(|a| a.session == session);
                let closed = active.is_some();
                if let Some(active) = active {
                    stop(active).await;
                }
                Ok(Value::Bool(closed))
            }
            _ => Err("Unknown voice command.".to_owned()),
        }
    }
    .await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "voice", error),
    }
}

/// Ends voice when the UI disconnects. The microphone never outlives the UI.
pub async fn close_all() {
    let active = {
        if let Ok(mut subscribers) = subscribers().lock() {
            subscribers.clear();
        }
        service().lock().await.active.take()
    };
    if let Some(active) = active {
        stop(active).await;
    }
}

fn rand_ssrc() -> Result<u32, String> {
    let mut bytes = [0u8; 4];
    if unsafe { libc::getentropy(bytes.as_mut_ptr().cast(), bytes.len()) } != 0 {
        return Err("Cannot create a media identity.".into());
    }
    Ok(u32::from_ne_bytes(bytes) | 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BytesMut;
    use std::sync::Mutex;
    use webrtc::data_channel::DataChannel;
    use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;

    fn tone(frequency: f64, samples: usize, start: usize) -> Vec<f32> {
        (start..start + samples)
            .map(|n| {
                (0.4 * (2.0 * std::f64::consts::PI * frequency * n as f64 / RATE as f64).sin())
                    as f32
            })
            .collect()
    }

    #[test]
    fn opus_round_trip_keeps_a_voice_tone_and_framer_is_exact() {
        let (mut encoder, mut decoder) = (Encoder::new().unwrap(), Decoder::new().unwrap());
        let mut framer = Framer::default();
        let mut output = vec![];
        for chunk in tone(440.0, RATE, 0).chunks(700) {
            for frame in framer.push(chunk, false) {
                output.extend(decoder.decode(&encoder.encode(&frame).unwrap()).unwrap());
            }
        }
        assert_eq!(output.len(), RATE / FRAME * FRAME);
        let middle = &output[RATE / 4..RATE * 3 / 4];
        let crossings = middle
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        assert!((crossings as i64 - 440).abs() <= 6, "{crossings}");
        let rms = (middle.iter().map(|s| s * s).sum::<f32>() / middle.len() as f32).sqrt();
        assert!((rms - 0.4 / 2f32.sqrt()).abs() < 0.05, "{rms}");
        assert!(framer.push(&[0.5; 10], true).is_empty());
        assert_eq!(framer.push(&[0.5; FRAME], true)[0], vec![0; FRAME]);
        assert!(encoder.encode(&[0; 10]).is_err());
        assert!(decoder.decode(&[0xff; 3]).is_err() || decoder.decode(&[0xff; 3]).is_ok());
    }

    #[derive(Clone)]
    struct Echo {
        gathered: mpsc::Sender<()>,
        output: Arc<TrackLocalStaticRTP>,
    }
    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for Echo {
        async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
            if state == RTCIceGatheringState::Complete {
                let _ = self.gathered.try_send(());
            }
        }
        async fn on_track(&self, track: Arc<dyn TrackRemote>) {
            let output = self.output.clone();
            tokio::spawn(async move {
                let ssrc = 4242;
                while let Some(event) = track.poll().await {
                    if let TrackRemoteEvent::OnRtpPacket(mut packet) = event {
                        packet.header.ssrc = ssrc;
                        let _ = output.write_rtp(packet).await;
                    }
                }
            });
        }
        async fn on_data_channel(&self, channel: Arc<dyn DataChannel>) {
            tokio::spawn(async move {
                let mut opened = false;
                while let Some(event) = channel.poll().await {
                    match event {
                        DataChannelEvent::OnOpen if !opened => {
                            opened = true;
                            let _ = channel.send_text(r#"{"type":"session.started"}"#).await;
                        }
                        DataChannelEvent::OnMessage(message) => {
                            let _ = channel.send(BytesMut::from(&message.data[..])).await;
                        }
                        DataChannelEvent::OnClose => break,
                        _ => {}
                    }
                }
            });
        }
    }

    /// A local peer plays the realtime service: it reflects audio and starts the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn loopback_peer_exchanges_audio_and_events() {
        let (events, mut event_receiver) = mpsc::channel(256);
        let (microphone, microphone_receiver) = mpsc::channel(64);
        let heard = Arc::new(Mutex::new(Vec::<f32>::new()));
        let sink = heard.clone();
        let muted = Arc::new(AtomicBool::new(false));
        let (peer, offer) = Peer::create(
            "127.0.0.1:0",
            events,
            microphone_receiver,
            Arc::new(move |samples: &[f32]| sink.lock().unwrap().extend_from_slice(samples)),
            muted.clone(),
        )
        .await
        .unwrap();
        assert!(
            offer.contains("opus/48000/2") && offer.contains("m=application"),
            "{offer}"
        );
        assert!(
            offer.contains("a=candidate"),
            "complete offer includes candidates"
        );

        let mut media = MediaEngine::default();
        media.register_default_codecs().unwrap();
        let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
        let (gathered, mut gathered_receiver) = mpsc::channel(1);
        let output = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
            "echo".into(),
            "echo-audio".into(),
            "Echo".into(),
            RtpCodecKind::Audio,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(4242),
                    ..Default::default()
                },
                codec: RTCRtpCodec {
                    mime_type: "audio/opus".into(),
                    clock_rate: 48_000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
                    rtcp_feedback: vec![],
                },
                ..Default::default()
            }],
        )));
        let service = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Echo {
                gathered,
                output: output.clone(),
            }))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .unwrap();
        service
            .add_track(output as Arc<dyn TrackLocal>)
            .await
            .unwrap();
        service
            .set_remote_description(RTCSessionDescription::offer(offer).unwrap())
            .await
            .unwrap();
        let answer = service.create_answer(None).await.unwrap();
        service.set_local_description(answer).await.unwrap();
        let _ = tokio::time::timeout(GATHER_TIMEOUT, gathered_receiver.recv()).await;
        peer.answer(service.local_description().await.unwrap().sdp)
            .await
            .unwrap();
        assert!(peer.answer("bad".into()).await.is_err());

        let deadline = Instant::now() + Duration::from_secs(20);
        let (mut connected, mut started) = (false, false);
        let mut position = 0;
        while Instant::now() < deadline
            && !(connected && started && heard.lock().unwrap().len() >= RATE)
        {
            let _ = microphone.try_send(tone(440.0, FRAME, position));
            position += FRAME;
            tokio::time::sleep(Duration::from_millis(20)).await;
            while let Ok(event) = event_receiver.try_recv() {
                connected |= event == json!({"type":"state","state":"connected"});
                started |=
                    event["type"] == "message" && event["data"] == r#"{"type":"session.started"}"#;
            }
        }
        assert!(connected, "peer connected");
        assert!(started, "data channel delivered the service event");
        let audio = heard.lock().unwrap().clone();
        assert!(audio.len() >= RATE, "heard {} samples", audio.len());
        let tail = &audio[audio.len() - RATE / 2..];
        let crossings = tail
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        assert!(
            (crossings as i64 - 440).abs() <= 10,
            "reflected tone crossings {crossings}"
        );

        // Muted frames keep timing and carry silence.
        muted.store(true, Ordering::Relaxed);
        heard.lock().unwrap().clear();
        for _ in 0..60 {
            let _ = microphone.try_send(tone(440.0, FRAME, position));
            position += FRAME;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let quiet = heard.lock().unwrap().clone();
        let tail = &quiet[quiet.len().saturating_sub(RATE / 4)..];
        assert!(
            !tail.is_empty() && tail.iter().all(|s| s.abs() < 0.02),
            "muted audio is silent"
        );
        drop(microphone);
        let ended = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = event_receiver.recv().await {
                if event == json!({"type":"microphone","state":"ended"}) {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(ended, "closing the microphone reports its end");
        peer.close().await;
        let _ = service.close().await;
    }
}
