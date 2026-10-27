//! Diagnostic history and dismissals. These rows never grant retry authority.
use rusqlite::{Transaction, params};
use studio_runtime_schema::persistence::{IncidentKind, OwnerKind};
use studio_runtime_schema::studio_errors::{Context, StudioError, Target};

use crate::{StoreError, StoreResult};

/// Incident API times use seconds; SQL history timestamps use milliseconds.
pub fn timestamp_ms(seconds: f64) -> StoreResult<i64> {
    if !seconds.is_finite() || seconds < 0.0 || seconds * 1000.0 > i64::MAX as f64 {
        return Err(StoreError::RecordContract("invalid incident timestamp"));
    }
    Ok((seconds * 1000.0) as i64)
}

fn owner(incident: &StudioError) -> (OwnerKind, &str) {
    match &incident.target {
        Target::Check { agent_id, .. } | Target::Resume { agent_id, .. } => {
            (OwnerKind::Agent, agent_id)
        }
        Target::Details => match &incident.diagnostics.context {
            Context::Recovery { account, .. } => (OwnerKind::Account, account),
            Context::Http {
                request_id: Some(id),
                ..
            } => (OwnerKind::Request, id),
            Context::Http {
                request_id: None, ..
            }
            | Context::Stored { .. }
            | Context::Monitor { .. }
            | Context::Input { .. }
            | Context::Agent { .. } => (OwnerKind::Request, &incident.id),
        },
    }
}

/// First diagnostic body wins. A dismissal may predate the diagnostic import.
pub fn put(tx: &Transaction<'_>, incident: &StudioError) -> StoreResult<()> {
    let (owner_kind, owner_id) = owner(incident);
    let at = timestamp_ms(incident.at)?;
    let payload = serde_json::to_string(incident)?;
    tx.execute(
        "INSERT INTO runtime_error_incidents(id,kind,owner_kind,owner_id,created,last_seen,payload) VALUES(?1,?2,?3,?4,?5,?5,?6)
         ON CONFLICT(id) DO UPDATE SET
           kind=CASE WHEN payload='null' THEN excluded.kind ELSE kind END,
           owner_kind=CASE WHEN payload='null' THEN excluded.owner_kind ELSE owner_kind END,
           owner_id=CASE WHEN payload='null' THEN excluded.owner_id ELSE owner_id END,
           created=CASE WHEN payload='null' THEN excluded.created ELSE created END,
           last_seen=MAX(last_seen,excluded.last_seen),
           payload=CASE WHEN payload='null' THEN excluded.payload ELSE payload END",
        params![incident.id, incident.kind, owner_kind, owner_id, at, payload],
    )?;
    Ok(())
}

/// A dismissal-only row has a typed absent incident (`null`), never a fabricated
/// error body. Its identifier suppresses the existing agent/event/group source.
pub fn dismiss(tx: &Transaction<'_>, id: &str, now: f64, advance: bool) -> StoreResult<()> {
    let at = timestamp_ms(now)?;
    tx.execute(
        "INSERT INTO runtime_error_incidents(id,kind,owner_kind,owner_id,created,last_seen,dismissed_at,payload)
         VALUES(?1,?2,?3,?1,?4,?4,?4,'null') ON CONFLICT(id) DO UPDATE SET
         dismissed_at=CASE WHEN ?5 THEN MAX(COALESCE(dismissed_at,0),excluded.dismissed_at) ELSE COALESCE(dismissed_at,excluded.dismissed_at) END",
        params![id, IncidentKind::UnclassifiedFailure, OwnerKind::Request, at, advance],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_incident_dismissal_is_independent_of_request_ttl_and_never_slides() {
        let mut db = rusqlite::Connection::open_in_memory().expect("database");
        crate::receipt_schema::create(&db).expect("schema");
        let tx = db.transaction().expect("transaction");
        dismiss(&tx, "event:one", 1.0, false).expect("dismiss");
        dismiss(&tx, "event:one", 2.0, false).expect("repeat");
        let (at, payload): (i64, String) = tx
            .query_row(
                "SELECT dismissed_at,payload FROM runtime_error_incidents WHERE id='event:one'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("dismissal");
        assert_eq!((at, payload.as_str()), (1000, "null"));
        dismiss(&tx, "monitors:one", 2.0, true).expect("group");
        dismiss(&tx, "monitors:one", 3.0, true).expect("group later");
        let at: i64 = tx
            .query_row(
                "SELECT dismissed_at FROM runtime_error_incidents WHERE id='monitors:one'",
                [],
                |row| row.get(0),
            )
            .expect("group time");
        assert_eq!(at, 3000);
        assert!(timestamp_ms(f64::NAN).is_err());
        assert!(timestamp_ms(-1.0).is_err());
    }
}
