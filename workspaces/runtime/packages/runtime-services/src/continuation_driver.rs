//! Transaction adapter used by scheduler and turn preparation. All decisions run on a fresh
//! checked snapshot under the writer. Provider probes and native submission occur elsewhere.
use crate::continuation::{ContinuationError, Decision, Effect, Result, Snapshot};
use studio_runtime_store::{ContinuationRepository, ContinuationTable, RuntimeStore, Transaction};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    BeforeSnapshot,
    AfterSnapshot,
    BeforeRecord,
    AfterRecord,
    BeforeAgent,
    AfterAgent,
    BeforeEvent,
    AfterEvent,
    BeforeMetadata,
    AfterMetadata,
    BeforeEffectRead,
    AfterEffectRead,
    BeforeEffect,
    AfterEffect,
    BeforeCommit,
    AfterCommit,
}
/// A deterministic fault hook; production uses `NoFault`.
pub trait Fault {
    fn at(&mut self, boundary: Boundary) -> Result<()>;
}
pub struct NoFault;
impl Fault for NoFault {
    fn at(&mut self, _: Boundary) -> Result<()> {
        Ok(())
    }
}
/// This adapter has no turn driver. Claim intents and worker holds fail atomically with a
/// named dependency until the integrator installs those consumers. No partial writes escape.
pub fn drive(
    store: &RuntimeStore,
    agent: &str,
    table: ContinuationTable,
    record: Option<&str>,
    now: f64,
    decide: impl FnOnce(&Snapshot) -> Result<Decision>,
) -> Result<Decision> {
    drive_with_fault(store, agent, table, record, now, decide, &mut NoFault)
}
pub fn drive_with_fault(
    store: &RuntimeStore,
    agent: &str,
    table: ContinuationTable,
    record: Option<&str>,
    now: f64,
    decide: impl FnOnce(&Snapshot) -> Result<Decision>,
    fault: &mut impl Fault,
) -> Result<Decision> {
    let decision = store.transaction_with(|tx| {
        fault.at(Boundary::BeforeSnapshot)?;
        let raw = ContinuationRepository::snapshot(tx, agent, table, record)?;
        let snapshot = Snapshot::from_records(raw)?;
        fault.at(Boundary::AfterSnapshot)?;
        let d = decide(&snapshot)?;
        apply(tx, &snapshot, table, &d, now, fault)?;
        fault.at(Boundary::BeforeCommit)?;
        Ok::<_, ContinuationError>(d)
    })?;
    fault.at(Boundary::AfterCommit)?;
    Ok(decision)
}
fn apply(
    tx: &Transaction<'_>,
    snapshot: &Snapshot,
    table: ContinuationTable,
    d: &Decision,
    now: f64,
    fault: &mut impl Fault,
) -> Result<()> {
    // Preflight effect support before any write. A rollback is still required on later failures.
    for effect in &d.effects {
        match effect {
            Effect::CapacityStart { .. } => {
                return Err(ContinuationError::Unavailable(
                    "turn-driver/capacity-admission",
                ));
            }
            Effect::WorkerHold { .. }
                if !crate::continuation::flag(&snapshot.agent, "isLead")
                    && crate::continuation::present(&snapshot.agent, "parentId") =>
            {
                return Err(ContinuationError::Unavailable(
                    "event-driver/permanent-worker-hold",
                ));
            }
            _ => {}
        }
    }
    // Validate all resulting stored records with the same constructor used for reads.
    let mut validation = serde_json::json!({"agent":d.agent.as_ref().unwrap_or(&snapshot.agent),"record":d.record.as_ref().or(snapshot.record.as_ref()),"events":d.events,"ancestors":snapshot.ancestors,"claims":snapshot.claims});
    validation["metadata"] = serde_json::json!(d.metadata);
    Snapshot::from_records(validation)?;
    if let Some(record) = &d.record {
        fault.at(Boundary::BeforeRecord)?;
        ContinuationRepository::put_record(tx, table, snapshot.agent_id(), record)?;
        fault.at(Boundary::AfterRecord)?;
    }
    for effect in &d.effects {
        fault.at(Boundary::BeforeEffect)?;
        if let Effect::Followup { id, text } = effect {
            fault.at(Boundary::BeforeEffectRead)?;
            let prior = studio_runtime_store::QueueEventRepository::get(
                tx,
                &studio_runtime_store::QueueEventId::from(id.as_str()),
            )?;
            fault.at(Boundary::AfterEffectRead)?;
            if prior.is_some_and(|e| {
                e.agent.0 != snapshot.agent_id()
                    || e.epoch != snapshot.agent["epoch"].as_i64().unwrap_or(-1)
                    || e.kind != "followup"
                    || e.text != *text
            }) {
                return Err(ContinuationError::Invalid("/events/identity".into()));
            }

            let agent = serde_json::from_value(snapshot.agent.clone())
                .map_err(|_| ContinuationError::Invalid("/agent".into()))?;
            crate::event_write::enqueue_event(
                tx,
                &agent,
                id.clone(),
                "followup",
                text.clone(),
                now,
            )
            .map_err(|e| {
                ContinuationError::Store(studio_runtime_store::StoreError::SchemaMismatch(
                    e.to_string(),
                ))
            })?;
        }
        fault.at(Boundary::AfterEffect)?;
    }
    if let Some(agent) = &d.agent {
        fault.at(Boundary::BeforeAgent)?;
        ContinuationRepository::put_agent(tx, agent)?;
        fault.at(Boundary::AfterAgent)?;
    }
    for event in &d.events {
        fault.at(Boundary::BeforeEvent)?;
        ContinuationRepository::put_event(tx, event)?;
        fault.at(Boundary::AfterEvent)?;
    }
    for (id, metadata) in &d.metadata {
        fault.at(Boundary::BeforeMetadata)?;
        ContinuationRepository::put_metadata(tx, id, metadata)?;
        fault.at(Boundary::AfterMetadata)?;
    }
    Ok(())
}
