//! Pure validation and state transitions for native voice sessions and audio.
//!
//! Provider requests, SQLite writes and private-file operations are performed
//! by the voice service adapter. This module accepts checked snapshots and
//! returns decisions without holding any runtime resources.

use base64::{Engine, engine::general_purpose::STANDARD};
use studio_runtime_store::{AgentId, sha256_hex};
use thiserror::Error;

pub const MAX_SESSION_ID_CHARS: usize = 120;
pub const MAX_OFFER_CHARS: usize = 100_000;
pub const MAX_AUDIO_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SessionId(String);

impl SessionId {
    pub fn parse(value: String) -> Result<Self, VoiceError> {
        if value.is_empty() || value.chars().count() > MAX_SESSION_ID_CHARS {
            return Err(VoiceError::InvalidSessionIdentity);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VoiceSessionState {
    Connecting,
    Ready,
    Stopping,
    Ended,
    Failed,
    Lost,
    Unknown,
}

impl VoiceSessionState {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "connecting" => Some(Self::Connecting),
            "ready" => Some(Self::Ready),
            "stopping" => Some(Self::Stopping),
            "ended" => Some(Self::Ended),
            "failed" => Some(Self::Failed),
            "lost" => Some(Self::Lost),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Ended => "ended",
            Self::Failed => "failed",
            Self::Lost => "lost",
            Self::Unknown => "unknown",
        }
    }
}

/// A validated view of one stored voice session row.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceSession {
    id: SessionId,
    agent: AgentId,
    created: f64,
    ended: Option<f64>,
    offer: Option<String>,
    answer: Option<String>,
    native_thread: Option<String>,
    account_key: Option<String>,
    connection_id: Option<String>,
    state: Option<VoiceSessionState>,
    error: Option<String>,
}

impl VoiceSession {
    pub fn from_row(row: VoiceSessionRow) -> Result<Self, VoiceError> {
        let state = match row.state.as_deref() {
            Some(value) => {
                Some(VoiceSessionState::parse(value).ok_or(VoiceError::InvalidStoredSession)?)
            }
            None => None,
        };
        if !row.created.is_finite()
            || row.created < 0.0
            || row
                .ended
                .is_some_and(|value| !value.is_finite() || value < 0.0)
            || row
                .offer
                .as_ref()
                .is_some_and(|value| value.chars().count() > MAX_OFFER_CHARS)
        {
            return Err(VoiceError::InvalidStoredSession);
        }
        Ok(Self {
            id: SessionId::parse(row.id).map_err(|_| VoiceError::InvalidStoredSession)?,
            agent: AgentId::from(row.agent),
            created: row.created,
            ended: row.ended,
            offer: row.offer,
            answer: row.answer,
            native_thread: row.native_thread,
            account_key: row.account_key,
            connection_id: row.connection_id,
            state,
            error: row.error,
        })
    }

    pub fn id(&self) -> &SessionId {
        &self.id
    }

    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    pub fn created(&self) -> f64 {
        self.created
    }

    pub fn ended(&self) -> Option<f64> {
        self.ended
    }

    pub fn answer(&self) -> Option<&str> {
        self.answer.as_deref()
    }

    pub fn native_thread(&self) -> Option<&str> {
        self.native_thread.as_deref()
    }

    pub fn account_key(&self) -> Option<&str> {
        self.account_key.as_deref()
    }

    pub fn connection_id(&self) -> Option<&str> {
        self.connection_id.as_deref()
    }

    pub fn state(&self) -> Option<VoiceSessionState> {
        self.state
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn is_active(&self) -> bool {
        self.state.is_some() && self.ended.is_none()
    }
}

/// Untrusted SQL columns. Convert these through [`VoiceSession::from_row`]
/// before they enter a decision snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct VoiceSessionRow {
    pub id: String,
    pub agent: String,
    pub created: f64,
    pub ended: Option<f64>,
    pub offer: Option<String>,
    pub answer: Option<String>,
    pub native_thread: Option<String>,
    pub account_key: Option<String>,
    pub connection_id: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StartRequest {
    agent: AgentId,
    session_id: SessionId,
    offer: String,
}

impl StartRequest {
    pub fn new(agent: AgentId, session_id: String, offer: String) -> Result<Self, VoiceError> {
        let session_id = SessionId::parse(session_id)?;
        if !offer.starts_with("v=0") || offer.chars().count() > MAX_OFFER_CHARS {
            return Err(VoiceError::InvalidOffer);
        }
        Ok(Self {
            agent,
            session_id,
            offer,
        })
    }

