//! Synchronous provider wire framing and per-connection request bookkeeping.
//!
//! This crate does not spawn providers or perform I/O. Callers feed bytes into
//! [`LineDecoder`] and write the bytes returned by [`encode_line`].

mod codec;
mod session;

pub use codec::{
    CodecError, Frame, LineDecoder, MAX_FRAME_BYTES, Method, NotificationFrame, RequestFrame,
    RequestId, ResponseBody, UnknownMethodFrame, encode_line,
};
pub use session::{BookError, PendingBook, RequestState, ResponseMatch};
