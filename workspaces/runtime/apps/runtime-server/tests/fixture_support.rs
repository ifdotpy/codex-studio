use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use studio_runtime_store::RuntimeStore;

pub(crate) const STATE_PLACEHOLDER: &str = "/fixture/state";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRow {
    id: String,
    record: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tables {
    runtime_agents: Vec<StoredRow>,
    runtime_projects: Vec<StoredRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFixture {
    agents: Value,
    tables: Tables,
}

#[derive(Clone, Copy)]
pub(crate) enum FixtureKind {
    Dispatch,
    Recovery,
}

pub(crate) async fn seed(store: &RuntimeStore, root: &Path, kind: FixtureKind) -> Value {
    let data = match kind {
        FixtureKind::Dispatch => include_str!("fixtures/dispatch.json"),
        FixtureKind::Recovery => include_str!("fixtures/recovery.json"),
    };
    let fixture: StateFixture =
        serde_json::from_str(&data.replace(STATE_PLACEHOLDER, &root.to_string_lossy()))
            .expect("frozen state fixture");
    store
        .transaction(move |tx| {
            for row in fixture.tables.runtime_projects {
                tx.execute(
                    "INSERT INTO runtime_projects(id,record) VALUES (?1,?2)",
                    [&row.id, &row.record],
                )?;
            }
            for row in fixture.tables.runtime_agents {
                let record: Value = serde_json::from_str(&row.record)?;
                studio_runtime_store::agent_volatile::put(tx, &row.id, &record, true)?;
            }
            Ok(())
        })
        .await
        .expect("seed frozen runtime records");
    fixture.agents
}
