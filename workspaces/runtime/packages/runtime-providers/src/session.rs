use std::collections::BTreeMap;

use crate::{Method, RequestId};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestState {
    Pending,
    Unknown,
    Replied,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseMatch {
    /// The response settled a request that was still pending.
    Pending { method: Method },
    /// A late response supplies positive evidence for a previously unknown request.
    Late { method: Method },
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum BookError {
    #[error("numeric request ID space exhausted")]
    IdExhausted,
    #[error("request ID was already used on this connection")]
    DuplicateId,
    #[error("response has an unknown request ID")]
    UnknownId,
    #[error("request already received a response")]
    DuplicateResponse,
    #[error("cannot submit on a connection after transport loss")]
    ConnectionClosed,
}

#[derive(Clone, Copy)]
struct Entry {
    method: Method,
    state: RequestState,
}

/// Pure per-connection ID allocation and response tracking.
///
/// A book is scoped to one provider connection. On disconnect, pending work is
/// marked unknown and this book becomes closed; reconnect with a new book.
#[derive(Default)]
pub struct PendingBook {
    next_numeric_id: u64,
    entries: BTreeMap<RequestId, Entry>,
    closed: bool,
}

impl PendingBook {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate the next positive integer ID and register its request method.
    pub fn allocate_numeric(&mut self, method: Method) -> Result<RequestId, BookError> {
        if self.closed {
            return Err(BookError::ConnectionClosed);
        }
        let next = self
            .next_numeric_id
            .checked_add(1)
            .ok_or(BookError::IdExhausted)?;
        let id = RequestId::Number(next);
        self.register(id.clone(), method)?;
        self.next_numeric_id = next;
        Ok(id)
    }

    /// Register an externally allocated ID, used for the bridge's string UUIDs.
    pub fn register(&mut self, id: RequestId, method: Method) -> Result<(), BookError> {
        if self.closed {
            return Err(BookError::ConnectionClosed);
        }
        if self.entries.contains_key(&id) {
            return Err(BookError::DuplicateId);
        }
        self.entries.insert(
            id,
            Entry {
                method,
                state: RequestState::Pending,
            },
        );
        Ok(())
    }

    /// Observe a provider response. A response to an unknown request is late
    /// evidence; duplicate and never-requested IDs remain explicit errors.
    pub fn observe_response(&mut self, id: &RequestId) -> Result<ResponseMatch, BookError> {
        let entry = self.entries.get_mut(id).ok_or(BookError::UnknownId)?;
        let matched = match entry.state {
            RequestState::Pending => ResponseMatch::Pending {
                method: entry.method,
            },
            RequestState::Unknown => ResponseMatch::Late {
                method: entry.method,
            },
            RequestState::Replied => return Err(BookError::DuplicateResponse),
        };
        entry.state = RequestState::Replied;
        Ok(matched)
    }

    /// Mark every request without an observed response as unknown after EOF,
    /// transport failure, or a caller deadline. No retry is scheduled.
    pub fn mark_connection_lost(&mut self) {
        self.closed = true;
        for entry in self.entries.values_mut() {
            if entry.state == RequestState::Pending {
                entry.state = RequestState::Unknown;
            }
        }
    }

    #[must_use]
    pub fn state(&self, id: &RequestId) -> Option<RequestState> {
        self.entries.get(id).map(|entry| entry.state)
    }

    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.state == RequestState::Pending)
            .count()
    }

    #[must_use]
    pub fn unknown_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.state == RequestState::Unknown)
            .count()
    }
}
