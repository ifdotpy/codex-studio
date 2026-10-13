//! The existing Python multi-server rename journal; no new storage format.

use crate::{
    StoreResult,
    schema::{SchemaRequirements, TableRequirement},
};
use rusqlite::{OptionalExtension, Transaction};

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_access_name_requests",
        columns: &["id", "label", "actor", "applied"],
    },
    TableRequirement {
        name: "runtime_access_name_targets",
        columns: &["id", "server", "label"],
    },
    TableRequirement {
        name: "runtime_access_audit",
        columns: &["seq", "client", "actor", "action", "created"],
    },
];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

pub struct FederationCredentialsRepository;

impl FederationCredentialsRepository {
    /// Return false on an ID conflict without changing either journal.
    pub fn reserve_name(
        tx: &Transaction<'_>,
        request_id: &str,
        label: &str,
        actor: &str,
    ) -> StoreResult<bool> {
        let prior = tx
            .query_row(
                "SELECT server,label FROM runtime_access_name_targets WHERE id=?1",
                [request_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()?;
        if prior.is_some_and(|(server, old)| server != "local" || old != label) {
            return Ok(false);
        }
        let prior = tx
            .query_row(
                "SELECT label FROM runtime_access_name_requests WHERE id=?1",
                [request_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        if prior.as_deref().is_some_and(|old| old != label) {
            return Ok(false);
        }
        tx.execute(
            "INSERT OR IGNORE INTO runtime_access_name_targets VALUES(?1,'local',?2)",
            rusqlite::params![request_id, label],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO runtime_access_name_requests VALUES(?1,?2,?3,0)",
            rusqlite::params![request_id, label, actor],
        )?;
        Ok(true)
    }

    pub fn pending_names(tx: &Transaction<'_>) -> StoreResult<Vec<(String, String, String)>> {
        let mut statement = tx.prepare("SELECT id,label,actor FROM runtime_access_name_requests WHERE applied=0 ORDER BY rowid")?;
        Ok(statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn complete_name(
        tx: &Transaction<'_>,
        id: &str,
        server_id: &str,
        actor: &str,
        now: f64,
    ) -> StoreResult<()> {
        tx.execute(
            "UPDATE runtime_access_name_requests SET applied=1 WHERE id=?1",
            [id],
        )?;
        tx.execute(
            "INSERT INTO runtime_access_audit(client,actor,action,created) VALUES(?1,?2,'name',?3)",
            rusqlite::params![server_id, actor, now],
        )?;
        tx.execute("DELETE FROM runtime_access_audit WHERE seq <= (SELECT max(seq)-10000 FROM runtime_access_audit)", [])?;
        Ok(())
    }
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "federation_credentials",
        requirements: &REQUIREMENTS,
    }
}
