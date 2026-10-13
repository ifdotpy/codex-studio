//! Transaction adapter used by scheduler and turn preparation. All decisions run on a fresh
//! checked snapshot under the writer. Provider probes and native submission occur elsewhere.
use crate::continuation::{ContinuationError, Decision, Effect, Result, Snapshot};
use studio_runtime_store::{ContinuationRepository, ContinuationTable, RuntimeStore, Transaction};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Boundary {
    BeforeSnapshot,
    AfterSnapshot,
    BeforeCapacityAdmission,
    AfterCapacityAdmission,
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
/// The turn driver checks workspace, account, budget and slot admission under this writer.
/// Implementations must not submit a turn or perform external I/O here.
pub trait CapacityAdmission {
    fn check(
        &mut self,
        tx: &Transaction<'_>,
        snapshot: &Snapshot,
        attempt: &serde_json::Value,
    ) -> Result<()>;
}
pub struct MissingCapacityAdmission;
impl CapacityAdmission for MissingCapacityAdmission {
    fn check(&mut self, _: &Transaction<'_>, _: &Snapshot, _: &serde_json::Value) -> Result<()> {
        Err(ContinuationError::Unavailable(
            "turn-driver/capacity-admission",
        ))
    }
}
pub struct DriverHooks<'a> {
    pub fault: &'a mut dyn Fault,
    pub capacity_admission: &'a mut dyn CapacityAdmission,
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
    drive_with_hooks(
        store,
        agent,
        table,
        record,
        now,
        decide,
        &mut DriverHooks {
            fault,
            capacity_admission: &mut MissingCapacityAdmission,
        },
    )
}
/// A successful capacity claim is returned only after its record and agent commit together.
/// The caller may consume its CapacityStart effect after commit; recovery uses the saved attempt.
pub fn drive_with_hooks(
    store: &RuntimeStore,
    agent: &str,
    table: ContinuationTable,
    record: Option<&str>,
    now: f64,
    decide: impl FnOnce(&Snapshot) -> Result<Decision>,
    hooks: &mut DriverHooks<'_>,
) -> Result<Decision> {
    let decision = store.transaction_with(|tx| {
        hooks.fault.at(Boundary::BeforeSnapshot)?;
        let raw = ContinuationRepository::snapshot(tx, agent, table, record)?;
        let snapshot = Snapshot::from_records(raw)?;
        hooks.fault.at(Boundary::AfterSnapshot)?;
        let d = decide(&snapshot)?;
        apply(tx, &snapshot, table, &d, now, hooks)?;
        hooks.fault.at(Boundary::BeforeCommit)?;
        Ok::<_, ContinuationError>(d)
    })?;
    hooks.fault.at(Boundary::AfterCommit)?;
    Ok(decision)
}
pub(crate) fn apply_in_transaction(
    tx: &Transaction<'_>,
    snapshot: &Snapshot,
    table: ContinuationTable,
    decision: &Decision,
    now: f64,
) -> Result<()> {
    let mut fault = NoFault;
    let mut hooks = DriverHooks {
        fault: &mut fault,
        capacity_admission: &mut MissingCapacityAdmission,
    };
    apply(tx, snapshot, table, decision, now, &mut hooks)
}

fn apply(
    tx: &Transaction<'_>,
    snapshot: &Snapshot,
    table: ContinuationTable,
    d: &Decision,
    now: f64,
    hooks: &mut DriverHooks<'_>,
) -> Result<()> {
    // Preflight effect support before any write. A rollback is still required on later failures.
    for effect in &d.effects {
        match effect {
            Effect::CapacityStart { attempt } => {
                hooks.fault.at(Boundary::BeforeCapacityAdmission)?;
                hooks.capacity_admission.check(tx, snapshot, attempt)?;
                hooks.fault.at(Boundary::AfterCapacityAdmission)?;
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
        hooks.fault.at(Boundary::BeforeRecord)?;
        ContinuationRepository::put_record(tx, table, snapshot.agent_id(), record)?;
        hooks.fault.at(Boundary::AfterRecord)?;
    }
    for effect in &d.effects {
        hooks.fault.at(Boundary::BeforeEffect)?;
        if let Effect::Followup { id, text } = effect {
            hooks.fault.at(Boundary::BeforeEffectRead)?;
            let prior = studio_runtime_store::QueueEventRepository::get(
                tx,
                &studio_runtime_store::QueueEventId::from(id.as_str()),
            )?;
            hooks.fault.at(Boundary::AfterEffectRead)?;
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
        hooks.fault.at(Boundary::AfterEffect)?;
    }
    if let Some(agent) = &d.agent {
        hooks.fault.at(Boundary::BeforeAgent)?;
        ContinuationRepository::put_agent(tx, agent)?;
        hooks.fault.at(Boundary::AfterAgent)?;
    }
    for event in &d.events {
        hooks.fault.at(Boundary::BeforeEvent)?;
        ContinuationRepository::put_event(tx, event)?;
        hooks.fault.at(Boundary::AfterEvent)?;
    }
    for (id, metadata) in &d.metadata {
        hooks.fault.at(Boundary::BeforeMetadata)?;
        ContinuationRepository::put_metadata(tx, id, metadata)?;
        hooks.fault.at(Boundary::AfterMetadata)?;
    }
    Ok(())
}
