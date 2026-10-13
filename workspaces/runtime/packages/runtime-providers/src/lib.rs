//! Synchronous provider wire framing and per-connection request bookkeeping.
//!
//! Provider sessions are pure state machines; a thin caller owns split byte I/O.

mod codec;
mod connection;
mod session;

pub use codec::{
    CodecError, DecodedJournalEntry, DecodedJournalPayload, Frame, LineDecoder, MAX_FRAME_BYTES,
    Method, NotificationFrame, RequestFrame, RequestId, ResponseBody, UnknownMethodFrame,
    decode_journal_entry, encode_line, validate_initialize_params,
};
pub use connection::{
    Action, Inbound, InitializationStatus, Lifecycle, RequestOutcome, RequestTicket, SessionError,
    SessionState,
};
pub use session::{BookError, PendingBook, RequestState, ResponseMatch};
