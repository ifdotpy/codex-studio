//! Synchronous provider wire framing and per-connection request bookkeeping.
//!
//! Provider sessions are pure state machines; a thin caller owns split byte I/O.

mod codec;
mod connection;
mod session;

pub use codec::{
    CodecError, Frame, LineDecoder, MAX_FRAME_BYTES, Method, NotificationFrame, RequestFrame,
    RequestId, ResponseBody, UnknownMethodFrame, encode_line,
};
pub use connection::{
    Action, Inbound, Lifecycle, RequestOutcome, RequestTicket, SessionError, SessionState,
};
pub use session::{BookError, PendingBook, RequestState, ResponseMatch};
