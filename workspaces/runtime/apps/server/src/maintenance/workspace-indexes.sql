-- Run only as an explicitly scheduled maintenance operation. Building these
-- indexes on the live canvas database takes SQLite's writer lock. Runtime
-- startup does not execute this file; scoped reads work without these indexes.
CREATE INDEX IF NOT EXISTS runtime_agent_workspace_root
    ON runtime_agents(json_extract(record,'$.rootId'));
CREATE INDEX IF NOT EXISTS runtime_request_workspace_agent_status
    ON runtime_requests(json_extract(record,'$.agent'), json_extract(record,'$.status'));
CREATE INDEX IF NOT EXISTS runtime_complaint_workspace_lead
    ON runtime_complaints(json_extract(record,'$.leadId'));
CREATE INDEX IF NOT EXISTS runtime_monitor_workspace_agent_status_created
    ON runtime_monitors(json_extract(record,'$.agent'), json_extract(record,'$.status'), json_extract(record,'$.created'));
CREATE INDEX IF NOT EXISTS runtime_work_workspace_root_status
    ON runtime_work(json_extract(record,'$.rootId'), json_extract(record,'$.status'));
CREATE INDEX IF NOT EXISTS runtime_work_workspace_owner_status
    ON runtime_work(json_extract(record,'$.owner'), json_extract(record,'$.status'));
CREATE INDEX IF NOT EXISTS runtime_task_workspace_agent_status_created
    ON runtime_tasks(json_extract(record,'$.agent'), json_extract(record,'$.status'), json_extract(record,'$.created') DESC);
CREATE INDEX IF NOT EXISTS runtime_task_agent_created_id
    ON runtime_tasks(json_extract(record,'$.agent'), json_extract(record,'$.created') DESC, id DESC);
CREATE INDEX IF NOT EXISTS runtime_task_agent_updated_id
    ON runtime_tasks(json_extract(record,'$.agent'),
        CASE WHEN COALESCE(json_extract(record,'$.finished'),0) > COALESCE(json_extract(record,'$.created'),0)
             THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END,
        id);
CREATE INDEX IF NOT EXISTS runtime_annotation_workspace_agent
    ON runtime_annotations(json_extract(record,'$.agent'));
CREATE INDEX IF NOT EXISTS runtime_checkpoint_workspace_agent
    ON runtime_checkpoints(json_extract(record,'$.agent'));
CREATE INDEX IF NOT EXISTS runtime_rule_workspace_agent
    ON runtime_rules(json_extract(record,'$.agent'));
