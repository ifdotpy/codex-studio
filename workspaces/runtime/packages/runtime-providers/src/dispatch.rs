//! Ordered delivery of provider-initiated requests and notifications.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use crate::{Frame, RequestFrame};
use thiserror::Error;

pub use crate::callback_queue_limit_generated::CALLBACK_QUEUE_LIMIT;

/// One provider callback in wire arrival order.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    Request(RequestFrame),
    Notification(Frame),
}

/// Consumer of provider initiated work. Unknown methods are passed as frames.
pub trait Handler {
    type Error: Send + 'static;

    fn handle(&mut self, message: Inbound) -> Result<(), Self::Error>;
}

/// Bounded FIFO callback lane. Its worker is independent of the provider reader.
pub struct CallbackDispatcher<H: Handler + Send + 'static> {
    sender: SyncSender<Inbound>,
    worker: Option<JoinHandle<Result<(), DispatchError<H::Error>>>>,
}

impl<H: Handler + Send + 'static> CallbackDispatcher<H> {
    #[must_use]
    pub fn new(mut handler: H) -> Self {
        let (sender, receiver) = mpsc::sync_channel(CALLBACK_QUEUE_LIMIT);
        let worker = thread::spawn(move || {
            while let Ok(message) = receiver.recv() {
                match catch_unwind(AssertUnwindSafe(|| handler.handle(message))) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(DispatchError::Handler(error)),
                    Err(_) => return Err(DispatchError::HandlerPanicked),
                }
            }
            Ok(())
        });
        Self {
            sender,
            worker: Some(worker),
        }
    }

    pub fn send(&self, message: Inbound) -> Result<(), DispatchError<H::Error>> {
        self.sender.try_send(message).map_err(|error| match error {
            TrySendError::Full(_) => DispatchError::QueueFull,
            TrySendError::Disconnected(_) => DispatchError::HandlerPanicked,
        })
    }

    pub fn close(mut self) -> Result<(), DispatchError<H::Error>> {
        drop(self.sender);
        match self.worker.take().map(JoinHandle::join) {
            Some(Ok(result)) => result,
            Some(Err(_)) => Err(DispatchError::HandlerPanicked),
            None => Ok(()),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DispatchError<E> {
    #[error("provider callback queue is full")]
    QueueFull,
    #[error("provider callback handler failed")]
    Handler(E),
    #[error("provider callback handler panicked")]
    HandlerPanicked,
}

/// FIFO handoff lets the reader continue receiving replies while a consumer
/// drains callbacks. The owner must call `dispatch_one` on its callback lane.
#[derive(Default)]
pub struct DispatchQueue {
    messages: VecDeque<Inbound>,
}

impl DispatchQueue {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enqueue(&mut self, frame: Frame) -> Result<(), DispatchError<()>> {
        let message = match frame {
            Frame::Request(request) => Inbound::Request(request),
            Frame::Notification(notification) => {
                Inbound::Notification(Frame::Notification(notification))
            }
            Frame::UnknownMethod(unknown) => Inbound::Notification(Frame::UnknownMethod(unknown)),
            Frame::Response { .. } => return Ok(()),
        };
        if self.messages.len() >= CALLBACK_QUEUE_LIMIT {
            return Err(DispatchError::QueueFull);
        }
        self.messages.push_back(message);
        Ok(())
    }

    pub fn dispatch_one<H: Handler>(
        &mut self,
        handler: &mut H,
    ) -> Result<bool, DispatchError<H::Error>> {
        let Some(message) = self.messages.pop_front() else {
            return Ok(false);
        };
        match catch_unwind(AssertUnwindSafe(|| handler.handle(message))) {
            Ok(Ok(())) => Ok(true),
            Ok(Err(error)) => Err(DispatchError::Handler(error)),
            Err(_) => Err(DispatchError::HandlerPanicked),
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Method, NotificationFrame};
    use serde_json::Map;

    struct FailingHandler(bool);

    impl Handler for FailingHandler {
        type Error = &'static str;

        fn handle(&mut self, _: Inbound) -> Result<(), Self::Error> {
            if self.0 {
                panic!("scripted handler panic");
            }
            Err("scripted handler error")
        }
    }

    fn queued_message() -> Frame {
        Frame::Notification(NotificationFrame {
            method: Method::TurnStarted,
            params: None,
            fields: Map::new(),
        })
    }

    #[test]
    fn handler_error_is_returned_to_dispatch_caller() {
        let mut queue = DispatchQueue::new();
        queue
            .enqueue(queued_message())
            .expect("scripted test operation");
        assert_eq!(
            queue.dispatch_one(&mut FailingHandler(false)),
            Err(DispatchError::Handler("scripted handler error"))
        );
    }

    #[test]
    fn handler_panic_is_caught_at_dispatch_boundary() {
        let mut queue = DispatchQueue::new();
        queue
            .enqueue(queued_message())
            .expect("scripted test operation");
        assert_eq!(
            queue.dispatch_one(&mut FailingHandler(true)),
            Err(DispatchError::HandlerPanicked)
        );
    }
}
