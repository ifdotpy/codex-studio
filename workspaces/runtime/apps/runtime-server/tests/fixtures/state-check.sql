CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY, agent TEXT, record TEXT NOT NULL);
INSERT INTO runtime_agents VALUES
('valid-secret-id', '{"id":"valid-secret-id","rootId":"root-secret","epoch":0,"status":"idle","sandbox":null}'),
('invalid-secret-id', '{"id":"invalid-secret-id","rootId":"root-secret","epoch":"broken","status":"idle","private":"private-record-content"}');
INSERT INTO runtime_tasks VALUES ('task-secret-id', 'valid-secret-id', '{"id":"task-secret-id","agent":"valid-secret-id","reattachRecovery":{"state":"pending"},"outputPreviewOnly":true}');
CREATE TABLE runtime_execution_attempts(id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO runtime_execution_attempts VALUES
('attempt-good', '{"id":"attempt-good","epoch":2}'),
('attempt-bad', '{"id":"attempt-bad","epoch":"wrong"}');