    pub fn offer(&self) -> &str {
        &self.offer
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum StartDecision {
    Replay(VoiceSession),
    Create {
        agent: AgentId,
        session_id: SessionId,
        offer: String,
        created: f64,
    },
}

/// Classify an idempotent start request against rows read in its write
/// transaction. Existing identity is resolved before the active-session gate.
pub fn decide_start(
    now: Option<f64>,
    request: StartRequest,
    existing: Option<&VoiceSession>,
    active: Option<&VoiceSession>,
) -> Result<StartDecision, VoiceError> {
    if let Some(old) = existing {
        if old.id != request.session_id
            || old.agent != request.agent
            || old.offer.as_ref().is_some_and(|sdp| sdp != &request.offer)
        {
            return Err(VoiceError::SessionIdentityConflict);
        }
        return Ok(StartDecision::Replay(old.clone()));
    }
    if active.is_some() {
        if active.is_some_and(|session| session.agent != request.agent) {
            return Err(VoiceError::InvalidStoredSession);
        }
        return Err(VoiceError::ActiveSessionExists);
    }
    let now = now.ok_or(VoiceError::InvalidClock)?;
    if !now.is_finite() || now < 0.0 {
        return Err(VoiceError::InvalidClock);
    }
    Ok(StartDecision::Create {
        agent: request.agent,
        session_id: request.session_id,
        offer: request.offer,
        created: now,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionSignal {
    AnswerReceived(String),
    ProviderFailed(Option<String>),
    PeerClosed(Option<String>),
    ProviderDisconnected,
    StopRequested { start_dispatched: bool },
    StopRejected(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionTransition {
    pub session: VoiceSession,
    pub stop_provider: bool,
}

/// Apply one provider or lifecycle event. `now` is supplied by the adapter so
/// decisions can be tested without reading a clock.
pub fn transition_session(
    current: &VoiceSession,
    signal: SessionSignal,
    now: f64,
) -> Result<SessionTransition, VoiceError> {
    if !now.is_finite() || now < 0.0 {
        return Err(VoiceError::InvalidClock);
    }
    let mut session = current.clone();
    let stop_provider = match signal {
        SessionSignal::AnswerReceived(answer) => {
            if answer.chars().count() > MAX_OFFER_CHARS {
                return Err(VoiceError::InvalidOffer);
            }
            session.answer = Some(answer);
            if session.ended.is_none() {
                session.state = Some(VoiceSessionState::Ready);
            }
            false
        }
        SessionSignal::ProviderFailed(error) => {
            session.state = Some(VoiceSessionState::Failed);
            session.error = Some(
                error
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "Voice failed".to_owned()),
            );
            true
        }
        SessionSignal::PeerClosed(reason) => {
            if session.state != Some(VoiceSessionState::Failed) {
                session.state = Some(VoiceSessionState::Ended);
                session.error = reason
                    .filter(|value| !value.is_empty() && value != "requested")
                    .map(|_| "Voice disconnected. The saved transcript remains.".to_owned());
            }
            session.ended.get_or_insert(now);
            false
        }
        SessionSignal::ProviderDisconnected => {
            session.state = Some(VoiceSessionState::Lost);
            session.error = Some("Codex disconnected. The saved transcript remains.".to_owned());
            session.ended.get_or_insert(now);
            false
        }
        SessionSignal::StopRequested { start_dispatched } => {
            if start_dispatched && current.ended.is_none() {
                if current.state != Some(VoiceSessionState::Failed) {
                    session.state = Some(VoiceSessionState::Stopping);
                }
                true
            } else {
                session.state = Some(VoiceSessionState::Ended);
                session.ended.get_or_insert(now);
                false
            }
        }
        SessionSignal::StopRejected(error) => {
            session.state = Some(VoiceSessionState::Unknown);
            session.error = Some(error);
            false
        }
    };
    Ok(SessionTransition {
        session,
        stop_provider,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioChunkRequest {
    pub agent: AgentId,
    pub session_id: SessionId,
    pub chunk_id: String,
    pub base64: String,
    pub mime: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioChunk {
    file_name: String,
    bytes: Vec<u8>,
    mime: String,
}

impl AudioChunk {
    pub fn file_name(&self) -> &str {
        &self.file_name
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn mime(&self) -> &str {
        &self.mime
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AudioDecision {
    Store(AudioChunk),
    Reuse(AudioChunk),
}

/// Validate and decode an upload and decide whether the deterministic file
/// identity is new or an exact retry. The caller performs the exclusive file
/// create and handles a create race with the same content comparison.
pub fn decide_audio(
    request: AudioChunkRequest,
    session_exists: bool,
    existing_bytes: Option<&[u8]>,
) -> Result<AudioDecision, VoiceError> {
    if !session_exists {
        return Err(VoiceError::UnknownSession);
    }
    let media_type = request.mime.split(';').next().unwrap_or_default();
    if !matches!(media_type, "audio/mp4" | "audio/webm" | "audio/ogg") {
        return Err(VoiceError::UnsupportedAudioFormat);
    }
    if request.base64.len() > MAX_AUDIO_BYTES.div_ceil(3) * 4 {
        return Err(VoiceError::AudioTooLarge);
    }
    let bytes = STANDARD
        .decode(request.base64.as_bytes())
        .map_err(|_| VoiceError::InvalidAudioEncoding)?;
    if bytes.len() > MAX_AUDIO_BYTES {
        return Err(VoiceError::AudioTooLarge);
    }
    let identity = format!(
        "{}:{}:{}",
        request.agent.as_ref(),
        request.session_id.as_str(),
        request.chunk_id
    );
    let file_name = sha256_hex(identity.as_bytes());
    let chunk = AudioChunk {
        file_name,
        bytes,
        mime: request.mime,
    };
    match existing_bytes {
        Some(existing) if existing == chunk.bytes => Ok(AudioDecision::Reuse(chunk)),
        Some(_) => Err(VoiceError::AudioIdentityConflict),
        None => Ok(AudioDecision::Store(chunk)),
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum VoiceError {
    #[error("invalid voice session identity")]
    InvalidSessionIdentity,
    #[error("invalid WebRTC offer")]
    InvalidOffer,
    #[error("invalid stored voice session")]
    InvalidStoredSession,
    #[error("voice session identity conflict")]
    SessionIdentityConflict,
    #[error("active voice session exists")]
    ActiveSessionExists,
    #[error("unknown voice session")]
    UnknownSession,
    #[error("unsupported audio format")]
    UnsupportedAudioFormat,
    #[error("invalid audio encoding")]
    InvalidAudioEncoding,
    #[error("audio chunk exceeds 4 MiB")]
    AudioTooLarge,
    #[error("audio chunk identity conflict")]
    AudioIdentityConflict,
    #[error("invalid clock")]
    InvalidClock,
}

#[cfg(test)]
mod tests {
    use super::{
        AgentId, AudioChunkRequest, AudioDecision, SessionId, SessionSignal, StartDecision,
        StartRequest, VoiceError, VoiceSession, VoiceSessionRow, VoiceSessionState, decide_audio,
        decide_start, transition_session,
    };

    fn session(
        id: &str,
        agent: &str,
        state: Option<&str>,
        ended: Option<f64>,
        offer: Option<&str>,
    ) -> VoiceSession {
        VoiceSession::from_row(VoiceSessionRow {
            id: id.to_owned(),
            agent: agent.to_owned(),
            created: 1.0,
            ended,
            offer: offer.map(str::to_owned),
            answer: None,
            native_thread: None,
            account_key: None,
            connection_id: None,
            state: state.map(str::to_owned),
            error: None,
        })
        .expect("test voice row is valid")
    }

    #[test]
    fn session_snapshot_rejects_unknown_states_and_non_finite_timestamps() {
        assert_eq!(
            VoiceSession::from_row(VoiceSessionRow {
                id: "s".to_owned(),
                agent: "a".to_owned(),
                created: 1.0,
                ended: None,
                offer: None,
                answer: None,
                native_thread: None,
                account_key: None,
                connection_id: None,
                state: Some("future-state".to_owned()),
                error: None,
            }),
            Err(VoiceError::InvalidStoredSession)
        );
        assert_eq!(
            VoiceSession::from_row(VoiceSessionRow {
                id: "s".to_owned(),
                agent: "a".to_owned(),
                created: f64::NAN,
                ended: None,
                offer: None,
                answer: None,
                native_thread: None,
                account_key: None,
                connection_id: None,
                state: None,
                error: None,
            }),
            Err(VoiceError::InvalidStoredSession)
        );
    }

    #[test]
    fn start_replay_precedes_active_session_refusal_and_rejects_changed_identity() {
        let prior = session("s", "a", Some("connecting"), None, Some("v=0 offer"));
        let request = StartRequest::new(AgentId::from("a"), "s".to_owned(), "v=0 offer".to_owned())
            .expect("offer is valid");
        assert_eq!(
            decide_start(Some(2.0), request.clone(), Some(&prior), Some(&prior)),
            Ok(StartDecision::Replay(prior.clone()))
        );
        let changed = StartRequest::new(AgentId::from("a"), "s".to_owned(), "v=0 other".to_owned())
            .expect("offer is valid");
        assert_eq!(
            decide_start(Some(2.0), changed, Some(&prior), None),
            Err(VoiceError::SessionIdentityConflict)
        );
        assert_eq!(
            decide_start(Some(2.0), request, None, Some(&prior)),
            Err(VoiceError::ActiveSessionExists)
        );
    }

    #[test]
    fn session_transition_preserves_provider_uncertainty_and_error_on_close() {
        let initial = session("s", "a", Some("connecting"), None, Some("v=0 offer"));
        let failed = transition_session(
            &initial,
            SessionSignal::ProviderFailed(Some("specific refusal".to_owned())),
            2.0,
        )
        .expect("clock is valid");
        assert_eq!(failed.session.state(), Some(VoiceSessionState::Failed));
        assert!(failed.session.ended().is_none());
        assert!(failed.stop_provider);
        let closed = transition_session(
            &failed.session,
            SessionSignal::PeerClosed(Some("requested".to_owned())),
            3.0,
        )
        .expect("clock is valid");
        assert_eq!(closed.session.state(), Some(VoiceSessionState::Failed));
        assert_eq!(closed.session.error(), Some("specific refusal"));
        assert_eq!(closed.session.ended(), Some(3.0));
    }

    #[test]
    fn stop_before_dispatch_tombstones_and_does_not_send_provider_stop() {
        let initial = session("s", "a", Some("connecting"), None, Some("v=0 offer"));
        let ended = transition_session(
            &initial,
            SessionSignal::StopRequested {
                start_dispatched: false,
            },
            2.0,
        )
        .expect("clock is valid");
        assert_eq!(ended.session.state(), Some(VoiceSessionState::Ended));
        assert_eq!(ended.session.ended(), Some(2.0));
        assert!(!ended.stop_provider);
    }

    #[test]
    fn audio_decision_limits_and_deduplicates_by_session_chunk_identity() {
        let request = AudioChunkRequest {
            agent: AgentId::from("a"),
            session_id: SessionId::parse("s".to_owned()).expect("session ID is valid"),
            chunk_id: "part-1".to_owned(),
            base64: "AQID".to_owned(),
            mime: "audio/webm; codecs=opus".to_owned(),
        };
        let first = decide_audio(request.clone(), true, None).expect("audio is valid");
        let AudioDecision::Store(chunk) = first else {
            panic!("new chunks must produce a store intent")
        };
        assert_eq!(chunk.bytes(), &[1, 2, 3]);
        assert_eq!(chunk.file_name().len(), 64);
        assert_eq!(
            decide_audio(request.clone(), true, Some(chunk.bytes())),
            Ok(AudioDecision::Reuse(chunk.clone()))
        );
        assert_eq!(
            decide_audio(request, true, Some(&[4, 5, 6])),
            Err(VoiceError::AudioIdentityConflict)
        );
    }

    #[test]
    fn audio_refuses_unknown_sessions_bad_encoding_and_unsupported_media() {
        let request = |base64: &str, mime: &str| AudioChunkRequest {
            agent: AgentId::from("a"),
            session_id: SessionId::parse("s".to_owned()).expect("session ID is valid"),
            chunk_id: "part".to_owned(),
            base64: base64.to_owned(),
            mime: mime.to_owned(),
        };
        assert_eq!(
            decide_audio(request("", "audio/ogg"), false, None),
            Err(VoiceError::UnknownSession)
        );
        assert_eq!(
            decide_audio(request("!!", "audio/ogg"), true, None),
            Err(VoiceError::InvalidAudioEncoding)
        );
        assert_eq!(
            decide_audio(request("AQID", "audio/wav"), true, None),
            Err(VoiceError::UnsupportedAudioFormat)
        );
    }
}
