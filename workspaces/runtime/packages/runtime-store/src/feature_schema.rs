use rusqlite::Transaction;

use crate::StoreResult;

const VOICE_SCHEMA: &str = r#"
    CREATE TABLE IF NOT EXISTS voice_sessions (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, created REAL NOT NULL,
                  ended REAL, sdp TEXT, answer TEXT, native_thread TEXT,
                  account_key TEXT, connection_id TEXT, state TEXT, error TEXT);
    CREATE TABLE IF NOT EXISTS voice_records (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  agent TEXT NOT NULL, session TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, item_id TEXT NOT NULL, previous_item_id TEXT NOT NULL,
                  payload TEXT NOT NULL, created REAL NOT NULL);
    CREATE INDEX IF NOT EXISTS voice_records_agent ON voice_records(agent,seq);
    CREATE TABLE IF NOT EXISTS voice_approvals (id TEXT PRIMARY KEY, agent TEXT NOT NULL, speech TEXT NOT NULL, transcript TEXT NOT NULL, status TEXT NOT NULL, result TEXT);
    CREATE TABLE IF NOT EXISTS voice_deliveries (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, records TEXT NOT NULL, text TEXT NOT NULL, edited_text TEXT);
    CREATE TABLE IF NOT EXISTS voice_speech_requests (
                id TEXT PRIMARY KEY, session TEXT NOT NULL, record_id TEXT NOT NULL,
                state TEXT NOT NULL, error TEXT);
"#;

