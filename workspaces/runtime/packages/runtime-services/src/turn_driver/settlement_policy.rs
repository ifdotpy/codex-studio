//! Pure admission policy, independent of the synchronous or asynchronous mailbox.
#[derive(Default)]
pub(super) struct EvidenceWindow {
    highest: u64,
    resolving: bool,
}
impl EvidenceWindow {
    pub(super) fn new(handled: u64) -> Self {
        Self {
            highest: handled,
            resolving: false,
        }
    }
    pub(super) fn observe(&mut self, cursor: u64) {
        self.highest = self.highest.max(cursor);
    }
    pub(super) fn restart(&mut self) {
        self.resolving = true;
    }
    pub(super) fn should_hold(
        &mut self,
        handled: u64,
        settlements: usize,
        limit: usize,
        requests: usize,
    ) -> bool {
        let outstanding = self.highest.saturating_sub(handled);
        if outstanding == 0 {
            self.resolving = false;
        }
        outstanding >= limit as u64
            || settlements >= limit
            || (!self.resolving && settlements >= limit - requests * 2)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EventDisposition {
    Ignore,
    Observe,
    Defer,
    Settle,
}
pub(super) fn event_disposition(
    method: studio_runtime_providers::Method,
    has_attempt: bool,
    bound: bool,
) -> EventDisposition {
    use studio_runtime_providers::Method;
    if !has_attempt {
        return EventDisposition::Ignore;
    }
    match method {
        Method::TurnStarted => EventDisposition::Observe,
        Method::TurnCompleted if bound => EventDisposition::Settle,
        Method::TurnCompleted => EventDisposition::Defer,
        _ => EventDisposition::Ignore,
    }
}
pub(super) fn start_delivery(
    method: studio_runtime_providers::Method,
    not_sent: bool,
) -> (crate::scheduler::TurnStart, bool) {
    use crate::scheduler::TurnStart;
    use studio_runtime_providers::Method;
    let decision = if not_sent {
        TurnStart::NotSubmitted
    } else if method == Method::TurnStart {
        TurnStart::Unknown
    } else {
        TurnStart::Blocked {
            reason: "Provider thread resume outcome is unknown; input remains pending.".into(),
        }
    };
    (decision, not_sent || method == Method::ThreadResume)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolving_reply_has_room_but_cursor_evidence_stays_bounded() {
        let mut policy = EvidenceWindow::new(10);
        policy.observe(74);
        assert!(policy.should_hold(10, 64, 128, 32));
        policy.restart();
        assert!(!policy.should_hold(10, 64, 128, 32));
        policy.observe(138);
        assert!(policy.should_hold(10, 1, 128, 32));
        assert!(!policy.should_hold(138, 0, 128, 32));
    }
}
