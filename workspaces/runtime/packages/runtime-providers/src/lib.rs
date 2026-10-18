//! Synchronous provider wire framing and per-connection request bookkeeping.
//!
//! Provider sessions are pure state machines; a thin caller owns split byte I/O.

mod callback_queue_limit_generated;
mod codec;
mod connection;
mod initialize;
mod provider_kind;
mod session;
pub mod supervisor_v1;

pub use callback_queue_limit_generated::CALLBACK_QUEUE_LIMIT;
pub use codec::{
    CodecError, DecodedJournalEntry, DecodedJournalPayload, Frame, LineDecoder, MAX_FRAME_BYTES,
    MAX_FRAME_DELIMITER_BYTES, Method, NotificationFrame, RequestFrame, RequestId, ResponseBody,
    UnknownMethodFrame, decode_journal_entry, decode_journal_entry_borrowed, decode_journal_value,
    encode_line, encode_line_into, frame_payload_bytes, frame_prefix_payload_bytes,
    validate_initialize_params,
};
pub use connection::{
    Action, Inbound, InitializationStatus, Lifecycle, RequestOutcome, RequestTicket, SessionError,
    SessionState,
};
pub use initialize::{ClientName, initialize_params};
pub use provider_kind::{ProviderKind, UnknownProvider};
pub use session::{BookError, PendingBook, RequestState, ResponseMatch};
