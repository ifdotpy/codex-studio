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
