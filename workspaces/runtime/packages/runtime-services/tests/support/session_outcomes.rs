use super::*;

pub(crate) fn assert_exit_outcomes_unique(exit: &SessionExit) {
    let mut terminal = BTreeMap::<(u64, RequestId), (usize, usize, usize)>::new();
    let mut writes = BTreeMap::new();
    for outcome in exit
        .outcomes
        .iter()
        .chain(exit.undelivered.iter().flat_map(|e| &e.outcomes))
    {
        match outcome {
            SessionOutcome::Resolved { ticket, outcome } => {
                let counts = terminal
                    .entry((ticket.generation, ticket.id.clone()))
                    .or_default();
                match outcome {
                    RequestOutcome::Unknown => counts.0 += 1,
                    RequestOutcome::Reply { .. } => counts.1 += 1,
                }
            }
            SessionOutcome::Write {
                operation_id,
                ticket,
                outcome,
                ..
            } => {
                assert!(
                    writes.insert(operation_id.0.clone(), ()).is_none(),
                    "duplicate write outcome"
                );
                if *outcome == WriteOutcome::NotSent
                    && let Some(ticket) = ticket
                {
                    terminal
                        .entry((ticket.generation, ticket.id.clone()))
                        .or_default()
                        .2 += 1;
                }
            }
            SessionOutcome::GenerationClosed { .. } => {}
        }
    }
    for (unknown, reply, not_sent) in terminal.values() {
        // A later positive reply may refine Unknown. Neither may be duplicated,
        // and definitely-unsent retirement excludes both request outcomes.
        assert!(
            *unknown <= 1 && *reply <= 1 && *not_sent <= 1,
            "duplicate terminal outcome"
        );
        assert!(
            *not_sent == 0 || (*unknown == 0 && *reply == 0),
            "unsent request also resolved"
        );
    }
}

#[allow(dead_code)] // The shared unit fixture only exercises completion.
pub(crate) trait CheckedExit {
    async fn checked_shutdown(&mut self) -> SessionExit;
    async fn checked_completion(&mut self) -> SessionExit;
}
impl CheckedExit for SessionHandle {
    async fn checked_shutdown(&mut self) -> SessionExit {
        let exit = self.shutdown().await;
        assert_exit_outcomes_unique(&exit);
        exit
    }
    async fn checked_completion(&mut self) -> SessionExit {
        let exit = self.completion().await;
        assert_exit_outcomes_unique(&exit);
        exit
    }
}