// These tables are installed by Runtime and feature constructors, or on first
// feature use, in canvas.sqlite3. Keep their DDL in step with Python creators.
const FEATURE_SCHEMA: &str = r#"
            CREATE TABLE IF NOT EXISTS runtime_work (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_work_root_status
                ON runtime_work(json_extract(record,'$.rootId'),json_extract(record,'$.status'));
            CREATE INDEX IF NOT EXISTS runtime_work_owner_status
                ON runtime_work(json_extract(record,'$.owner'),json_extract(record,'$.status'));
            CREATE INDEX IF NOT EXISTS runtime_work_archive_due ON runtime_work(
                COALESCE(json_extract(record,'$.archiveIntent.nextAttemptAt'),0),id)
                WHERE json_extract(record,'$.archiveIntent.status')='pending';
            CREATE TABLE IF NOT EXISTS runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_item_fulltext (id TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
            CREATE TABLE IF NOT EXISTS runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
            CREATE VIRTUAL TABLE IF NOT EXISTS runtime_search_next USING fts5(body,content='',contentless_delete=1,tokenize='unicode61');
            CREATE TABLE IF NOT EXISTS runtime_payload_migrations (name TEXT PRIMARY KEY,cursor INTEGER NOT NULL DEFAULT 0,complete INTEGER NOT NULL DEFAULT 0,status TEXT NOT NULL DEFAULT 'pending',updated REAL,error TEXT);
            CREATE TABLE IF NOT EXISTS runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_sidebar_order (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_workspace_operation_phase ON runtime_workspace_operations(
                json_extract(record,'$.phase'), json_extract(record,'$.agent'));
            CREATE TABLE IF NOT EXISTS runtime_payload_refs (
            table_name TEXT NOT NULL, record_id TEXT NOT NULL, field TEXT NOT NULL,
            sha256 TEXT NOT NULL, PRIMARY KEY(table_name,record_id,field));
            CREATE INDEX IF NOT EXISTS runtime_payload_refs_hash ON runtime_payload_refs(sha256);
            CREATE TRIGGER IF NOT EXISTS runtime_payload_checkpoints_insert AFTER INSERT ON runtime_checkpoints BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
            CREATE TRIGGER IF NOT EXISTS runtime_payload_checkpoints_update AFTER UPDATE OF record ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
            CREATE TRIGGER IF NOT EXISTS runtime_payload_checkpoints_delete AFTER DELETE ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
        END;
            CREATE TABLE IF NOT EXISTS runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_tool_request_actor
                ON runtime_tool_requests(json_extract(record,'$.agent'), json_extract(record,'$.updated') DESC);
            CREATE TABLE IF NOT EXISTS runtime_tool_request_aliases
                (agent TEXT NOT NULL, alias TEXT NOT NULL, request TEXT NOT NULL, PRIMARY KEY(agent,alias,request));
            CREATE TRIGGER IF NOT EXISTS runtime_payload_requests_insert AFTER INSERT ON runtime_tool_requests BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
            CREATE TRIGGER IF NOT EXISTS runtime_payload_requests_update AFTER UPDATE OF record ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
            CREATE TRIGGER IF NOT EXISTS runtime_payload_requests_delete AFTER DELETE ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
        END;
            CREATE TABLE IF NOT EXISTS runtime_tool_results (id TEXT PRIMARY KEY, result TEXT NOT NULL);
            CREATE TRIGGER IF NOT EXISTS runtime_payload_results_insert AFTER INSERT ON runtime_tool_results BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
            CREATE TRIGGER IF NOT EXISTS runtime_payload_results_update AFTER UPDATE OF result ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
            CREATE TRIGGER IF NOT EXISTS runtime_payload_results_delete AFTER DELETE ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
        END;
            CREATE TABLE IF NOT EXISTS runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_rule_ready_tick
                ON runtime_rules(json_extract(record,'$.status'))
                WHERE NOT COALESCE(json_extract(record,'$.inFlight'),0);
            CREATE TABLE IF NOT EXISTS runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_budget_usage (id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL, tokens INTEGER NOT NULL, at REAL NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_budget_usage_agent ON runtime_budget_usage(agent,kind);
            CREATE TABLE IF NOT EXISTS runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_execution_run_agent ON runtime_execution_runs(agent,created DESC);
            CREATE INDEX IF NOT EXISTS runtime_execution_run_created ON runtime_execution_runs(created DESC);
            CREATE INDEX IF NOT EXISTS runtime_execution_run_message ON runtime_execution_runs(agent,thread,turn);
            CREATE INDEX IF NOT EXISTS runtime_execution_run_native ON runtime_execution_runs(agent,account,thread,turn);
            CREATE TABLE IF NOT EXISTS runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_execution_attempt_run ON runtime_execution_attempts(run);
            CREATE TABLE IF NOT EXISTS runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
            CREATE INDEX IF NOT EXISTS runtime_execution_input_event ON runtime_execution_inputs(event);
            CREATE TABLE IF NOT EXISTS runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_execution_effect_run ON runtime_execution_effects(run);
            CREATE TABLE IF NOT EXISTS runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_execution_node_run ON runtime_execution_nodes(run);
        CREATE TABLE IF NOT EXISTS runtime_federation_settings (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_identity (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_peers (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_invites (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_rooms (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS runtime_federation_outbox (
          id TEXT PRIMARY KEY, peer_id TEXT NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS runtime_federation_outbox_peer
          ON runtime_federation_outbox(peer_id);
        CREATE TABLE IF NOT EXISTS runtime_federation_inbox (
          peer_id TEXT NOT NULL, message_id TEXT NOT NULL, record TEXT NOT NULL,
          PRIMARY KEY(peer_id,message_id));
        CREATE TABLE IF NOT EXISTS runtime_federation_nonces (
          peer_id TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL,
          PRIMARY KEY(peer_id,nonce));
        CREATE INDEX IF NOT EXISTS runtime_federation_nonce_age
          ON runtime_federation_nonces(created);
            CREATE INDEX IF NOT EXISTS runtime_workspace_operation_agent_phase ON runtime_workspace_operations(json_extract(record,'$.agent'),json_extract(record,'$.phase'));
            CREATE INDEX IF NOT EXISTS runtime_request_agent_status ON runtime_requests(json_extract(record,'$.agent'),json_extract(record,'$.status'));
            CREATE INDEX IF NOT EXISTS runtime_rule_agent_inflight ON runtime_rules(json_extract(record,'$.agent'),json_extract(record,'$.inFlight'));
            CREATE INDEX IF NOT EXISTS runtime_conversion_rules_rootId ON runtime_rules(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
            CREATE INDEX IF NOT EXISTS runtime_conversion_plans_rootId ON runtime_plans(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
            CREATE INDEX IF NOT EXISTS runtime_conversion_annotations_rootId ON runtime_annotations(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
            CREATE INDEX IF NOT EXISTS runtime_conversion_complaints_leadId ON runtime_complaints(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
            CREATE INDEX IF NOT EXISTS runtime_conversion_event_meta_rootId ON runtime_event_meta(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
            CREATE INDEX IF NOT EXISTS runtime_conversion_event_meta_leadId ON runtime_event_meta(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
            CREATE INDEX IF NOT EXISTS runtime_room_root ON runtime_rooms(json_extract(record,'$.rootId'));
            CREATE INDEX IF NOT EXISTS runtime_room_member_first ON runtime_rooms(json_extract(record,'$.members[0]'));
            CREATE INDEX IF NOT EXISTS runtime_room_member_second ON runtime_rooms(json_extract(record,'$.members[1]'));
            CREATE INDEX IF NOT EXISTS runtime_room_many_members ON runtime_rooms(json_array_length(record,'$.members')) WHERE json_array_length(record,'$.members')>2;
            CREATE TABLE IF NOT EXISTS groups (
                  id TEXT PRIMARY KEY, name TEXT NOT NULL, members TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS messages (
                  id TEXT PRIMARY KEY, room TEXT NOT NULL, author TEXT NOT NULL,
                  text TEXT NOT NULL, at REAL NOT NULL, deliveries TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS messages_room ON messages(room, at);
            CREATE TABLE IF NOT EXISTS graph_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS graph_edges (
                  id TEXT PRIMARY KEY, source TEXT NOT NULL, target TEXT NOT NULL,
                  kind TEXT NOT NULL, UNIQUE(source, target, kind));
            CREATE TABLE IF NOT EXISTS user_terminals (id TEXT PRIMARY KEY, record TEXT NOT NULL, output TEXT NOT NULL, offset INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS user_terminal_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS user_terminal_output (terminal TEXT NOT NULL, start INTEGER NOT NULL, end INTEGER NOT NULL, text TEXT NOT NULL, PRIMARY KEY (terminal,start));
            CREATE INDEX IF NOT EXISTS user_terminal_output_end ON user_terminal_output (terminal,end);
            CREATE TABLE IF NOT EXISTS user_terminal_event_cursor (terminal TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS user_terminal_decoder_state (terminal TEXT PRIMARY KEY, pending BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_settings(id INTEGER PRIMARY KEY CHECK(id=1), auto_pair INTEGER NOT NULL);
            INSERT OR IGNORE INTO runtime_access_settings VALUES(1,1);
            CREATE TABLE IF NOT EXISTS runtime_access_discovered(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_auto_attempts(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_unrevokes(id TEXT PRIMARY KEY, client TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_clients (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_invites (
          id TEXT PRIMARY KEY, request_id TEXT NOT NULL UNIQUE, hash TEXT NOT NULL, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_nonces (
          client TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL, PRIMARY KEY(client,nonce));
            CREATE INDEX IF NOT EXISTS runtime_access_nonce_age ON runtime_access_nonces(created);
            CREATE TABLE IF NOT EXISTS runtime_access_receipts (
          client TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL, session TEXT NOT NULL,
          status TEXT NOT NULL, record TEXT NOT NULL, PRIMARY KEY(client,request_id));
            CREATE TABLE IF NOT EXISTS runtime_access_outbound (
          server TEXT NOT NULL, request_id TEXT NOT NULL, hash TEXT NOT NULL,
          record TEXT NOT NULL, PRIMARY KEY(server,request_id));
            CREATE INDEX IF NOT EXISTS runtime_access_receipt_age
          ON runtime_access_receipts(json_extract(record,'$.completed')) WHERE status='complete';
            CREATE INDEX IF NOT EXISTS runtime_access_outbound_age
          ON runtime_access_outbound(json_extract(record,'$.completed'))
          WHERE json_extract(record,'$.outcome')='complete';
            CREATE TABLE IF NOT EXISTS runtime_access_audit (
          seq INTEGER PRIMARY KEY AUTOINCREMENT, client TEXT NOT NULL, actor TEXT NOT NULL,
          action TEXT NOT NULL, created REAL NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_local_alias (
          id INTEGER PRIMARY KEY CHECK(id=1), alias TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_name_targets (
          id TEXT PRIMARY KEY, server TEXT NOT NULL, label TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_name_requests (
          id TEXT PRIMARY KEY, label TEXT NOT NULL, actor TEXT NOT NULL, applied INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_access_alias_requests (
          id TEXT PRIMARY KEY, server TEXT NOT NULL, alias TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_model_status (agent TEXT, revision TEXT, at REAL, record TEXT, PRIMARY KEY(agent,revision));
            CREATE TABLE IF NOT EXISTS runtime_model_modes (agent TEXT, request TEXT, version TEXT, record TEXT, PRIMARY KEY(agent,request,version));
            CREATE TABLE IF NOT EXISTS runtime_native_notices (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_native_catalog_updates (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_native_limit_refreshes (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_native_sweeps (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_supervisor_cursor (handle TEXT PRIMARY KEY, sequence INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_agent_sweep_busy ON runtime_agents(COALESCE(json_extract(record,'$.accountKey'),'default')) WHERE COALESCE(json_extract(record,'$.provider'),'codex')='codex' AND (json_extract(record,'$.contextRepair.phase') IN ('preparing','submitted','unknown','ready') OR COALESCE(json_extract(record,'$.nativeToolRefreshId'),'')!='');
            CREATE INDEX IF NOT EXISTS runtime_agent_sweep_cleanup ON runtime_agents(json_extract(record,'$.contextRepair.sourceCleanup.accountKey'),json_extract(record,'$.contextRepair.sourceCleanup.threadId')) WHERE json_extract(record,'$.contextRepair.sourceCleanup.phase') IN ('planned','submitted');
            CREATE TABLE IF NOT EXISTS runtime_native_action_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, receipt TEXT NOT NULL, outcome TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_safety_retries (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_context_terminal_receipts (id TEXT PRIMARY KEY, agent TEXT NOT NULL, operation TEXT NOT NULL, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'));
            CREATE TABLE IF NOT EXISTS runtime_limit_reset_attempts (id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_limit_reset_requests (id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_server_outbox (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, body TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                    next_at REAL NOT NULL DEFAULT 0, error TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, fingerprint TEXT);
            CREATE INDEX IF NOT EXISTS runtime_server_outbox_due
                    ON runtime_server_outbox(state,next_at);
            CREATE TABLE IF NOT EXISTS runtime_server_inbox (
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, state TEXT NOT NULL,
                    result TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, principal TEXT, actor TEXT);
            CREATE TABLE IF NOT EXISTS runtime_server_links (
                    id TEXT PRIMARY KEY, record TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_server_links_home_parent
                    ON runtime_server_links(json_extract(record,'$.parent'))
                    WHERE json_extract(record,'$.side')='home';
            CREATE TABLE IF NOT EXISTS runtime_server_fetch (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, signature TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT);
            CREATE TABLE IF NOT EXISTS runtime_server_retired (
                    id TEXT NOT NULL, direction TEXT NOT NULL, signature TEXT NOT NULL,
                    server TEXT, actor TEXT, PRIMARY KEY(id,direction));
            CREATE TABLE IF NOT EXISTS runtime_server_output_tools (
                    id TEXT PRIMARY KEY, expires REAL NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_server_output_tools_expiry
                    ON runtime_server_output_tools(expires);
            CREATE TABLE IF NOT EXISTS runtime_server_sequence (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
            INSERT OR IGNORE INTO runtime_server_sequence VALUES(1,0);
            CREATE TABLE IF NOT EXISTS runtime_server_exec (
            id TEXT PRIMARY KEY, owner TEXT NOT NULL, actor TEXT NOT NULL,
            record TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS runtime_server_exec_audit (
            id TEXT PRIMARY KEY, actor TEXT NOT NULL, source_server TEXT NOT NULL,
            server TEXT NOT NULL, cwd TEXT NOT NULL, argv_hash TEXT NOT NULL,
            start REAL NOT NULL, end REAL, exit_code INTEGER, signal INTEGER);
            CREATE TABLE IF NOT EXISTS runtime_server_exec_input (
            id TEXT PRIMARY KEY, handle TEXT NOT NULL);
            CREATE INDEX IF NOT EXISTS runtime_server_exec_input_handle
            ON runtime_server_exec_input(handle);
"#;

pub(crate) fn prepare(transaction: &Transaction<'_>) -> StoreResult<()> {
    transaction.execute_batch(FEATURE_SCHEMA)?;
    transaction.execute_batch(VOICE_SCHEMA)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::VOICE_SCHEMA;
    use rusqlite::Connection;

    fn compact(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn voice_bootstrap_catalog_matches_python_voice_store() {
        let mut connection = Connection::open_in_memory().expect("in-memory database opens");
        let transaction = connection.transaction().expect("transaction opens");
        transaction
            .execute_batch(VOICE_SCHEMA)
            .expect("voice schema bootstraps");

        let expected = [
            (
                "table",
                "voice_sessions",
                "CREATE TABLE voice_sessions ( id TEXT PRIMARY KEY, agent TEXT NOT NULL, created REAL NOT NULL, ended REAL, sdp TEXT, answer TEXT, native_thread TEXT, account_key TEXT, connection_id TEXT, state TEXT, error TEXT)",
            ),
            (
                "table",
                "voice_records",
                "CREATE TABLE voice_records ( seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, agent TEXT NOT NULL, session TEXT NOT NULL, kind TEXT NOT NULL, text TEXT NOT NULL, item_id TEXT NOT NULL, previous_item_id TEXT NOT NULL, payload TEXT NOT NULL, created REAL NOT NULL)",
            ),
            (
                "index",
                "voice_records_agent",
                "CREATE INDEX voice_records_agent ON voice_records(agent,seq)",
            ),
            (
                "table",
                "voice_approvals",
                "CREATE TABLE voice_approvals (id TEXT PRIMARY KEY, agent TEXT NOT NULL, speech TEXT NOT NULL, transcript TEXT NOT NULL, status TEXT NOT NULL, result TEXT)",
            ),
            (
                "table",
                "voice_deliveries",
                "CREATE TABLE voice_deliveries ( id TEXT PRIMARY KEY, agent TEXT NOT NULL, records TEXT NOT NULL, text TEXT NOT NULL, edited_text TEXT)",
            ),
            (
                "table",
                "voice_speech_requests",
                "CREATE TABLE voice_speech_requests ( id TEXT PRIMARY KEY, session TEXT NOT NULL, record_id TEXT NOT NULL, state TEXT NOT NULL, error TEXT)",
            ),
        ];
        for (kind, name, python_sql) in expected {
            let rust_sql: String = transaction
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                    [kind, name],
                    |row| row.get(0),
                )
                .unwrap_or_else(|_| panic!("bootstrap is missing {name}"));
            assert_eq!(
                compact(&rust_sql),
                compact(python_sql),
                "{name} DDL differs"
            );
        }
    }
}
