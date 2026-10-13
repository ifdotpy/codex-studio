//! Required Python-owned tables for incoming pairing and durable delivery.
use crate::schema::{SchemaRequirements, TableRequirement};

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_access_discovered",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_access_invites",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_access_receipts",
        columns: &[
            "client",
            "request_id",
            "hash",
            "session",
            "status",
            "record",
        ],
    },
    TableRequirement {
        name: "runtime_access_settings",
        columns: &["id", "auto_pair"],
    },
    TableRequirement {
        name: "runtime_server_inbox",
        columns: &[
            "id",
            "signature",
            "state",
            "result",
            "created_at",
            "completed_at",
            "principal",
            "actor",
        ],
    },
    TableRequirement {
        name: "runtime_server_outbox",
        columns: &[
            "id",
            "server",
            "body",
            "state",
            "result",
            "created_at",
            "completed_at",
            "fingerprint",
            "attempts",
        ],
    },
    TableRequirement {
        name: "runtime_server_retired",
        columns: &["id", "direction", "signature", "server"],
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
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "multi_server_delivery",
        requirements: &REQUIREMENTS,
    }
}
