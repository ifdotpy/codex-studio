BEGIN TRANSACTION;
CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_moves(id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_agent_record_generation (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
INSERT INTO "runtime_agent_record_generation" VALUES(1,14);
CREATE TABLE runtime_agents (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_agents" VALUES('agent','{"id": "agent", "rootId": "agent", "epoch": 1, "status": "approval", "cwd": "@FIXTURE_ROOT@", "threadId": "thread", "accountKey": "default", "turnId": "turn", "inFlight": true, "events": 7, "activeTools": [], "autoWake": true, "isLead": true, "name": "Transcript fixture", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "concurrency": 32, "maxAgentsExplicit": false, "subagentConcurrencyVersion": 2, "nativeResponseTurn": "turn", "lastEvent": "2026-10-22T17:20:38Z", "activity": {"phase": "writing", "at": 300.0}, "compactions": 1, "contextUsage": null, "tail": "Needle streamed answer", "workspaceMode": "image"}');
INSERT INTO "runtime_agents" VALUES('other','{"id": "other", "rootId": "other", "epoch": 1, "status": "running", "cwd": "@FIXTURE_ROOT@", "threadId": "other-thread", "accountKey": "default", "turnId": "turn", "inFlight": true, "events": 0, "activeTools": [], "autoWake": true, "isLead": true, "name": "Transcript fixture", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "concurrency": 32, "maxAgentsExplicit": false, "subagentConcurrencyVersion": 2}');
INSERT INTO "runtime_agents" VALUES('child','{"id": "child", "rootId": "agent", "epoch": 1, "status": "running", "cwd": "@FIXTURE_ROOT@", "threadId": "child-thread", "accountKey": "default", "turnId": "turn", "inFlight": true, "events": 0, "activeTools": [], "autoWake": true, "isLead": false, "name": "Transcript fixture", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "concurrency": 32, "maxAgentsExplicit": false, "subagentConcurrencyVersion": 2, "parentId": "agent"}');
INSERT INTO "runtime_agents" VALUES('gone','{"id": "gone", "rootId": "agent", "epoch": 1, "status": "approval", "cwd": "@FIXTURE_ROOT@", "threadId": "thread", "accountKey": "default", "turnId": "turn", "inFlight": true, "events": 7, "activeTools": [], "autoWake": true, "isLead": true, "name": "Transcript fixture", "agentMode": "multi", "agentModeRevision": 0, "agentModeSupported": true, "concurrency": 32, "maxAgentsExplicit": false, "subagentConcurrencyVersion": 2, "nativeResponseTurn": "turn", "lastEvent": "2026-10-22T17:20:38Z", "activity": {"phase": "writing", "at": 300.0}, "compactions": 1, "contextUsage": null, "tail": "Needle streamed answer", "workspaceMode": "image", "deletedAt": 1.0}');
CREATE TABLE runtime_annotations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_assets (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_assets" VALUES('attachment','{"id": "attachment", "agent": "gone", "name": "fixture.txt", "path": "@FIXTURE_ROOT@/absent", "size": 1, "image": false, "mime": "text/plain", "hash": "sha256:fixture", "created": 1.0}');
CREATE TABLE runtime_budget (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_budget_usage (id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL, tokens INTEGER NOT NULL, at REAL NOT NULL);
CREATE TABLE runtime_capacity_retries (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_chat_messages (
                  seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL,
                  room TEXT NOT NULL, sender TEXT NOT NULL, text TEXT NOT NULL,
                  created REAL NOT NULL, deliveries TEXT NOT NULL);
INSERT INTO "runtime_chat_messages" VALUES(1,'chat','private','child','needle room',210.0,'{}');
INSERT INTO "runtime_chat_messages" VALUES(2,'chat-peer','peer','agent','needle peer',800.0,'{}');
INSERT INTO "runtime_chat_messages" VALUES(3,'chat-unavailable','unavailable','agent','needle unavailable',800.0,'{}');
INSERT INTO "runtime_chat_messages" VALUES(4,'chat-approved','approved','agent','needle approved',800.0,'{}');
INSERT INTO "runtime_chat_messages" VALUES(5,'chat-unapproved','unapproved','agent','needle unapproved',800.0,'{}');
CREATE TABLE runtime_checkpoints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_compactions (id TEXT PRIMARY KEY, agent TEXT NOT NULL);
INSERT INTO "runtime_compactions" VALUES('agent:compact','agent');
CREATE TABLE runtime_complaints (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_complaints" VALUES('complaint','{"id": "complaint", "leadId": "agent", "title": "needle complaint", "text": "needle detail", "status": "open"}');
CREATE TABLE runtime_completed_turns (id TEXT PRIMARY KEY);
INSERT INTO "runtime_completed_turns" VALUES('agent:turn');
CREATE TABLE runtime_context_manifests (
                  agent TEXT NOT NULL, thread_id TEXT NOT NULL, compactions INTEGER NOT NULL,
                  sequence INTEGER NOT NULL, event_id TEXT NOT NULL, record TEXT NOT NULL,
                  PRIMARY KEY(agent,thread_id,compactions));
CREATE TABLE runtime_context_reminders (
                  agent TEXT NOT NULL, version TEXT NOT NULL, event_id TEXT NOT NULL,
                  PRIMARY KEY(agent,version));
CREATE TABLE runtime_event_meta (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_event_meta" VALUES('input','{"requestedDelivery": "steer", "transcriptItemId": "agent:input"}');
INSERT INTO "runtime_event_meta" VALUES('legacy-event','{"delivery":"current_turn","timing":{"enqueuedAt":228874508067265,"fastSkipReason":"scheduleOverride"},"transcriptItemId":"agent:legacy"}');
INSERT INTO "runtime_event_meta" VALUES('pending','{"delivery":"current_turn","timing":{"enqueuedAt":228874422279577,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-0','{"delivery":"current_turn","timing":{"enqueuedAt":228874635419216,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-1','{"delivery":"current_turn","timing":{"enqueuedAt":228874636304034,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-2','{"delivery":"current_turn","timing":{"enqueuedAt":228874636600789,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-3','{"delivery":"current_turn","timing":{"enqueuedAt":228874636862900,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-4','{"delivery":"current_turn","timing":{"enqueuedAt":228874637097470,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-5','{"delivery":"current_turn","timing":{"enqueuedAt":228874637260505,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-6','{"delivery":"current_turn","timing":{"enqueuedAt":228874637424412,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-7','{"delivery":"current_turn","timing":{"enqueuedAt":228874637585474,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-8','{"delivery":"current_turn","timing":{"enqueuedAt":228874637750052,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-9','{"delivery":"current_turn","timing":{"enqueuedAt":228874637902848,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-10','{"delivery":"current_turn","timing":{"enqueuedAt":228874638060253,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-11','{"delivery":"current_turn","timing":{"enqueuedAt":228874638211456,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-12','{"delivery":"current_turn","timing":{"enqueuedAt":228874638362138,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-13','{"delivery":"current_turn","timing":{"enqueuedAt":228874638512420,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-14','{"delivery":"current_turn","timing":{"enqueuedAt":228874638664064,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-15','{"delivery":"current_turn","timing":{"enqueuedAt":228874638816108,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-16','{"delivery":"current_turn","timing":{"enqueuedAt":228874638967602,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-17','{"delivery":"current_turn","timing":{"enqueuedAt":228874639123965,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-18','{"delivery":"current_turn","timing":{"enqueuedAt":228874639274357,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-19','{"delivery":"current_turn","timing":{"enqueuedAt":228874639425750,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-20','{"delivery":"current_turn","timing":{"enqueuedAt":228874639635944,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-21','{"delivery":"current_turn","timing":{"enqueuedAt":228874639819037,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-22','{"delivery":"current_turn","timing":{"enqueuedAt":228874639992982,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-23','{"delivery":"current_turn","timing":{"enqueuedAt":228874640170966,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-24','{"delivery":"current_turn","timing":{"enqueuedAt":228874640344701,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-25','{"delivery":"current_turn","timing":{"enqueuedAt":228874640535398,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-26','{"delivery":"current_turn","timing":{"enqueuedAt":228874640721727,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-27','{"delivery":"current_turn","timing":{"enqueuedAt":228874640901143,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-28','{"delivery":"current_turn","timing":{"enqueuedAt":228874641226472,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-29','{"delivery":"current_turn","timing":{"enqueuedAt":228874641522317,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-30','{"delivery":"current_turn","timing":{"enqueuedAt":228874641770912,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-31','{"delivery":"current_turn","timing":{"enqueuedAt":228874642041148,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-32','{"delivery":"current_turn","timing":{"enqueuedAt":228874642374212,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-33','{"delivery":"current_turn","timing":{"enqueuedAt":228874642692608,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-34','{"delivery":"current_turn","timing":{"enqueuedAt":228874643036603,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-35','{"delivery":"current_turn","timing":{"enqueuedAt":228874643357934,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-36','{"delivery":"current_turn","timing":{"enqueuedAt":228874643662645,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-37','{"delivery":"current_turn","timing":{"enqueuedAt":228874643976213,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-38','{"delivery":"current_turn","timing":{"enqueuedAt":228874644290241,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-39','{"delivery":"current_turn","timing":{"enqueuedAt":228874644614969,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-40','{"delivery":"current_turn","timing":{"enqueuedAt":228874644918517,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-41','{"delivery":"current_turn","timing":{"enqueuedAt":228874645220182,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-42','{"delivery":"current_turn","timing":{"enqueuedAt":228874645525244,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-43','{"delivery":"current_turn","timing":{"enqueuedAt":228874645834362,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-44','{"delivery":"current_turn","timing":{"enqueuedAt":228874646129836,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-45','{"delivery":"current_turn","timing":{"enqueuedAt":228874646405322,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-46','{"delivery":"current_turn","timing":{"enqueuedAt":228874646686749,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-47','{"delivery":"current_turn","timing":{"enqueuedAt":228874646960542,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-48','{"delivery":"current_turn","timing":{"enqueuedAt":228874647257808,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-49','{"delivery":"current_turn","timing":{"enqueuedAt":228874647540959,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-50','{"delivery":"current_turn","timing":{"enqueuedAt":228874647836522,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-51','{"delivery":"current_turn","timing":{"enqueuedAt":228874648127437,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-52','{"delivery":"current_turn","timing":{"enqueuedAt":228874648439922,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-53','{"delivery":"current_turn","timing":{"enqueuedAt":228874648716440,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-54','{"delivery":"current_turn","timing":{"enqueuedAt":228874649041589,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-55','{"delivery":"current_turn","timing":{"enqueuedAt":228874649348624,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-56','{"delivery":"current_turn","timing":{"enqueuedAt":228874649612228,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-57','{"delivery":"current_turn","timing":{"enqueuedAt":228874649822542,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-58','{"delivery":"current_turn","timing":{"enqueuedAt":228874650105872,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-59','{"delivery":"current_turn","timing":{"enqueuedAt":228874650450438,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-60','{"delivery":"current_turn","timing":{"enqueuedAt":228874650776077,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-61','{"delivery":"current_turn","timing":{"enqueuedAt":228874651101848,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-62','{"delivery":"current_turn","timing":{"enqueuedAt":228874651456802,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-63','{"delivery":"current_turn","timing":{"enqueuedAt":228874651758628,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-64','{"delivery":"current_turn","timing":{"enqueuedAt":228874651990723,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-65','{"delivery":"current_turn","timing":{"enqueuedAt":228874652235621,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-66','{"delivery":"current_turn","timing":{"enqueuedAt":228874652447789,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-67','{"delivery":"current_turn","timing":{"enqueuedAt":228874652656049,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-68','{"delivery":"current_turn","timing":{"enqueuedAt":228874652874418,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-69','{"delivery":"current_turn","timing":{"enqueuedAt":228874653094149,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-70','{"delivery":"current_turn","timing":{"enqueuedAt":228874653308240,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-71','{"delivery":"current_turn","timing":{"enqueuedAt":228874653514937,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-72','{"delivery":"current_turn","timing":{"enqueuedAt":228874653723488,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-73','{"delivery":"current_turn","timing":{"enqueuedAt":228874653928993,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-74','{"delivery":"current_turn","timing":{"enqueuedAt":228874654144676,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-75','{"delivery":"current_turn","timing":{"enqueuedAt":228874654353698,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-76','{"delivery":"current_turn","timing":{"enqueuedAt":228874654563801,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-77','{"delivery":"current_turn","timing":{"enqueuedAt":228874654772793,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-78','{"delivery":"current_turn","timing":{"enqueuedAt":228874655016389,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-79','{"delivery":"current_turn","timing":{"enqueuedAt":228874655230470,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-80','{"delivery":"current_turn","timing":{"enqueuedAt":228874655473275,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-81','{"delivery":"current_turn","timing":{"enqueuedAt":228874655759841,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-82','{"delivery":"current_turn","timing":{"enqueuedAt":228874656006443,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-83','{"delivery":"current_turn","timing":{"enqueuedAt":228874656234040,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-84','{"delivery":"current_turn","timing":{"enqueuedAt":228874656450184,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-85','{"delivery":"current_turn","timing":{"enqueuedAt":228874656666930,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-86','{"delivery":"current_turn","timing":{"enqueuedAt":228874656884027,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-87','{"delivery":"current_turn","timing":{"enqueuedAt":228874657122103,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-88','{"delivery":"current_turn","timing":{"enqueuedAt":228874657347865,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-89','{"delivery":"current_turn","timing":{"enqueuedAt":228874657578818,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-90','{"delivery":"current_turn","timing":{"enqueuedAt":228874657826001,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-91','{"delivery":"current_turn","timing":{"enqueuedAt":228874658126374,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-92','{"delivery":"current_turn","timing":{"enqueuedAt":228874658375590,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-93','{"delivery":"current_turn","timing":{"enqueuedAt":228874658608126,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-94','{"delivery":"current_turn","timing":{"enqueuedAt":228874658837946,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-95','{"delivery":"current_turn","timing":{"enqueuedAt":228874659090399,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-96','{"delivery":"current_turn","timing":{"enqueuedAt":228874659323155,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-97','{"delivery":"current_turn","timing":{"enqueuedAt":228874659558315,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-98','{"delivery":"current_turn","timing":{"enqueuedAt":228874659790781,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-99','{"delivery":"current_turn","timing":{"enqueuedAt":228874660033986,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-100','{"delivery":"current_turn","timing":{"enqueuedAt":228874660271571,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-101','{"delivery":"current_turn","timing":{"enqueuedAt":228874660501051,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-102','{"delivery":"current_turn","timing":{"enqueuedAt":228874660723297,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-103','{"delivery":"current_turn","timing":{"enqueuedAt":228874660951555,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-104','{"delivery":"current_turn","timing":{"enqueuedAt":228874661187066,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-105','{"delivery":"current_turn","timing":{"enqueuedAt":228874661410384,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-106','{"delivery":"current_turn","timing":{"enqueuedAt":228874661632971,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-107','{"delivery":"current_turn","timing":{"enqueuedAt":228874661875966,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-108','{"delivery":"current_turn","timing":{"enqueuedAt":228874662126004,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-109','{"delivery":"current_turn","timing":{"enqueuedAt":228874662368729,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-110','{"delivery":"current_turn","timing":{"enqueuedAt":228874662624809,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-111','{"delivery":"current_turn","timing":{"enqueuedAt":228874662868224,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-112','{"delivery":"current_turn","timing":{"enqueuedAt":228874663160141,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-113','{"delivery":"current_turn","timing":{"enqueuedAt":228874663394500,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-114','{"delivery":"current_turn","timing":{"enqueuedAt":228874663625893,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-115','{"delivery":"current_turn","timing":{"enqueuedAt":228874663859280,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-116','{"delivery":"current_turn","timing":{"enqueuedAt":228874664116813,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-117','{"delivery":"current_turn","timing":{"enqueuedAt":228874664355870,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-118','{"delivery":"current_turn","timing":{"enqueuedAt":228874664593075,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-119','{"delivery":"current_turn","timing":{"enqueuedAt":228874664827454,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-120','{"delivery":"current_turn","timing":{"enqueuedAt":228874665073675,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-121','{"delivery":"current_turn","timing":{"enqueuedAt":228874665312622,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-122','{"delivery":"current_turn","timing":{"enqueuedAt":228874665548474,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-123','{"delivery":"current_turn","timing":{"enqueuedAt":228874665795226,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-124','{"delivery":"current_turn","timing":{"enqueuedAt":228874666062817,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-125','{"delivery":"current_turn","timing":{"enqueuedAt":228874666416169,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-126','{"delivery":"current_turn","timing":{"enqueuedAt":228874666794327,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-127','{"delivery":"current_turn","timing":{"enqueuedAt":228874667147389,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-128','{"delivery":"current_turn","timing":{"enqueuedAt":228874667407836,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-129','{"delivery":"current_turn","timing":{"enqueuedAt":228874667668685,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-130','{"delivery":"current_turn","timing":{"enqueuedAt":228874667921548,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-131','{"delivery":"current_turn","timing":{"enqueuedAt":228874668173180,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-132','{"delivery":"current_turn","timing":{"enqueuedAt":228874668431303,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-133','{"delivery":"current_turn","timing":{"enqueuedAt":228874668708933,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-134','{"delivery":"current_turn","timing":{"enqueuedAt":228874668969762,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-135','{"delivery":"current_turn","timing":{"enqueuedAt":228874669236501,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-136','{"delivery":"current_turn","timing":{"enqueuedAt":228874669488433,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-137','{"delivery":"current_turn","timing":{"enqueuedAt":228874669867273,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-138','{"delivery":"current_turn","timing":{"enqueuedAt":228874670248527,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-139','{"delivery":"current_turn","timing":{"enqueuedAt":228874670618249,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-140','{"delivery":"current_turn","timing":{"enqueuedAt":228874671002719,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-141','{"delivery":"current_turn","timing":{"enqueuedAt":228874671392229,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-142','{"delivery":"current_turn","timing":{"enqueuedAt":228874671771669,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-143','{"delivery":"current_turn","timing":{"enqueuedAt":228874672135441,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-144','{"delivery":"current_turn","timing":{"enqueuedAt":228874672420765,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-145','{"delivery":"current_turn","timing":{"enqueuedAt":228874672702603,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-146','{"delivery":"current_turn","timing":{"enqueuedAt":228874673015328,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-147','{"delivery":"current_turn","timing":{"enqueuedAt":228874673293890,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-148','{"delivery":"current_turn","timing":{"enqueuedAt":228874673562393,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-149','{"delivery":"current_turn","timing":{"enqueuedAt":228874673826898,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-150','{"delivery":"current_turn","timing":{"enqueuedAt":228874674114707,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-151','{"delivery":"current_turn","timing":{"enqueuedAt":228874674395042,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-152','{"delivery":"current_turn","timing":{"enqueuedAt":228874674669236,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-153','{"delivery":"current_turn","timing":{"enqueuedAt":228874674943049,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-154','{"delivery":"current_turn","timing":{"enqueuedAt":228874675231429,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-155','{"delivery":"current_turn","timing":{"enqueuedAt":228874675519709,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-156','{"delivery":"current_turn","timing":{"enqueuedAt":228874675808059,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-157','{"delivery":"current_turn","timing":{"enqueuedAt":228874676090928,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-158','{"delivery":"current_turn","timing":{"enqueuedAt":228874676364230,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-159','{"delivery":"current_turn","timing":{"enqueuedAt":228874676662419,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-160','{"delivery":"current_turn","timing":{"enqueuedAt":228874677095380,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-161','{"delivery":"current_turn","timing":{"enqueuedAt":228874677532308,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-162','{"delivery":"current_turn","timing":{"enqueuedAt":228874678074844,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-163','{"delivery":"current_turn","timing":{"enqueuedAt":228874678627219,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-164','{"delivery":"current_turn","timing":{"enqueuedAt":228874679158854,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-165','{"delivery":"current_turn","timing":{"enqueuedAt":228874679693967,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-166','{"delivery":"current_turn","timing":{"enqueuedAt":228874680235320,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-167','{"delivery":"current_turn","timing":{"enqueuedAt":228874680737140,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-168','{"delivery":"current_turn","timing":{"enqueuedAt":228874681256944,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-169','{"delivery":"current_turn","timing":{"enqueuedAt":228874681762180,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-170','{"delivery":"current_turn","timing":{"enqueuedAt":228874682326336,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-171','{"delivery":"current_turn","timing":{"enqueuedAt":228874682887528,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-172','{"delivery":"current_turn","timing":{"enqueuedAt":228874683431086,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-173','{"delivery":"current_turn","timing":{"enqueuedAt":228874683973141,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-174','{"delivery":"current_turn","timing":{"enqueuedAt":228874684525676,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-175','{"delivery":"current_turn","timing":{"enqueuedAt":228874685032866,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-176','{"delivery":"current_turn","timing":{"enqueuedAt":228874685555855,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-177','{"delivery":"current_turn","timing":{"enqueuedAt":228874686098511,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-178','{"delivery":"current_turn","timing":{"enqueuedAt":228874686674149,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-179','{"delivery":"current_turn","timing":{"enqueuedAt":228874687262952,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-180','{"delivery":"current_turn","timing":{"enqueuedAt":228874687813744,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-181','{"delivery":"current_turn","timing":{"enqueuedAt":228874688377409,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-182','{"delivery":"current_turn","timing":{"enqueuedAt":228874688934312,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-183','{"delivery":"current_turn","timing":{"enqueuedAt":228874689455158,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-184','{"delivery":"current_turn","timing":{"enqueuedAt":228874689983397,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-185','{"delivery":"current_turn","timing":{"enqueuedAt":228874690526765,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-186','{"delivery":"current_turn","timing":{"enqueuedAt":228874691111029,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-187','{"delivery":"current_turn","timing":{"enqueuedAt":228874691691997,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-188','{"delivery":"current_turn","timing":{"enqueuedAt":228874692105461,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-189','{"delivery":"current_turn","timing":{"enqueuedAt":228874692451009,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-190','{"delivery":"current_turn","timing":{"enqueuedAt":228874692814119,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-191','{"delivery":"current_turn","timing":{"enqueuedAt":228874693188480,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-192','{"delivery":"current_turn","timing":{"enqueuedAt":228874693619277,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-193','{"delivery":"current_turn","timing":{"enqueuedAt":228874694014296,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-194','{"delivery":"current_turn","timing":{"enqueuedAt":228874694469018,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-195','{"delivery":"current_turn","timing":{"enqueuedAt":228874694986227,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-196','{"delivery":"current_turn","timing":{"enqueuedAt":228874695369985,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-197','{"delivery":"current_turn","timing":{"enqueuedAt":228874695739698,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-198','{"delivery":"current_turn","timing":{"enqueuedAt":228874696103810,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-199','{"delivery":"current_turn","timing":{"enqueuedAt":228874696467180,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-200','{"delivery":"current_turn","timing":{"enqueuedAt":228874696821404,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-201','{"delivery":"current_turn","timing":{"enqueuedAt":228874697180867,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-202','{"delivery":"current_turn","timing":{"enqueuedAt":228874697536653,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-203','{"delivery":"current_turn","timing":{"enqueuedAt":228874697885647,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-204','{"delivery":"current_turn","timing":{"enqueuedAt":228874698269165,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-205','{"delivery":"current_turn","timing":{"enqueuedAt":228874698629620,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-206','{"delivery":"current_turn","timing":{"enqueuedAt":228874699078310,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-207','{"delivery":"current_turn","timing":{"enqueuedAt":228874699672413,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-208','{"delivery":"current_turn","timing":{"enqueuedAt":228874700232432,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-209','{"delivery":"current_turn","timing":{"enqueuedAt":228874700819241,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-210','{"delivery":"current_turn","timing":{"enqueuedAt":228874701403195,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-211','{"delivery":"current_turn","timing":{"enqueuedAt":228874702055987,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-212','{"delivery":"current_turn","timing":{"enqueuedAt":228874702684555,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-213','{"delivery":"current_turn","timing":{"enqueuedAt":228874703304686,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-214','{"delivery":"current_turn","timing":{"enqueuedAt":228874703875365,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-215','{"delivery":"current_turn","timing":{"enqueuedAt":228874704431055,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-216','{"delivery":"current_turn","timing":{"enqueuedAt":228874704952622,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-217','{"delivery":"current_turn","timing":{"enqueuedAt":228874705442650,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-218','{"delivery":"current_turn","timing":{"enqueuedAt":228874705829384,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-219','{"delivery":"current_turn","timing":{"enqueuedAt":228874706197854,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-220','{"delivery":"current_turn","timing":{"enqueuedAt":228874706551577,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-221','{"delivery":"current_turn","timing":{"enqueuedAt":228874706927842,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-222','{"delivery":"current_turn","timing":{"enqueuedAt":228874707291663,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-223','{"delivery":"current_turn","timing":{"enqueuedAt":228874707641859,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-224','{"delivery":"current_turn","timing":{"enqueuedAt":228874708032530,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-225','{"delivery":"current_turn","timing":{"enqueuedAt":228874708387786,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-226','{"delivery":"current_turn","timing":{"enqueuedAt":228874708734255,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-227','{"delivery":"current_turn","timing":{"enqueuedAt":228874709128573,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-228','{"delivery":"current_turn","timing":{"enqueuedAt":228874709717617,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-229','{"delivery":"current_turn","timing":{"enqueuedAt":228874710301750,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-230','{"delivery":"current_turn","timing":{"enqueuedAt":228874710879923,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-231','{"delivery":"current_turn","timing":{"enqueuedAt":228874711402232,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-232','{"delivery":"current_turn","timing":{"enqueuedAt":228874711786181,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-233','{"delivery":"current_turn","timing":{"enqueuedAt":228874712155011,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-234','{"delivery":"current_turn","timing":{"enqueuedAt":228874712517460,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-235','{"delivery":"current_turn","timing":{"enqueuedAt":228874712891791,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-236','{"delivery":"current_turn","timing":{"enqueuedAt":228874713281932,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-237','{"delivery":"current_turn","timing":{"enqueuedAt":228874713648118,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-238','{"delivery":"current_turn","timing":{"enqueuedAt":228874714064888,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-239','{"delivery":"current_turn","timing":{"enqueuedAt":228874714429170,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-240','{"delivery":"current_turn","timing":{"enqueuedAt":228874714788644,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-241','{"delivery":"current_turn","timing":{"enqueuedAt":228874715156953,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-242','{"delivery":"current_turn","timing":{"enqueuedAt":228874715519492,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-243','{"delivery":"current_turn","timing":{"enqueuedAt":228874715897280,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-244','{"delivery":"current_turn","timing":{"enqueuedAt":228874716382449,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-245','{"delivery":"current_turn","timing":{"enqueuedAt":228874717007970,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-246','{"delivery":"current_turn","timing":{"enqueuedAt":228874717613304,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-247','{"delivery":"current_turn","timing":{"enqueuedAt":228874718227594,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-248','{"delivery":"current_turn","timing":{"enqueuedAt":228874718860670,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-249','{"delivery":"current_turn","timing":{"enqueuedAt":228874719477686,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-250','{"delivery":"current_turn","timing":{"enqueuedAt":228874720085714,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-251','{"delivery":"current_turn","timing":{"enqueuedAt":228874720721235,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-252','{"delivery":"current_turn","timing":{"enqueuedAt":228874721350744,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-253','{"delivery":"current_turn","timing":{"enqueuedAt":228874721961517,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-254','{"delivery":"current_turn","timing":{"enqueuedAt":228874722576990,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-255','{"delivery":"current_turn","timing":{"enqueuedAt":228874723200348,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-256','{"delivery":"current_turn","timing":{"enqueuedAt":228874723710263,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-257','{"delivery":"current_turn","timing":{"enqueuedAt":228874724107076,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-258','{"delivery":"current_turn","timing":{"enqueuedAt":228874724501234,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-259','{"delivery":"current_turn","timing":{"enqueuedAt":228874724895753,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-260','{"delivery":"current_turn","timing":{"enqueuedAt":228874725274102,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-261','{"delivery":"current_turn","timing":{"enqueuedAt":228874725644094,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-262','{"delivery":"current_turn","timing":{"enqueuedAt":228874726021612,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-263','{"delivery":"current_turn","timing":{"enqueuedAt":228874726388889,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-264','{"delivery":"current_turn","timing":{"enqueuedAt":228874726828984,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-265','{"delivery":"current_turn","timing":{"enqueuedAt":228874727420020,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-266','{"delivery":"current_turn","timing":{"enqueuedAt":228874727923273,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-267','{"delivery":"current_turn","timing":{"enqueuedAt":228874728362165,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-268','{"delivery":"current_turn","timing":{"enqueuedAt":228874728755081,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-269','{"delivery":"current_turn","timing":{"enqueuedAt":228874729145682,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-270','{"delivery":"current_turn","timing":{"enqueuedAt":228874729529411,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-271','{"delivery":"current_turn","timing":{"enqueuedAt":228874730029708,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-272','{"delivery":"current_turn","timing":{"enqueuedAt":228874730458902,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-273','{"delivery":"current_turn","timing":{"enqueuedAt":228874730842480,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-274','{"delivery":"current_turn","timing":{"enqueuedAt":228874731271754,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-275','{"delivery":"current_turn","timing":{"enqueuedAt":228874731657166,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-276','{"delivery":"current_turn","timing":{"enqueuedAt":228874732041916,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-277','{"delivery":"current_turn","timing":{"enqueuedAt":228874732416458,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-278','{"delivery":"current_turn","timing":{"enqueuedAt":228874732822969,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-279','{"delivery":"current_turn","timing":{"enqueuedAt":228874733401774,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-280','{"delivery":"current_turn","timing":{"enqueuedAt":228874733946884,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-281','{"delivery":"current_turn","timing":{"enqueuedAt":228874734520298,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-282','{"delivery":"current_turn","timing":{"enqueuedAt":228874735088102,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-283','{"delivery":"current_turn","timing":{"enqueuedAt":228874735627492,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-284','{"delivery":"current_turn","timing":{"enqueuedAt":228874736359823,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-285','{"delivery":"current_turn","timing":{"enqueuedAt":228874737133092,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-286','{"delivery":"current_turn","timing":{"enqueuedAt":228874738092528,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-287','{"delivery":"current_turn","timing":{"enqueuedAt":228874739163825,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-288','{"delivery":"current_turn","timing":{"enqueuedAt":228874740146215,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-289','{"delivery":"current_turn","timing":{"enqueuedAt":228874741103999,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-290','{"delivery":"current_turn","timing":{"enqueuedAt":228874742057124,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-291','{"delivery":"current_turn","timing":{"enqueuedAt":228874742987556,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-292','{"delivery":"current_turn","timing":{"enqueuedAt":228874743879567,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-293','{"delivery":"current_turn","timing":{"enqueuedAt":228874744812244,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-294','{"delivery":"current_turn","timing":{"enqueuedAt":228874745785176,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-295','{"delivery":"current_turn","timing":{"enqueuedAt":228874746759150,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-296','{"delivery":"current_turn","timing":{"enqueuedAt":228874747675065,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-297','{"delivery":"current_turn","timing":{"enqueuedAt":228874748732105,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-298','{"delivery":"current_turn","timing":{"enqueuedAt":228874749741325,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-299','{"delivery":"current_turn","timing":{"enqueuedAt":228874750652091,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-300','{"delivery":"current_turn","timing":{"enqueuedAt":228874751542538,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-301','{"delivery":"current_turn","timing":{"enqueuedAt":228874752426915,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-302','{"delivery":"current_turn","timing":{"enqueuedAt":228874753303897,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-303','{"delivery":"current_turn","timing":{"enqueuedAt":228874753889123,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-304','{"delivery":"current_turn","timing":{"enqueuedAt":228874754463609,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-305','{"delivery":"current_turn","timing":{"enqueuedAt":228874755025031,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-306','{"delivery":"current_turn","timing":{"enqueuedAt":228874755600238,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-307','{"delivery":"current_turn","timing":{"enqueuedAt":228874756167029,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-308','{"delivery":"current_turn","timing":{"enqueuedAt":228874756721839,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-309','{"delivery":"current_turn","timing":{"enqueuedAt":228874757275556,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-310','{"delivery":"current_turn","timing":{"enqueuedAt":228874757824654,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-311','{"delivery":"current_turn","timing":{"enqueuedAt":228874758401975,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-312','{"delivery":"current_turn","timing":{"enqueuedAt":228874758958908,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-313','{"delivery":"current_turn","timing":{"enqueuedAt":228874759522985,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-314','{"delivery":"current_turn","timing":{"enqueuedAt":228874760389738,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-315','{"delivery":"current_turn","timing":{"enqueuedAt":228874761319429,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-316','{"delivery":"current_turn","timing":{"enqueuedAt":228874762343697,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-317','{"delivery":"current_turn","timing":{"enqueuedAt":228874763278658,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-318','{"delivery":"current_turn","timing":{"enqueuedAt":228874764958244,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-319','{"delivery":"current_turn","timing":{"enqueuedAt":228874765827562,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-320','{"delivery":"current_turn","timing":{"enqueuedAt":228874766436372,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-321','{"delivery":"current_turn","timing":{"enqueuedAt":228874767020897,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-322','{"delivery":"current_turn","timing":{"enqueuedAt":228874767590073,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-323','{"delivery":"current_turn","timing":{"enqueuedAt":228874768153518,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-324','{"delivery":"current_turn","timing":{"enqueuedAt":228874768709620,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-325','{"delivery":"current_turn","timing":{"enqueuedAt":228874769273526,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-326','{"delivery":"current_turn","timing":{"enqueuedAt":228874769823416,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-327','{"delivery":"current_turn","timing":{"enqueuedAt":228874770387893,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-328','{"delivery":"current_turn","timing":{"enqueuedAt":228874770947631,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-329','{"delivery":"current_turn","timing":{"enqueuedAt":228874771544840,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-330','{"delivery":"current_turn","timing":{"enqueuedAt":228874772169470,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-331','{"delivery":"current_turn","timing":{"enqueuedAt":228874772732785,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-332','{"delivery":"current_turn","timing":{"enqueuedAt":228874773295408,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-333','{"delivery":"current_turn","timing":{"enqueuedAt":228874773871778,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-334','{"delivery":"current_turn","timing":{"enqueuedAt":228874774442607,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-335','{"delivery":"current_turn","timing":{"enqueuedAt":228874775003417,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-336','{"delivery":"current_turn","timing":{"enqueuedAt":228874775563606,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-337','{"delivery":"current_turn","timing":{"enqueuedAt":228874776262846,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-338','{"delivery":"current_turn","timing":{"enqueuedAt":228874777066130,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-339','{"delivery":"current_turn","timing":{"enqueuedAt":228874777999098,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-340','{"delivery":"current_turn","timing":{"enqueuedAt":228874778927957,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-341','{"delivery":"current_turn","timing":{"enqueuedAt":228874779920406,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-342','{"delivery":"current_turn","timing":{"enqueuedAt":228874780705346,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-343','{"delivery":"current_turn","timing":{"enqueuedAt":228874781615040,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-344','{"delivery":"current_turn","timing":{"enqueuedAt":228874782591098,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-345','{"delivery":"current_turn","timing":{"enqueuedAt":228874783667524,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-346','{"delivery":"current_turn","timing":{"enqueuedAt":228874784634395,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-347','{"delivery":"current_turn","timing":{"enqueuedAt":228874785640769,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-348','{"delivery":"current_turn","timing":{"enqueuedAt":228874786674225,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-349','{"delivery":"current_turn","timing":{"enqueuedAt":228874787715786,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-350','{"delivery":"current_turn","timing":{"enqueuedAt":228874788728532,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-351','{"delivery":"current_turn","timing":{"enqueuedAt":228874789376386,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-352','{"delivery":"current_turn","timing":{"enqueuedAt":228874790067791,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-353','{"delivery":"current_turn","timing":{"enqueuedAt":228874790783321,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-354','{"delivery":"current_turn","timing":{"enqueuedAt":228874791555276,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-355','{"delivery":"current_turn","timing":{"enqueuedAt":228874792536684,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-356','{"delivery":"current_turn","timing":{"enqueuedAt":228874793532850,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-357','{"delivery":"current_turn","timing":{"enqueuedAt":228874794541279,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-358','{"delivery":"current_turn","timing":{"enqueuedAt":228874795587648,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-359','{"delivery":"current_turn","timing":{"enqueuedAt":228874796634269,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-360','{"delivery":"current_turn","timing":{"enqueuedAt":228874797681470,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-361','{"delivery":"current_turn","timing":{"enqueuedAt":228874798713503,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-362','{"delivery":"current_turn","timing":{"enqueuedAt":228874799790891,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-363','{"delivery":"current_turn","timing":{"enqueuedAt":228874800835918,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-364','{"delivery":"current_turn","timing":{"enqueuedAt":228874801897235,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-365','{"delivery":"current_turn","timing":{"enqueuedAt":228874802974293,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-366','{"delivery":"current_turn","timing":{"enqueuedAt":228874804050969,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-367','{"delivery":"current_turn","timing":{"enqueuedAt":228874805125201,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-368','{"delivery":"current_turn","timing":{"enqueuedAt":228874806206246,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-369','{"delivery":"current_turn","timing":{"enqueuedAt":228874807155594,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-370','{"delivery":"current_turn","timing":{"enqueuedAt":228874808105112,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-371','{"delivery":"current_turn","timing":{"enqueuedAt":228874809078284,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-372','{"delivery":"current_turn","timing":{"enqueuedAt":228874809994310,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-373','{"delivery":"current_turn","timing":{"enqueuedAt":228874810627025,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-374','{"delivery":"current_turn","timing":{"enqueuedAt":228874811233531,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-375','{"delivery":"current_turn","timing":{"enqueuedAt":228874811815551,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-376','{"delivery":"current_turn","timing":{"enqueuedAt":228874812396549,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-377','{"delivery":"current_turn","timing":{"enqueuedAt":228874812969211,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-378','{"delivery":"current_turn","timing":{"enqueuedAt":228874813560950,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-379','{"delivery":"current_turn","timing":{"enqueuedAt":228874814244300,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-380','{"delivery":"current_turn","timing":{"enqueuedAt":228874814944411,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-381','{"delivery":"current_turn","timing":{"enqueuedAt":228874815592485,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-382','{"delivery":"current_turn","timing":{"enqueuedAt":228874816210292,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-383','{"delivery":"current_turn","timing":{"enqueuedAt":228874816790619,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-384','{"delivery":"current_turn","timing":{"enqueuedAt":228874817372438,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-385','{"delivery":"current_turn","timing":{"enqueuedAt":228874817977121,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-386','{"delivery":"current_turn","timing":{"enqueuedAt":228874818574049,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-387','{"delivery":"current_turn","timing":{"enqueuedAt":228874819156419,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-388','{"delivery":"current_turn","timing":{"enqueuedAt":228874819740984,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-389','{"delivery":"current_turn","timing":{"enqueuedAt":228874820370733,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-390','{"delivery":"current_turn","timing":{"enqueuedAt":228874821227147,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-391','{"delivery":"current_turn","timing":{"enqueuedAt":228874822217492,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-392','{"delivery":"current_turn","timing":{"enqueuedAt":228874823236411,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-393','{"delivery":"current_turn","timing":{"enqueuedAt":228874824249879,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-394','{"delivery":"current_turn","timing":{"enqueuedAt":228874825018488,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-395','{"delivery":"current_turn","timing":{"enqueuedAt":228874825754527,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-396','{"delivery":"current_turn","timing":{"enqueuedAt":228874826455760,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-397','{"delivery":"current_turn","timing":{"enqueuedAt":228874827095608,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-398','{"delivery":"current_turn","timing":{"enqueuedAt":228874827711662,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-399','{"delivery":"current_turn","timing":{"enqueuedAt":228874828326083,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-400','{"delivery":"current_turn","timing":{"enqueuedAt":228874829171596,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-401','{"delivery":"current_turn","timing":{"enqueuedAt":228874829988396,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-402','{"delivery":"current_turn","timing":{"enqueuedAt":228874830968371,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-403','{"delivery":"current_turn","timing":{"enqueuedAt":228874831970598,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-404','{"delivery":"current_turn","timing":{"enqueuedAt":228874832891393,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-405','{"delivery":"current_turn","timing":{"enqueuedAt":228874833686823,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-406','{"delivery":"current_turn","timing":{"enqueuedAt":228874834309669,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-407','{"delivery":"current_turn","timing":{"enqueuedAt":228874835065976,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-408','{"delivery":"current_turn","timing":{"enqueuedAt":228874835702247,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-409','{"delivery":"current_turn","timing":{"enqueuedAt":228874836431383,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-410','{"delivery":"current_turn","timing":{"enqueuedAt":228874837186607,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-411','{"delivery":"current_turn","timing":{"enqueuedAt":228874837803392,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-412','{"delivery":"current_turn","timing":{"enqueuedAt":228874838424516,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-413','{"delivery":"current_turn","timing":{"enqueuedAt":228874839029869,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-414','{"delivery":"current_turn","timing":{"enqueuedAt":228874839632819,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-415','{"delivery":"current_turn","timing":{"enqueuedAt":228874840270583,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-416','{"delivery":"current_turn","timing":{"enqueuedAt":228874840884383,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-417','{"delivery":"current_turn","timing":{"enqueuedAt":228874841522277,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-418','{"delivery":"current_turn","timing":{"enqueuedAt":228874842200297,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-419','{"delivery":"current_turn","timing":{"enqueuedAt":228874842911429,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-420','{"delivery":"current_turn","timing":{"enqueuedAt":228874843564682,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-421','{"delivery":"current_turn","timing":{"enqueuedAt":228874844337420,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-422','{"delivery":"current_turn","timing":{"enqueuedAt":228874845050475,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-423','{"delivery":"current_turn","timing":{"enqueuedAt":228874845667090,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-424','{"delivery":"current_turn","timing":{"enqueuedAt":228874846480594,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-425','{"delivery":"current_turn","timing":{"enqueuedAt":228874847219888,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-426','{"delivery":"current_turn","timing":{"enqueuedAt":228874847874434,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-427','{"delivery":"current_turn","timing":{"enqueuedAt":228874848601836,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-428','{"delivery":"current_turn","timing":{"enqueuedAt":228874849230634,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-429','{"delivery":"current_turn","timing":{"enqueuedAt":228874849826199,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-430','{"delivery":"current_turn","timing":{"enqueuedAt":228874850654240,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-431','{"delivery":"current_turn","timing":{"enqueuedAt":228874851342860,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-432','{"delivery":"current_turn","timing":{"enqueuedAt":228874852054442,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-433','{"delivery":"current_turn","timing":{"enqueuedAt":228874852826078,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-434','{"delivery":"current_turn","timing":{"enqueuedAt":228874853580430,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-435','{"delivery":"current_turn","timing":{"enqueuedAt":228874854336847,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-436','{"delivery":"current_turn","timing":{"enqueuedAt":228874855028843,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-437','{"delivery":"current_turn","timing":{"enqueuedAt":228874855753100,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-438','{"delivery":"current_turn","timing":{"enqueuedAt":228874856418065,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-439','{"delivery":"current_turn","timing":{"enqueuedAt":228874857146720,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-440','{"delivery":"current_turn","timing":{"enqueuedAt":228874857782150,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-441','{"delivery":"current_turn","timing":{"enqueuedAt":228874858535491,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-442','{"delivery":"current_turn","timing":{"enqueuedAt":228874859546624,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-443','{"delivery":"current_turn","timing":{"enqueuedAt":228874860512994,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-444','{"delivery":"current_turn","timing":{"enqueuedAt":228874861536842,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-445','{"delivery":"current_turn","timing":{"enqueuedAt":228874862572752,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-446','{"delivery":"current_turn","timing":{"enqueuedAt":228874863312487,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-447','{"delivery":"current_turn","timing":{"enqueuedAt":228874863944130,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-448','{"delivery":"current_turn","timing":{"enqueuedAt":228874864595480,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-449','{"delivery":"current_turn","timing":{"enqueuedAt":228874865267699,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-450','{"delivery":"current_turn","timing":{"enqueuedAt":228874865918808,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-451','{"delivery":"current_turn","timing":{"enqueuedAt":228874866633216,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-452','{"delivery":"current_turn","timing":{"enqueuedAt":228874867413979,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-453','{"delivery":"current_turn","timing":{"enqueuedAt":228874868180785,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-454','{"delivery":"current_turn","timing":{"enqueuedAt":228874869100627,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-455','{"delivery":"current_turn","timing":{"enqueuedAt":228874869749433,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-456','{"delivery":"current_turn","timing":{"enqueuedAt":228874870433965,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-457','{"delivery":"current_turn","timing":{"enqueuedAt":228874871297512,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-458','{"delivery":"current_turn","timing":{"enqueuedAt":228874871948131,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-459','{"delivery":"current_turn","timing":{"enqueuedAt":228874872581988,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-460','{"delivery":"current_turn","timing":{"enqueuedAt":228874873212659,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-461','{"delivery":"current_turn","timing":{"enqueuedAt":228874873830977,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-462','{"delivery":"current_turn","timing":{"enqueuedAt":228874874467098,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-463','{"delivery":"current_turn","timing":{"enqueuedAt":228874875095335,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-464','{"delivery":"current_turn","timing":{"enqueuedAt":228874875718973,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-465','{"delivery":"current_turn","timing":{"enqueuedAt":228874876335748,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-466','{"delivery":"current_turn","timing":{"enqueuedAt":228874877059944,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-467','{"delivery":"current_turn","timing":{"enqueuedAt":228874877692819,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-468','{"delivery":"current_turn","timing":{"enqueuedAt":228874878317830,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-469','{"delivery":"current_turn","timing":{"enqueuedAt":228874878938482,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-470','{"delivery":"current_turn","timing":{"enqueuedAt":228874879559896,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-471','{"delivery":"current_turn","timing":{"enqueuedAt":228874880194745,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-472','{"delivery":"current_turn","timing":{"enqueuedAt":228874880833922,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-473','{"delivery":"current_turn","timing":{"enqueuedAt":228874881462780,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-474','{"delivery":"current_turn","timing":{"enqueuedAt":228874882096727,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-475','{"delivery":"current_turn","timing":{"enqueuedAt":228874882715957,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-476','{"delivery":"current_turn","timing":{"enqueuedAt":228874883350465,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-477','{"delivery":"current_turn","timing":{"enqueuedAt":228874883970457,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-478','{"delivery":"current_turn","timing":{"enqueuedAt":228874884684354,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-479','{"delivery":"current_turn","timing":{"enqueuedAt":228874885712890,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-480','{"delivery":"current_turn","timing":{"enqueuedAt":228874886852544,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-481','{"delivery":"current_turn","timing":{"enqueuedAt":228874887951372,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-482','{"delivery":"current_turn","timing":{"enqueuedAt":228874889147683,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-483','{"delivery":"current_turn","timing":{"enqueuedAt":228874890326320,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-484','{"delivery":"current_turn","timing":{"enqueuedAt":228874891522410,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-485','{"delivery":"current_turn","timing":{"enqueuedAt":228874892734971,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-486','{"delivery":"current_turn","timing":{"enqueuedAt":228874893951019,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-487','{"delivery":"current_turn","timing":{"enqueuedAt":228874895167086,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-488','{"delivery":"current_turn","timing":{"enqueuedAt":228874896372784,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-489','{"delivery":"current_turn","timing":{"enqueuedAt":228874897585065,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-490','{"delivery":"current_turn","timing":{"enqueuedAt":228874898790923,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-491','{"delivery":"current_turn","timing":{"enqueuedAt":228874899854605,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-492','{"delivery":"current_turn","timing":{"enqueuedAt":228874900839139,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-493','{"delivery":"current_turn","timing":{"enqueuedAt":228874902035790,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-494','{"delivery":"current_turn","timing":{"enqueuedAt":228874903222242,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-495','{"delivery":"current_turn","timing":{"enqueuedAt":228874904357599,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-496','{"delivery":"current_turn","timing":{"enqueuedAt":228874905544962,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-497','{"delivery":"current_turn","timing":{"enqueuedAt":228874906694255,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-498','{"delivery":"current_turn","timing":{"enqueuedAt":228874907901917,"fastSkipReason":"scheduleOverride"}}');
INSERT INTO "runtime_event_meta" VALUES('bound-over','{"delivery":"current_turn","timing":{"enqueuedAt":228874937842150,"fastSkipReason":"scheduleOverride"}}');
CREATE TABLE runtime_events (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                  text TEXT NOT NULL, status TEXT NOT NULL, created REAL NOT NULL,
                  epoch INTEGER NOT NULL, turn_id TEXT, error TEXT);
INSERT INTO "runtime_events" VALUES('input','agent','user','Needle submitted input','delivered',200.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('pending','agent','user','pending receipt needle','pending',500.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('legacy-event','agent','user','needle legacy durable body','delivered',1792689640.5376985,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-0','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-1','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-2','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-3','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-4','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-5','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-6','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-7','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-8','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-9','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-10','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-11','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-12','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-13','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-14','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-15','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-16','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-17','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-18','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-19','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-20','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-21','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-22','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-23','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-24','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-25','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-26','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-27','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-28','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-29','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-30','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-31','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-32','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-33','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-34','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-35','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-36','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-37','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-38','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-39','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-40','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-41','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-42','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-43','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-44','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-45','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-46','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-47','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-48','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-49','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-50','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-51','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-52','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-53','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-54','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-55','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-56','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-57','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-58','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-59','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-60','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-61','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-62','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-63','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-64','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-65','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-66','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-67','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-68','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-69','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-70','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-71','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-72','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-73','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-74','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-75','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-76','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-77','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-78','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-79','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-80','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-81','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-82','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-83','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-84','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-85','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-86','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-87','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-88','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-89','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-90','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-91','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-92','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-93','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-94','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-95','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-96','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-97','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-98','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-99','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-100','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-101','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-102','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-103','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-104','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-105','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-106','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-107','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-108','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-109','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-110','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-111','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-112','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-113','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-114','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-115','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-116','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-117','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-118','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-119','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-120','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-121','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-122','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-123','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-124','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-125','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-126','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-127','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-128','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-129','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-130','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-131','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-132','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-133','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-134','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-135','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-136','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-137','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-138','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-139','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-140','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-141','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-142','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-143','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-144','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-145','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-146','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-147','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-148','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-149','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-150','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-151','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-152','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-153','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-154','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-155','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-156','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-157','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-158','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-159','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-160','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-161','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-162','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-163','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-164','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-165','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-166','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-167','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-168','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-169','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-170','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-171','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-172','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-173','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-174','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-175','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-176','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-177','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-178','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-179','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-180','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-181','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-182','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-183','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-184','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-185','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-186','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-187','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-188','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-189','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-190','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-191','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-192','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-193','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-194','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-195','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-196','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-197','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-198','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-199','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-200','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-201','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-202','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-203','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-204','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-205','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-206','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-207','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-208','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-209','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-210','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-211','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-212','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-213','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-214','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-215','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-216','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-217','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-218','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-219','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-220','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-221','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-222','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-223','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-224','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-225','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-226','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-227','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-228','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-229','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-230','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-231','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-232','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-233','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-234','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-235','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-236','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-237','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-238','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-239','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-240','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-241','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-242','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-243','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-244','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-245','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-246','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-247','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-248','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-249','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-250','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-251','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-252','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-253','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-254','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-255','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-256','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-257','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-258','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-259','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-260','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-261','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-262','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-263','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-264','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-265','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-266','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-267','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-268','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-269','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-270','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-271','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-272','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-273','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-274','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-275','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-276','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-277','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-278','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-279','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-280','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-281','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-282','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-283','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-284','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-285','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-286','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-287','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-288','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-289','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-290','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-291','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-292','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-293','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-294','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-295','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-296','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-297','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-298','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-299','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-300','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-301','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-302','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-303','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-304','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-305','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-306','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-307','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-308','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-309','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-310','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-311','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-312','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-313','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-314','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-315','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-316','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-317','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-318','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-319','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-320','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-321','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-322','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-323','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-324','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-325','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-326','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-327','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-328','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-329','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-330','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-331','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-332','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-333','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-334','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-335','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-336','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-337','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-338','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-339','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-340','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-341','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-342','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-343','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-344','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-345','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-346','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-347','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-348','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-349','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-350','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-351','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-352','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-353','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-354','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-355','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-356','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-357','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-358','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-359','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-360','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-361','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-362','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-363','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-364','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-365','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-366','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-367','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-368','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-369','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-370','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-371','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-372','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-373','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-374','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-375','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-376','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-377','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-378','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-379','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-380','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-381','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-382','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-383','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-384','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-385','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-386','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-387','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-388','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-389','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-390','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-391','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-392','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-393','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-394','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-395','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-396','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-397','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-398','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-399','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-400','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-401','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-402','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-403','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-404','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-405','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-406','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-407','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-408','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-409','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-410','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-411','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-412','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-413','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-414','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-415','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-416','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-417','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-418','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-419','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-420','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-421','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-422','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-423','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-424','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-425','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-426','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-427','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-428','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-429','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-430','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-431','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-432','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-433','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-434','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-435','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-436','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-437','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-438','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-439','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-440','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-441','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-442','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-443','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-444','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-445','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-446','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-447','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-448','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-449','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-450','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-451','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-452','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-453','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-454','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-455','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-456','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-457','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-458','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-459','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-460','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-461','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-462','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-463','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-464','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-465','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-466','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-467','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-468','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-469','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-470','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-471','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-472','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-473','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-474','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-475','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-476','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-477','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-478','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-479','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-480','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-481','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-482','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-483','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-484','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-485','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-486','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-487','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-488','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-489','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-490','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-491','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-492','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-493','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-494','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-495','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-496','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-497','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-498','agent','user','bound receipt','pending',900.0,1,NULL,NULL);
INSERT INTO "runtime_events" VALUES('bound-over','agent','user','bound receipt','pending',1792689640.9669857,1,NULL,NULL);
CREATE TABLE runtime_execution_attempts (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_execution_effects (id TEXT PRIMARY KEY, run TEXT NOT NULL, kind TEXT NOT NULL, reference TEXT NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_effects" VALUES('spawn:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','spawn','child','{"id": "spawn:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "spawn", "referenceId": "child", "attemptId": null, "status": "running"}');
CREATE TABLE runtime_execution_inputs (run TEXT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(run,event));
CREATE TABLE runtime_execution_nodes (id TEXT PRIMARY KEY, run TEXT NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_nodes" VALUES('worker:child','run:543b6d63-ba17-5996-aa5b-7bc7c6764369','{"id": "worker:child", "runId": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "kind": "managed_worker", "agentId": "child", "status": "running"}');
CREATE TABLE runtime_execution_runs (id TEXT PRIMARY KEY, agent TEXT NOT NULL, account TEXT NOT NULL, epoch INTEGER NOT NULL, thread TEXT, turn TEXT, created REAL NOT NULL, record TEXT NOT NULL);
INSERT INTO "runtime_execution_runs" VALUES('run:543b6d63-ba17-5996-aa5b-7bc7c6764369','agent','default',1,'thread','turn',1792689637.9987645,'{"id": "run:543b6d63-ba17-5996-aa5b-7bc7c6764369", "agent": "agent", "accountKey": "default", "epoch": 1, "threadId": "thread", "turnId": "turn", "created": 1792689637.9987645, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO "runtime_execution_runs" VALUES('run:48f065a1-8be0-543a-9b6f-69bc8581573d','other','default',1,'other-thread','turn',1792689637.9999597,'{"id": "run:48f065a1-8be0-543a-9b6f-69bc8581573d", "agent": "other", "accountKey": "default", "epoch": 1, "threadId": "other-thread", "turnId": "turn", "created": 1792689637.9999597, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO "runtime_execution_runs" VALUES('run:24b2429b-e38b-5348-a024-3759e25043e1','child','default',1,'child-thread','turn',1792689638.0005055,'{"id": "run:24b2429b-e38b-5348-a024-3759e25043e1", "agent": "child", "accountKey": "default", "epoch": 1, "threadId": "child-thread", "turnId": "turn", "created": 1792689638.0005054, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
INSERT INTO "runtime_execution_runs" VALUES('run:f87743b1-c05c-578e-abca-80c319c6f7f4','gone','default',1,'thread','turn',1792689640.534606,'{"id": "run:f87743b1-c05c-578e-abca-80c319c6f7f4", "agent": "gone", "accountKey": "default", "epoch": 1, "threadId": "thread", "turnId": "turn", "created": 1792689640.534606, "status": "running", "firstAttemptId": null, "rootAttemptId": null, "latestAttemptId": null, "reconciled": true}');
CREATE TABLE runtime_federation_identity (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_inbox (
          peer_id TEXT NOT NULL, message_id TEXT NOT NULL, record TEXT NOT NULL,
          PRIMARY KEY(peer_id,message_id));
CREATE TABLE runtime_federation_invites (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_nonces (
          peer_id TEXT NOT NULL, nonce TEXT NOT NULL, created INTEGER NOT NULL,
          PRIMARY KEY(peer_id,nonce));
CREATE TABLE runtime_federation_outbox (
          id TEXT PRIMARY KEY, peer_id TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_federation_peers (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_federation_rooms (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_federation_rooms" VALUES('approved','{"id": "approved", "kind": "federated", "status": "approved", "localApproved": true, "remoteApproved": true, "localMembers": ["agent"], "remoteMembers": [], "peerId": "remote", "peerLabel": "Remote", "name": "Federated", "shareNames": false, "shareStatus": false, "created": 800.0, "updated": 800.0}');
INSERT INTO "runtime_federation_rooms" VALUES('unapproved','{"id": "unapproved", "kind": "federated", "status": "pending", "localApproved": true, "remoteApproved": false, "localMembers": ["agent"], "remoteMembers": [], "peerId": "remote", "peerLabel": "Remote", "name": "Federated", "shareNames": false, "shareStatus": false, "created": 800.0, "updated": 800.0}');
CREATE TABLE runtime_federation_settings (
          id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_item_fulltext (id TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE runtime_items (
                  id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL, created REAL NOT NULL);
INSERT INTO "runtime_items" VALUES('agent:old-1','agent','{"id": "agent:old-1", "role": "assistant", "title": "Agent", "text": "needle history 1", "truncated": false, "at": 101.0, "turnId": "old-turn"}',101.0);
INSERT INTO "runtime_items" VALUES('agent:old-2','agent','{"id": "agent:old-2", "role": "assistant", "title": "Agent", "text": "needle history 2", "truncated": false, "at": 102.0, "turnId": "old-turn"}',102.0);
INSERT INTO "runtime_items" VALUES('agent:old-3','agent','{"id": "agent:old-3", "role": "assistant", "title": "Agent", "text": "needle history 3", "truncated": false, "at": 103.0, "turnId": "old-turn"}',103.0);
INSERT INTO "runtime_items" VALUES('agent:old-4','agent','{"id": "agent:old-4", "role": "assistant", "title": "Agent", "text": "needle history 4", "truncated": false, "at": 104.0, "turnId": "old-turn"}',104.0);
INSERT INTO "runtime_items" VALUES('agent:old-5','agent','{"id": "agent:old-5", "role": "assistant", "title": "Agent", "text": "needle history 5", "truncated": false, "at": 105.0, "turnId": "old-turn"}',105.0);
INSERT INTO "runtime_items" VALUES('agent:old-6','agent','{"id": "agent:old-6", "role": "assistant", "title": "Agent", "text": "needle history 6", "truncated": false, "at": 106.0, "turnId": "old-turn"}',106.0);
INSERT INTO "runtime_items" VALUES('agent:old-7','agent','{"id": "agent:old-7", "role": "assistant", "title": "Agent", "text": "needle history 7", "truncated": false, "at": 107.0, "turnId": "old-turn"}',107.0);
INSERT INTO "runtime_items" VALUES('other:other','other','{"id": "other:other", "role": "assistant", "title": "Agent", "text": "needle foreign", "truncated": false, "at": 1792689638.0096738}',1792689638.0096784);
INSERT INTO "runtime_items" VALUES('agent:input','agent','{"id": "agent:input", "role": "user", "title": "You", "text": "Needle submitted input", "truncated": false, "at": 200.0, "turnId": "turn", "inputs": [{"id": "input", "at": 200.0, "kind": "user", "text": "Needle submitted input", "truncated": false, "assets": []}]}',200.0);
INSERT INTO "runtime_items" VALUES('agent:answer','agent','{"id": "agent:answer", "role": "assistant", "title": "Assistant", "text": "Needle updated answer", "truncated": false, "at": 300.0, "streaming": true, "turnId": "turn", "phase": null}',300.0);
INSERT INTO "runtime_items" VALUES('agent:command','agent','{"id": "agent:command", "role": "output", "title": "commandExecution", "text": "{\"id\": \"command\", \"type\": \"commandExecution\", \"exitCode\": 0, \"aggregatedOutput\": \"needle tool output\", \"outputTruncated\": false, \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300.0);
INSERT INTO "runtime_items" VALUES('agent:compact','agent','{"id": "agent:compact", "role": "output", "title": "contextCompaction", "text": "{\"id\": \"compact\", \"type\": \"contextCompaction\", \"durationMs\": 0}", "truncated": false, "at": 300.0, "toolStatus": "completed", "turnId": "turn"}',300.0);
INSERT INTO "runtime_items" VALUES('agent:dynamic','agent','{"id": "agent:dynamic", "role": "tool", "title": "dynamicToolCall", "text": "{\"id\": \"call\", \"type\": \"dynamicToolCall\", \"status\": \"inProgress\", \"contentItems\": null}", "truncated": false, "at": 600.0, "turnId": "turn", "toolStatus": "running"}',600.0);
INSERT INTO "runtime_items" VALUES('agent:legacy','agent','{"id":"agent:legacy","role":"user","title":"You","truncated":false,"at":850.0,"inputs":[{"id":"legacy-event","at":null,"kind":"user","text":"short","truncated":false,"assets":[]}]}',1792689640.5387073);
INSERT INTO "runtime_items" VALUES('agent:concurrent','agent','{"id": "agent:concurrent", "role": "assistant", "title": "Agent", "text": "concurrent append", "truncated": false, "at": 150.0}',150.0);
CREATE TABLE runtime_lead_requests (id TEXT PRIMARY KEY, agent TEXT NOT NULL, signature TEXT NOT NULL);
CREATE TABLE runtime_monitors (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_native_sweeps (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_operation_receipts (id TEXT PRIMARY KEY, signature TEXT NOT NULL, result TEXT NOT NULL);
CREATE TABLE runtime_payload_refs (
            table_name TEXT NOT NULL, record_id TEXT NOT NULL, field TEXT NOT NULL,
            sha256 TEXT NOT NULL, PRIMARY KEY(table_name,record_id,field));
CREATE TABLE runtime_plans (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_plans" VALUES('agent','{"id": "agent", "agent": "agent", "rootId": "agent", "text": "needle plan", "version": 1, "updated": 1.0}');
CREATE TABLE runtime_profiles (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_projects (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_projects" VALUES('@FIXTURE_ROOT@','{"id": "@FIXTURE_ROOT@", "path": "@FIXTURE_ROOT@", "name": "fixture", "peerTeams": [{"id": "peers", "name": "Peers", "members": ["agent", "other"]}], "homeServerId": "fixture-server", "locations": [{"serverId": "fixture-server", "path": "@FIXTURE_ROOT@", "projectId": "@FIXTURE_ROOT@"}], "locationsRevision": 0}');
CREATE TABLE runtime_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_rooms (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_rooms" VALUES('private','{"id": "private", "kind": "private", "rootId": "agent", "members": ["agent", "child"], "updated": 1.0}');
INSERT INTO "runtime_rooms" VALUES('peer','{"id": "peer", "kind": "private", "rootId": "agent", "members": ["agent", "other"], "updated": 1.0}');
INSERT INTO "runtime_rooms" VALUES('unavailable','{"id": "unavailable", "kind": "private", "rootId": "agent", "members": ["agent", "gone"], "updated": 1.0}');
INSERT INTO "runtime_rooms" VALUES('approved','{"id": "approved", "kind": "federated", "rootId": "agent", "members": ["agent"], "updated": 1.0}');
INSERT INTO "runtime_rooms" VALUES('unapproved','{"id": "unapproved", "kind": "federated", "rootId": "agent", "members": ["agent"], "updated": 1.0}');
CREATE TABLE runtime_rules (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_scheduler_agent_change_state (id INTEGER PRIMARY KEY CHECK(id=1),floor INTEGER NOT NULL,generation INTEGER NOT NULL,entries INTEGER NOT NULL,total INTEGER NOT NULL);
INSERT INTO "runtime_scheduler_agent_change_state" VALUES(1,0,14,4,32);
CREATE TABLE runtime_scheduler_agent_changes (id TEXT PRIMARY KEY,generation INTEGER NOT NULL CHECK(typeof(generation)='integer' AND generation>=0));
INSERT INTO "runtime_scheduler_agent_changes" VALUES('agent',13);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('other',2);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('child',3);
INSERT INTO "runtime_scheduler_agent_changes" VALUES('gone',14);
PRAGMA writable_schema=ON;
INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search','runtime_search',0,'CREATE VIRTUAL TABLE runtime_search USING fts5(id UNINDEXED,agent UNINDEXED,kind UNINDEXED,body,tokenize=''unicode61'')');
CREATE TABLE 'runtime_search_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO "runtime_search_config" VALUES('version',4);
CREATE TABLE 'runtime_search_content'(id INTEGER PRIMARY KEY, c0, c1, c2, c3);
INSERT INTO "runtime_search_content" VALUES(1,'agent:old-0','agent','Agent','needle history 0');
INSERT INTO "runtime_search_content" VALUES(2,'agent:old-1','agent','Agent','needle history 1');
INSERT INTO "runtime_search_content" VALUES(3,'agent:old-2','agent','Agent','needle history 2');
INSERT INTO "runtime_search_content" VALUES(4,'agent:old-3','agent','Agent','needle history 3');
INSERT INTO "runtime_search_content" VALUES(5,'agent:old-4','agent','Agent','needle history 4');
INSERT INTO "runtime_search_content" VALUES(6,'agent:old-5','agent','Agent','needle history 5');
INSERT INTO "runtime_search_content" VALUES(7,'agent:old-6','agent','Agent','needle history 6');
INSERT INTO "runtime_search_content" VALUES(8,'agent:old-7','agent','Agent','needle history 7');
INSERT INTO "runtime_search_content" VALUES(9,'other:other','other','Agent','needle foreign');
INSERT INTO "runtime_search_content" VALUES(10,'agent:input','agent','You','Needle submitted input');
INSERT INTO "runtime_search_content" VALUES(12,'agent:command','agent','commandExecution','{"id": "command", "type": "commandExecution", "exitCode": 0, "aggregatedOutput": "needle tool output", "outputTruncated": false, "durationMs": 0}');
INSERT INTO "runtime_search_content" VALUES(13,'agent:compact','agent','contextCompaction','{"id": "compact", "type": "contextCompaction", "durationMs": 0}');
INSERT INTO "runtime_search_content" VALUES(14,'agent:dynamic','agent','dynamicToolCall','{"id": "call", "type": "dynamicToolCall", "status": "inProgress", "contentItems": null}');
INSERT INTO "runtime_search_content" VALUES(15,'agent:legacy','agent','You','needle legacy durable body');
INSERT INTO "runtime_search_content" VALUES(16,'agent:concurrent','agent','Agent','concurrent append');
CREATE TABLE 'runtime_search_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO "runtime_search_data" VALUES(1,X'0F0000003F');
INSERT INTO "runtime_search_data" VALUES(10,X'0000000002021100021101010101010000');
INSERT INTO "runtime_search_data" VALUES(137438953473,X'000000220730617070656E641006010303010A636F6E63757272656E741006010302040D');
INSERT INTO "runtime_search_data" VALUES(2336462209025,X'0000023502303001060103040B080103070A010601030701013102060103040101320306010304010133040601030401013405060103040101350606010304010136070601030401013708060103040110616767726567617465646F75747075740C060103080104626F64790F06010305010463616C6C0E0601030302066F6D6D616E640C060103030809657865637574696F6E0C060103050404706163740D06010303030A6E74656E746974656D730E06010308060C7874636F6D70616374696F6E0D06010305010764757261626C650F06010304050674696F6E6D730C0601030E0106010306020E796E616D6963746F6F6C63616C6C0E06010305010865786974636F64650C06010306010566616C73650C0601030D02066F726569676E09060103030107686973746F727901060103030106010303010601030301060103030106010303010601030301060103030106010303010269640C060103020106010302010601030202096E70726F67726573730E06010307040275740A0601030401066C65676163790F0601030301066E6565646C650106010302010601030201060103020106010302010601030201060103020106010302010601030201060103020106010302020601030903060103020203756C6C0E0601030901066F75747075740C0601030B07097472756E63617465640C0601030C01067374617475730E06010306020875626D69747465640A060103030104746F6F6C0C0601030A02037970650C0601030401060103040106010304041308080808080808170B0B0D100B11130E12150F0C0D311310090D440A0D100D0F0B');
CREATE TABLE 'runtime_search_docsize'(id INTEGER PRIMARY KEY, sz BLOB);
INSERT INTO "runtime_search_docsize" VALUES(1,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(2,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(3,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(4,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(5,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(6,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(7,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(8,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(9,X'00000002');
INSERT INTO "runtime_search_docsize" VALUES(10,X'00000003');
INSERT INTO "runtime_search_docsize" VALUES(12,X'0000000E');
INSERT INTO "runtime_search_docsize" VALUES(13,X'00000006');
INSERT INTO "runtime_search_docsize" VALUES(14,X'00000008');
INSERT INTO "runtime_search_docsize" VALUES(15,X'00000004');
INSERT INTO "runtime_search_docsize" VALUES(16,X'00000002');
CREATE TABLE 'runtime_search_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO "runtime_search_idx" VALUES(1,X'',2);
INSERT INTO "runtime_search_idx" VALUES(17,X'',2);
CREATE TABLE runtime_search_indexed (id TEXT PRIMARY KEY);
INSERT INTO "runtime_search_indexed" VALUES('agent:old-0');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-1');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-2');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-3');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-4');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-5');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-6');
INSERT INTO "runtime_search_indexed" VALUES('agent:old-7');
INSERT INTO "runtime_search_indexed" VALUES('other:other');
INSERT INTO "runtime_search_indexed" VALUES('agent:input');
INSERT INTO "runtime_search_indexed" VALUES('agent:command');
INSERT INTO "runtime_search_indexed" VALUES('agent:compact');
INSERT INTO "runtime_search_indexed" VALUES('agent:dynamic');
INSERT INTO "runtime_search_indexed" VALUES('agent:legacy');
INSERT INTO "runtime_search_indexed" VALUES('agent:concurrent');
INSERT INTO sqlite_master(type,name,tbl_name,rootpage,sql)VALUES('table','runtime_search_next','runtime_search_next',0,'CREATE VIRTUAL TABLE runtime_search_next USING fts5(body,content='''',contentless_delete=1,tokenize=''unicode61'')');
CREATE TABLE 'runtime_search_next_config'(k PRIMARY KEY, v) WITHOUT ROWID;
INSERT INTO "runtime_search_next_config" VALUES('version',4);
CREATE TABLE 'runtime_search_next_data'(id INTEGER PRIMARY KEY, block BLOB);
INSERT INTO "runtime_search_next_data" VALUES(1,X'0F3F');
INSERT INTO "runtime_search_next_data" VALUES(10,X'00000000FF00000102011200000001110101011000000E');
INSERT INTO "runtime_search_next_data" VALUES(2336462209025,X'000001D30230300102040B04070A0102070101310202040101320302040101330402040101340502040101350602040101360702040101370802040110616767726567617465646F75747075740C020802057070656E641002030104626F64790F0205010463616C6C0E020302066F6D6D616E640C02030809657865637574696F6E0C02050404706163740D020303086E63757272656E74100202040974656E746974656D730E0208060C7874636F6D70616374696F6E0D0205010764757261626C650F0204050674696F6E6D730C020E010206020E796E616D6963746F6F6C63616C6C0E0205010865786974636F64650C0206010566616C73650C020D02066F726569676E0902030107686973746F7279010203010203010203010203010203010203010203010203010269640C020201020201020202096E70726F67726573730E0207040275740A020401066C65676163790F020301066E6565646C650102020102020102020102020102020102020102020102020102020102020202090302020203756C6C0E020901066F75747075740C020B07097472756E63617465640C020C01067374617475730E0206020875626D69747465640A02030104746F6F6C0C020A02037970650C0204010204010204040D06060606060606150A09090B0E090D0E110C0E130D0A0B210D0E070B2C080B0E0B0D09');
CREATE TABLE 'runtime_search_next_docsize'(id INTEGER PRIMARY KEY, sz BLOB, origin INTEGER);
INSERT INTO "runtime_search_next_docsize" VALUES(1,X'03',1);
INSERT INTO "runtime_search_next_docsize" VALUES(2,X'03',2);
INSERT INTO "runtime_search_next_docsize" VALUES(3,X'03',3);
INSERT INTO "runtime_search_next_docsize" VALUES(4,X'03',4);
INSERT INTO "runtime_search_next_docsize" VALUES(5,X'03',5);
INSERT INTO "runtime_search_next_docsize" VALUES(6,X'03',6);
INSERT INTO "runtime_search_next_docsize" VALUES(7,X'03',7);
INSERT INTO "runtime_search_next_docsize" VALUES(8,X'03',8);
INSERT INTO "runtime_search_next_docsize" VALUES(9,X'02',9);
INSERT INTO "runtime_search_next_docsize" VALUES(10,X'03',10);
INSERT INTO "runtime_search_next_docsize" VALUES(12,X'0E',12);
INSERT INTO "runtime_search_next_docsize" VALUES(13,X'06',13);
INSERT INTO "runtime_search_next_docsize" VALUES(14,X'08',14);
INSERT INTO "runtime_search_next_docsize" VALUES(15,X'04',15);
INSERT INTO "runtime_search_next_docsize" VALUES(16,X'02',16);
CREATE TABLE 'runtime_search_next_idx'(segid, term, pgno, PRIMARY KEY(segid, term)) WITHOUT ROWID;
INSERT INTO "runtime_search_next_idx" VALUES(17,X'',2);
CREATE TABLE runtime_search_next_meta (
                id TEXT PRIMARY KEY, agent TEXT NOT NULL, kind TEXT NOT NULL,
                search_rowid INTEGER NOT NULL UNIQUE);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-0','agent','Agent',1);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-1','agent','Agent',2);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-2','agent','Agent',3);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-3','agent','Agent',4);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-4','agent','Agent',5);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-5','agent','Agent',6);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-6','agent','Agent',7);
INSERT INTO "runtime_search_next_meta" VALUES('agent:old-7','agent','Agent',8);
INSERT INTO "runtime_search_next_meta" VALUES('other:other','other','Agent',9);
INSERT INTO "runtime_search_next_meta" VALUES('agent:input','agent','You',10);
INSERT INTO "runtime_search_next_meta" VALUES('agent:command','agent','commandExecution',12);
INSERT INTO "runtime_search_next_meta" VALUES('agent:compact','agent','contextCompaction',13);
INSERT INTO "runtime_search_next_meta" VALUES('agent:dynamic','agent','dynamicToolCall',14);
INSERT INTO "runtime_search_next_meta" VALUES('agent:legacy','agent','You',15);
INSERT INTO "runtime_search_next_meta" VALUES('agent:concurrent','agent','Agent',16);
CREATE TABLE runtime_search_rollout (
                id INTEGER PRIMARY KEY CHECK(id=1), phase TEXT NOT NULL,
                cursor INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL);
INSERT INTO "runtime_search_rollout" VALUES(1,'building',0,1792689637.8706896);
CREATE TABLE runtime_search_rows (id TEXT PRIMARY KEY, search_rowid INTEGER NOT NULL UNIQUE);
INSERT INTO "runtime_search_rows" VALUES('agent:old-0',1);
INSERT INTO "runtime_search_rows" VALUES('agent:old-1',2);
INSERT INTO "runtime_search_rows" VALUES('agent:old-2',3);
INSERT INTO "runtime_search_rows" VALUES('agent:old-3',4);
INSERT INTO "runtime_search_rows" VALUES('agent:old-4',5);
INSERT INTO "runtime_search_rows" VALUES('agent:old-5',6);
INSERT INTO "runtime_search_rows" VALUES('agent:old-6',7);
INSERT INTO "runtime_search_rows" VALUES('agent:old-7',8);
INSERT INTO "runtime_search_rows" VALUES('other:other',9);
INSERT INTO "runtime_search_rows" VALUES('agent:input',10);
INSERT INTO "runtime_search_rows" VALUES('agent:command',12);
INSERT INTO "runtime_search_rows" VALUES('agent:compact',13);
INSERT INTO "runtime_search_rows" VALUES('agent:dynamic',14);
INSERT INTO "runtime_search_rows" VALUES('agent:legacy',15);
INSERT INTO "runtime_search_rows" VALUES('agent:concurrent',16);
CREATE TABLE runtime_search_rows_rollout (id INTEGER PRIMARY KEY CHECK(id=1),cursor INTEGER NOT NULL DEFAULT 0);
INSERT INTO "runtime_search_rows_rollout" VALUES(1,0);
CREATE TABLE runtime_server_fetch (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, signature TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT);
CREATE TABLE runtime_server_inbox (
                    id TEXT PRIMARY KEY, signature TEXT NOT NULL, state TEXT NOT NULL,
                    result TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, principal TEXT, actor TEXT);
CREATE TABLE runtime_server_links (
                    id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_server_outbox (
                    id TEXT PRIMARY KEY, server TEXT NOT NULL, body TEXT NOT NULL,
                    state TEXT NOT NULL, result TEXT, attempts INTEGER NOT NULL DEFAULT 0,
                    next_at REAL NOT NULL DEFAULT 0, error TEXT, created_at REAL NOT NULL DEFAULT 0, completed_at REAL NOT NULL DEFAULT 0, fingerprint TEXT);
CREATE TABLE runtime_server_output_tools (
                    id TEXT PRIMARY KEY, expires REAL NOT NULL);
CREATE TABLE runtime_server_retired (
                    id TEXT NOT NULL, direction TEXT NOT NULL, signature TEXT NOT NULL,
                    server TEXT, actor TEXT, PRIMARY KEY(id,direction));
CREATE TABLE runtime_server_sequence (
                    id INTEGER PRIMARY KEY CHECK(id=1), value INTEGER NOT NULL);
INSERT INTO "runtime_server_sequence" VALUES(1,0);
CREATE TABLE runtime_sidebar_order (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_supervisor_cursor (handle TEXT PRIMARY KEY, sequence INTEGER NOT NULL, epoch TEXT);
CREATE TABLE runtime_tasks (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_tasks" VALUES('agent:command','{"id": "agent:command", "agent": "agent", "itemId": "command", "turnId": "turn", "created": 300.0, "kind": "command", "ownerEpoch": 1, "sourceAccountKey": "default", "sourceConnectionId": "connection-7", "sourceThreadId": "thread", "sourceProvider": "codex", "type": "commandExecution", "name": "commandExecution", "status": "completed", "command": "printf needle", "processId": "42", "tail": "needle tool output", "outputTruncated": false, "exitCode": 0, "durationMs": 0, "finished": 300.0}');
INSERT INTO "runtime_tasks" VALUES('agent:compact','{"id": "agent:compact", "agent": "agent", "itemId": "compact", "turnId": "turn", "created": 300.0, "kind": "tool", "ownerEpoch": 1, "sourceAccountKey": "default", "sourceConnectionId": "connection-7", "sourceThreadId": "thread", "sourceProvider": "codex", "type": "contextCompaction", "name": "contextCompaction", "status": "completed", "durationMs": 0, "finished": 300.0}');
CREATE TABLE runtime_tool_request_aliases
                (agent TEXT NOT NULL, alias TEXT NOT NULL, request TEXT NOT NULL, PRIMARY KEY(agent,alias,request));
INSERT INTO "runtime_tool_request_aliases" VALUES('agent','call','thread:call');
CREATE TABLE runtime_tool_requests (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE runtime_tool_results (id TEXT PRIMARY KEY, result TEXT NOT NULL);
INSERT INTO "runtime_tool_results" VALUES('thread:call','{"success": true, "contentItems": [{"type": "inputText", "text": "needle durable tool receipt"}]}');
CREATE TABLE runtime_transcript_revisions(
                        agent TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0);
INSERT INTO "runtime_transcript_revisions" VALUES('agent',46);
INSERT INTO "runtime_transcript_revisions" VALUES('other',2);
INSERT INTO "runtime_transcript_revisions" VALUES('child',1);
INSERT INTO "runtime_transcript_revisions" VALUES('gone',1);
CREATE TABLE runtime_turn_item_link_state_v1 (id INTEGER PRIMARY KEY CHECK(id=1),epoch TEXT NOT NULL,source_rootpage INTEGER NOT NULL,cursor INTEGER,target INTEGER,complete INTEGER NOT NULL CHECK(complete IN (0,1)),scanned_rows INTEGER NOT NULL,source_bytes INTEGER NOT NULL,updated REAL NOT NULL);
INSERT INTO "runtime_turn_item_link_state_v1" VALUES(1,'814eff3c807b4a799cc3f63d64d7d0ce',73,NULL,NULL,1,0,0,1792689637.776191);
CREATE TABLE runtime_turn_item_links_v1 (id TEXT PRIMARY KEY,agent TEXT NOT NULL,turn_id,created REAL NOT NULL,epoch TEXT NOT NULL);
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-1','agent','old-turn',101.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-2','agent','old-turn',102.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-3','agent','old-turn',103.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-4','agent','old-turn',104.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-5','agent','old-turn',105.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-6','agent','old-turn',106.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:old-7','agent','old-turn',107.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('other:other','other',NULL,1792689638.0096784,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:input','agent','turn',200.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:answer','agent','turn',300.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:command','agent','turn',300.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:compact','agent','turn',300.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:dynamic','agent','turn',600.0,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:legacy','agent',NULL,1792689640.5387073,'814eff3c807b4a799cc3f63d64d7d0ce');
INSERT INTO "runtime_turn_item_links_v1" VALUES('agent:concurrent','agent',NULL,150.0,'814eff3c807b4a799cc3f63d64d7d0ce');
CREATE TABLE runtime_usage_resumes (id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
CREATE TABLE runtime_work (id TEXT PRIMARY KEY, record TEXT NOT NULL);
INSERT INTO "runtime_work" VALUES('work','{"id": "work", "rootId": "agent", "title": "needle work", "status": "ready"}');
INSERT INTO "runtime_work" VALUES('child-work','{"id": "child-work", "rootId": "child", "title": "needle child root", "status": "ready"}');
CREATE TABLE runtime_workspace_operations (id TEXT PRIMARY KEY, record TEXT NOT NULL);
CREATE TABLE sync_documents (
        seq INTEGER PRIMARY KEY AUTOINCREMENT, scope TEXT NOT NULL,
        id TEXT NOT NULL, payload TEXT NOT NULL, deleted INTEGER NOT NULL DEFAULT 0,
        UNIQUE(scope,id));
CREATE TABLE sync_entities (
        collection TEXT NOT NULL, id TEXT NOT NULL, seq INTEGER NOT NULL,
        hash TEXT NOT NULL, payload TEXT, deleted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY(collection,id));
INSERT INTO "sync_entities" VALUES('workspace','current',1,'5f0bc1ead2a9c2e010e53e0399e775e92eecc591bba396f2a69721886a312c65','{"collection":"workspace","id":"current","value":{"connected":false,"nativeNotices":[],"peerTeamsVersion":1,"projectOrganizationVersion":1,"rateLimits":{"accountKey":"default","at":null,"data":null,"error":null},"rateLimitsByAccount":{},"sidebarOrder":{"groups":null,"revision":0},"stateDir":"@FIXTURE_ROOT@/python","tasksHistoryLimit":100}}',0);
INSERT INTO "sync_entities" VALUES('agent','agent',561,'3293609b66441d3efda248080a266969b05b3aa0f7437f67f18a2b9f12836cc5','{"collection":"agent","id":"agent","value":{"accountKey":"default","activity":{"at":300.0,"phase":"writing"},"agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"compactions":1,"concurrency":32,"contextUsage":null,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"agent","inFlight":true,"isLead":true,"kind":"agent","lastEvent":"2026-10-22T17:20:38Z","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"approval","subagentConcurrencyVersion":2,"tail":"Needle streamed answer","threadId":"thread","turnId":"turn","workspaceMode":"image"}}',0);
INSERT INTO "sync_entities" VALUES('agent','other',3,'b96aaa3e57c6560b3dc97745e2a439b5028a3a1911b0a5b51a0201aee563c350','{"collection":"agent","id":"other","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","empty":false,"epoch":1,"id":"other","inFlight":true,"isLead":true,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"readStateSupported":true,"rootId":"other","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"other-thread","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('agent','child',4,'9e9c192a7b84f0298b5ba5436c9928cec6da36e65a008ebe3c94543ce9f2f1fd','{"collection":"agent","id":"child","value":{"accountKey":"default","agentMode":"multi","agentModeRevision":0,"agentModeSupported":true,"autoWake":true,"canSend":true,"concurrency":32,"cwd":"@FIXTURE_ROOT@","epoch":1,"id":"child","inFlight":true,"isLead":false,"kind":"agent","launcherAlive":true,"name":"Transcript fixture","nextTurnSettingsSupported":true,"overview":{"result":"","resultFile":null,"resultTruncated":false,"resultTurnId":null,"task":"","taskTruncated":false},"parentId":"agent","readStateSupported":true,"rootId":"agent","serverId":"fixture-server","source":"managed","status":"running","subagentConcurrencyVersion":2,"threadId":"child-thread","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('event','input',965,'1d2577f88b647937ad48b049744a42f45a6c36f0bde9616b516fe1d8c55ff923','{"collection":"event","id":"input","value":{}}',1);
INSERT INTO "sync_entities" VALUES('room','private',7,'53df58b94e00bc7d1706ac8d79cae6fecdbce6e7f494160687490829200aa899','{"collection":"room","id":"private","value":{"id":"private","kind":"private","lastMessage":null,"members":["agent","child"],"name":"Transcript fixture ↔ Transcript fixture","rootId":"agent","updated":1.0}}',0);
INSERT INTO "sync_entities" VALUES('task','agent:command',12,'2af7a8c98cc5344143fa75ebea6446595058e8a29e9cd224fd9f23aada6ff6a3','{"collection":"task","id":"agent:command","value":{"agent":"agent","command":"printf needle","created":300.0,"durationMs":0,"exitCode":0,"finished":300.0,"id":"agent:command","kind":"command","name":"commandExecution","outputTruncated":false,"processId":"42","status":"completed","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('task','agent:compact',14,'aafd76c2984f447747e28ce0d00ca6c04a0704bb86cfcddcc757ec57ec637f30','{"collection":"task","id":"agent:compact","value":{"agent":"agent","created":300.0,"durationMs":0,"finished":300.0,"id":"agent:compact","kind":"tool","name":"contextCompaction","status":"completed","turnId":"turn"}}',0);
INSERT INTO "sync_entities" VALUES('event','pending',968,'37c06cb04d353f0823b57105a7b5485954d609b64333a2833b6e1c9ae79afaf8','{"collection":"event","id":"pending","value":{}}',1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-1',29,'d7691a12cf5a07d416f04bad6e765e8a9726d49000c79aab7c483d81256f46d6',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-2',30,'c6f47674d75b4a9be1c2234aeb869c7300bf7ea8d24af5679a2d47a50feadef2',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-3',31,'13a8de652b968460e7ee53d1bd87a3c9b6e8e8d17a99feeb3c84c4c92675dc6f',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-4',32,'c0c268158ed5bbc9652588fb95795c56bc476faa0e54e8e1a4c890058bd156b1',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-5',33,'6a03dc6fa2c5f8b554893687e15bb68f459854ec47db888dcb304fbbaaac0e7f',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-6',34,'a0ab283be23e339f7b118c7b9834b23e6ccbec768576a1fae535c24d05ccc9e1',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:old-7',35,'78ab4a25d68ea456e7802cb6d8f6fa7e9add2647da7b6a8402359d9c1b4984e0',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:input',36,'a017bf6c56f7d24d197ca72e5217e9f657223699d313f6fdf13b2ef2ef6bdfce',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:answer',44,'e9e5be0bdc89f23fd49fc80687728696f07559b94e992cb11fbd6416925606f0',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:command',38,'0104d41b94f4c9e0fcdcb71623b0413d7158539c887a4e328a5a069de44c838f',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:compact',39,'383fa2e16eb15f5801958fb85e6e9bd57a9e463806c601d818b834f9253c2541',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:agent:pending',40,'55465de0e6f5ee108b1173ea590f1b600647b1f0c932b55a1f9ab16ec8ecc634',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@order',46,'ffa8eda309acc4023f59146efb5a0e02da71bbae602d1cd17f7b26f888c802d5',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@meta',42,'2dff93b54c1092bc22009f56070df43e2c74db861e6097b42f8d15bd35907cb0',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@revision',47,'7a61b53701befdae0eeeffaecc73f14e20b537bb0f8b91ad7c2936dc63562b25',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','@floor',48,'98010bd9270f9b100b6214a21754fd33bdc8d41b2bc9f9dd16ff54d3c34ffd71',NULL,0);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-1',49,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-2',50,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-3',51,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-4',52,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-5',53,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-6',54,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-7',55,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-8',56,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-9',57,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-10',58,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-11',59,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-12',60,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-13',61,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-14',62,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-15',63,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-16',64,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-17',65,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-18',66,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-19',67,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-20',68,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-21',69,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-22',70,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-23',71,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-24',72,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-25',73,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-26',74,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-27',75,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-28',76,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-29',77,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-30',78,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-31',79,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-32',80,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-33',81,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-34',82,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-35',83,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-36',84,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-37',85,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-38',86,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-39',87,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-40',88,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-41',89,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-42',90,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-43',91,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-44',92,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-45',93,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-46',94,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-47',95,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-48',96,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-49',97,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-50',98,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-51',99,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-52',100,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-53',101,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-54',102,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-55',103,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-56',104,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-57',105,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-58',106,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-59',107,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-60',108,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-61',109,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-62',110,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-63',111,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-64',112,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-65',113,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-66',114,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-67',115,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-68',116,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-69',117,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-70',118,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-71',119,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-72',120,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-73',121,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-74',122,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-75',123,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-76',124,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-77',125,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-78',126,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-79',127,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-80',128,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-81',129,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-82',130,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-83',131,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-84',132,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-85',133,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-86',134,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-87',135,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-88',136,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-89',137,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-90',138,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-91',139,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-92',140,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-93',141,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-94',142,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-95',143,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-96',144,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-97',145,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-98',146,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-99',147,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-100',148,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-101',149,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-102',150,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-103',151,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-104',152,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-105',153,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-106',154,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-107',155,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-108',156,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-109',157,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-110',158,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-111',159,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-112',160,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-113',161,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-114',162,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-115',163,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-116',164,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-117',165,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-118',166,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-119',167,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-120',168,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-121',169,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-122',170,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-123',171,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-124',172,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-125',173,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-126',174,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-127',175,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-128',176,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-129',177,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-130',178,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-131',179,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-132',180,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-133',181,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-134',182,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-135',183,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-136',184,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-137',185,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-138',186,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-139',187,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-140',188,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-141',189,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-142',190,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-143',191,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-144',192,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-145',193,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-146',194,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-147',195,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-148',196,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-149',197,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-150',198,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-151',199,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-152',200,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-153',201,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-154',202,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-155',203,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-156',204,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-157',205,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-158',206,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-159',207,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-160',208,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-161',209,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-162',210,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-163',211,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-164',212,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-165',213,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-166',214,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-167',215,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-168',216,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-169',217,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-170',218,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-171',219,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-172',220,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-173',221,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-174',222,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-175',223,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-176',224,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-177',225,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-178',226,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-179',227,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-180',228,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-181',229,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-182',230,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-183',231,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-184',232,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-185',233,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-186',234,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-187',235,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-188',236,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-189',237,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-190',238,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-191',239,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-192',240,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-193',241,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-194',242,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-195',243,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-196',244,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-197',245,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-198',246,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-199',247,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-200',248,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-201',249,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-202',250,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-203',251,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-204',252,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-205',253,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-206',254,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-207',255,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-208',256,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-209',257,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-210',258,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-211',259,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-212',260,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-213',261,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-214',262,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-215',263,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-216',264,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-217',265,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-218',266,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-219',267,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-220',268,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-221',269,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-222',270,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-223',271,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-224',272,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-225',273,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-226',274,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-227',275,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-228',276,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-229',277,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-230',278,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-231',279,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-232',280,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-233',281,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-234',282,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-235',283,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-236',284,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-237',285,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-238',286,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-239',287,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-240',288,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-241',289,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-242',290,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-243',291,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-244',292,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-245',293,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-246',294,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-247',295,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-248',296,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-249',297,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-250',298,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-251',299,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-252',300,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-253',301,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-254',302,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-255',303,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-256',304,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-257',305,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-258',306,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-259',307,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-260',308,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-261',309,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-262',310,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-263',311,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-264',312,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-265',313,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-266',314,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-267',315,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-268',316,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-269',317,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-270',318,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-271',319,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-272',320,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-273',321,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-274',322,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-275',323,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-276',324,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-277',325,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-278',326,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-279',327,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-280',328,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-281',329,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-282',330,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-283',331,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-284',332,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-285',333,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-286',334,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-287',335,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-288',336,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-289',337,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-290',338,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-291',339,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-292',340,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-293',341,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-294',342,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-295',343,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-296',344,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-297',345,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-298',346,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-299',347,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-300',348,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-301',349,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-302',350,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-303',351,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-304',352,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-305',353,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-306',354,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-307',355,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-308',356,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-309',357,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-310',358,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-311',359,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-312',360,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-313',361,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-314',362,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-315',363,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-316',364,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-317',365,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-318',366,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-319',367,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-320',368,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-321',369,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-322',370,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-323',371,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-324',372,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-325',373,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-326',374,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-327',375,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-328',376,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-329',377,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-330',378,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-331',379,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-332',380,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-333',381,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-334',382,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-335',383,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-336',384,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-337',385,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-338',386,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-339',387,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-340',388,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-341',389,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-342',390,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-343',391,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-344',392,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-345',393,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-346',394,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-347',395,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-348',396,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-349',397,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-350',398,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-351',399,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-352',400,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-353',401,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-354',402,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-355',403,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-356',404,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-357',405,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-358',406,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-359',407,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-360',408,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-361',409,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-362',410,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-363',411,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-364',412,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-365',413,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-366',414,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-367',415,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-368',416,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-369',417,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-370',418,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-371',419,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-372',420,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-373',421,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-374',422,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-375',423,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-376',424,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-377',425,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-378',426,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-379',427,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-380',428,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-381',429,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-382',430,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-383',431,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-384',432,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-385',433,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-386',434,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-387',435,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-388',436,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-389',437,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-390',438,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-391',439,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-392',440,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-393',441,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-394',442,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-395',443,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-396',444,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-397',445,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-398',446,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-399',447,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-400',448,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-401',449,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-402',450,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-403',451,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-404',452,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-405',453,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-406',454,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-407',455,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-408',456,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-409',457,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-410',458,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-411',459,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-412',460,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-413',461,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-414',462,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-415',463,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-416',464,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-417',465,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-418',466,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-419',467,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-420',468,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-421',469,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-422',470,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-423',471,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-424',472,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-425',473,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-426',474,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-427',475,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-428',476,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-429',477,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-430',478,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-431',479,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-432',480,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-433',481,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-434',482,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-435',483,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-436',484,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-437',485,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-438',486,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-439',487,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-440',488,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-441',489,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-442',490,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-443',491,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-444',492,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-445',493,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-446',494,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-447',495,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-448',496,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-449',497,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-450',498,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-451',499,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-452',500,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-453',501,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-454',502,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-455',503,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-456',504,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-457',505,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-458',506,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-459',507,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-460',508,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-461',509,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-462',510,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-463',511,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-464',512,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-465',513,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-466',514,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-467',515,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-468',516,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-469',517,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-470',518,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-471',519,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-472',520,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-473',521,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-474',522,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-475',523,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-476',524,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-477',525,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-478',526,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-479',527,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-480',528,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-481',529,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-482',530,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-483',531,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-484',532,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-485',533,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-486',534,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-487',535,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-488',536,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-489',537,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-490',538,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-491',539,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-492',540,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-493',541,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-494',542,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-495',543,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-496',544,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-497',545,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-498',546,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-499',547,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-500',548,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-501',549,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-502',550,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-503',551,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-504',552,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-505',553,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-506',554,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-507',555,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-508',556,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-509',557,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-510',558,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-511',559,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('transcript:agent','item:stale-512',560,'stale',NULL,1);
INSERT INTO "sync_entities" VALUES('agent','gone',562,'e80d513d643858e5c9bff7b17b40fb4b2af298215bc26d3ee603dfc2c233f542','{"collection":"agent","id":"gone","value":{}}',1);
INSERT INTO "sync_entities" VALUES('project','@FIXTURE_ROOT@',563,'d423b005768e13394f79c51017f624f241fbf2d5339d4cd1bd643ec572645b40','{"collection":"project","id":"@FIXTURE_ROOT@","value":{"homeServerId":"fixture-server","id":"@FIXTURE_ROOT@","locations":[{"path":"@FIXTURE_ROOT@","projectId":"@FIXTURE_ROOT@","serverId":"fixture-server"}],"locationsRevision":0,"name":"fixture","path":"@FIXTURE_ROOT@","peerTeams":[{"id":"peers","members":["agent","other"],"name":"Peers"}]}}',0);
INSERT INTO "sync_entities" VALUES('work','work',564,'aabb57526846e96b3e42676b854f6fac3c0716471236c07bab860fc85b9ccb77','{"collection":"work","id":"work","value":{"id":"work","rootId":"agent","status":"ready","title":"needle work"}}',0);
INSERT INTO "sync_entities" VALUES('work','child-work',565,'f75bed3c0f9f416f778fb76bbc6760fcd710c3b2d1983001b8fa0b9d0544d17f','{"collection":"work","id":"child-work","value":{"id":"child-work","rootId":"child","status":"ready","title":"needle child root"}}',0);
INSERT INTO "sync_entities" VALUES('complaint','complaint',566,'f63dc2706cd91550aa1138e5cd9575e94c9723182dad637765d4b16acb137d0c','{"collection":"complaint","id":"complaint","value":{"author":null,"authorName":null,"created":null,"id":"complaint","leadId":"agent","leadName":"Transcript fixture","needsUserResponse":false,"readAt":null,"recipient":"lead","status":"open","title":"needle detail","updated":null,"version":null}}',0);
INSERT INTO "sync_entities" VALUES('room','peer',567,'6f341bf2e6ba4816552f70872aca418ae7303548a3a99d5b2bdf2c7c3335c333','{"collection":"room","id":"peer","value":{"id":"peer","kind":"private","lastMessage":null,"members":["agent","other"],"name":"Transcript fixture ↔ Transcript fixture","peerTeamId":"peers","peerTeamName":"Peers","rootId":"agent","updated":1.0}}',0);
INSERT INTO "sync_entities" VALUES('event','legacy-event',569,'05f1b967815f0155b188ec680dc49e06a0639a847d49ce9aaf15f8a1acaa4ea2','{"collection":"event","id":"legacy-event","value":{"agent":"agent","created":1792689640.5376985,"error":null,"id":"legacy-event","kind":"user","status":"delivered"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-0',571,'8e0ff897a5aae3ca5cef6868d7c7fa8d19f5ece41671214debe3c3aa79f299ff','{"collection":"event","id":"bound-0","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-0","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-1',573,'b23e7d891b4e400e1d07c1400a4c5cbfa59513aef0b5aa02d9ab02c4dbc2054d','{"collection":"event","id":"bound-1","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-1","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-2',575,'676fdcb003143ca7dc3bc1384d029c21324a803f9688cd570a6d86c0052f5812','{"collection":"event","id":"bound-2","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-2","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-3',1199,'e504b78786c1e65f135051271b2fa7a34060bb77751c9fb058d3638f8e95e2c3','{"collection":"event","id":"bound-3","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-4',1166,'79ebce5ac8caa46923143f9a88ebfb4a253b03b41f2812896860e40016c17fd5','{"collection":"event","id":"bound-4","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-5',1133,'31592f1b4bf44cd5c8c0c2dd52026eae0b9a632c4c63050df39a42aded145596','{"collection":"event","id":"bound-5","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-6',1100,'a5c9697394cf52ec2c9568f0e79958c6224a07ea13d7815ef65722016dacbec1','{"collection":"event","id":"bound-6","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-7',1067,'f4c0c014a91c6e65ddf0ad3e01a70e829f74e70c385d422ca8c82280c67a1c1c','{"collection":"event","id":"bound-7","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-8',1034,'c454eefb1eb7bcb98d21c9929c9b0cf75754225334f3ff673db04b732254897d','{"collection":"event","id":"bound-8","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-9',1001,'c8680885037768b56410c5400667358177869074ace290be96147408be459336','{"collection":"event","id":"bound-9","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-10',591,'83dde16526a6f47e03acdc5e25720a8de8c4ce087b394894da6be2ec03d0ce18','{"collection":"event","id":"bound-10","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-10","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-11',593,'1f4324eaa1bd69027f5e40bbd76f1be304da7f0cbb4e4f64da3870819884759e','{"collection":"event","id":"bound-11","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-11","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-12',595,'c326a07c990503fea15456b33f2cd0045d6f16f9355b24723ca2dbaebf4e80c6','{"collection":"event","id":"bound-12","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-12","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-13',597,'19737bb1e0fe0e02873265a91d9eca903d145a20f3eb650ef2f5ae9623777f90','{"collection":"event","id":"bound-13","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-13","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-14',599,'e6de5b13f3a5679f1bf399f3882798c8b399bd2b3998cb8815b4ba287b50286e','{"collection":"event","id":"bound-14","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-14","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-15',601,'46692a2ba617675b2cfb22f3dbdd3830179a64dc1dbe092716e01da883f8a876','{"collection":"event","id":"bound-15","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-15","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-16',603,'8190ff118a5f4e23ca0867a243e4202aa40d9274c2d02e53ede1255db119507f','{"collection":"event","id":"bound-16","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-16","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-17',605,'ccfa0be39e5473caf31d8b6b6b929a8eb6899a41a91eb0965549244732a03dd8','{"collection":"event","id":"bound-17","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-17","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-18',607,'c3e9d5798a84d26a219d8c0b477f7e681fa10cf628cfce256d765276a2eb9fc8','{"collection":"event","id":"bound-18","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-18","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-19',609,'b8270c203e20d65c5f902159596cfad35eb474fd635d5474b0a5cba6c1cbac19','{"collection":"event","id":"bound-19","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-19","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-20',611,'8167b7229bbcd0d9b9785e46d7fc42f317c0c4719574396b7c09a275e56a1cda','{"collection":"event","id":"bound-20","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-20","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-21',613,'1768cea13343ecff402023091d9705385f18f8ccd8e8b69f025638b724d2e6d6','{"collection":"event","id":"bound-21","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-21","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-22',615,'3e6d7b50832a6e1cf679385cf80bc1465e6e2438ea078d4af02428a3fbd69e7b','{"collection":"event","id":"bound-22","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-22","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-23',617,'9c044a400b0492a8093b088a9f2da56ce51e5717eb284bec8cd15b98df3ec985','{"collection":"event","id":"bound-23","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-23","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-24',619,'ea27f87323815378f8c2815970bc7335f257d0d68b61daf309ce3750d1777471','{"collection":"event","id":"bound-24","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-24","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-25',621,'e017911fdd075714624bdd814cc1aa282f63db7d539e629794782f9009b629de','{"collection":"event","id":"bound-25","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-25","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-26',623,'1a946374977a7acf3a93a0ff343f0af2070c91052ce9c7ef50a1ffff1fd5d291','{"collection":"event","id":"bound-26","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-26","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-27',625,'d10cce402f8c01ae1626de024987536336d9465881f12824db19bf89c4dff804','{"collection":"event","id":"bound-27","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-27","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-28',1205,'b8057a1512e2591a6e99172753f6e9b13658a96086b132ba7ead320fcc249d08','{"collection":"event","id":"bound-28","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-29',1202,'8b654fc36e800b7b3a5287ea85624eafa0367804ca02ccb3b34120d8c4825744','{"collection":"event","id":"bound-29","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-30',1196,'67d7aa5b47e9212763a00f35fdce7b99d9e055cd762749700321592aa1ff87fd','{"collection":"event","id":"bound-30","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-31',1193,'73585e5bc0a6bf46004214a4b42a854f89092e0334dbec8eb214f84d7fc49b60','{"collection":"event","id":"bound-31","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-32',1190,'2f55c78d3716e5cdafe8209ea92df03a67d0ec38e86d35230a97cbfea9c0e7aa','{"collection":"event","id":"bound-32","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-33',1187,'bc9d34ec14166af333c0098bccab3f8c22066d1543e7b561110171d05737f12e','{"collection":"event","id":"bound-33","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-34',1184,'b68c409945275dc5e9d45f99328a956a06b555e5dee27d91023054a5d62f9245','{"collection":"event","id":"bound-34","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-35',1181,'bbd093f05a4a6094663d686e2f7f54994c3e94332062b53d27d081c4aaf1d096','{"collection":"event","id":"bound-35","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-36',1178,'a0dd747ccebe79eabe3fdd9ddad48f91ce3e11d2e35649c6ef3b83c9ebd5a5ff','{"collection":"event","id":"bound-36","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-37',1175,'6decff759b319116bdffbdc0574e13d99e29d580a90ce40eff503ffc71386788','{"collection":"event","id":"bound-37","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-38',1172,'a768a851f3516758e2dffcf5361adf743959285558587aaeb4e07008d962a5a3','{"collection":"event","id":"bound-38","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-39',1169,'2133ec70f7679820d8a1db3d48744b8416bac2e9364a941d34a2c3ed446bb5e2','{"collection":"event","id":"bound-39","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-40',1163,'8c4eeb5efe9bf1da167686356bd5b5a887ae4715b8d6dfd92e5482d87a984c55','{"collection":"event","id":"bound-40","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-41',1160,'4bc242508309ee7d3dd3e890bf00d14519f3ec0b07ab54080beafac9656b83be','{"collection":"event","id":"bound-41","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-42',1157,'2e9ebc6d71561e40518445f0b15479ac4cabf68ff1830aaf709d3af34219f5db','{"collection":"event","id":"bound-42","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-43',1154,'fa75d010c6666f7a51b5e5a2b69d1747b93a14b27f3ce004dfdd08589e2fdfbb','{"collection":"event","id":"bound-43","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-44',1151,'f288a2fbcde61d66e6b02eaedc92a88b77a637faedcbca5fa46b8c54a6500c44','{"collection":"event","id":"bound-44","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-45',1148,'d8ff1e0e9afb2d1834e06ff05344a8bc09ac2a0391966ad70304974abccdfe03','{"collection":"event","id":"bound-45","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-46',1145,'ee03fb7e304947aa876790113a775cf57b8c88f904c1aa0502a5a99cad1b9984','{"collection":"event","id":"bound-46","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-47',1142,'06fbbe70f340a082785069ecee9b4be1f218ce24e829fa3196f7af9d215315d2','{"collection":"event","id":"bound-47","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-48',1139,'928c814ec68b7492f8e509a627a42e104f5207237f4e36d0db18c67608e92873','{"collection":"event","id":"bound-48","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-49',1136,'ae9c7b5e96aab75acde65390e45b69459f7e2338e31c9b48ca200eb2fb106689','{"collection":"event","id":"bound-49","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-50',1130,'bad2d15739dd9853f9e7cddafbbbd957d654a771126969e36fedb35781115cc4','{"collection":"event","id":"bound-50","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-51',1127,'3e3505385fe67710523c089ee838d659543074ac0ae969847a67edd7a6d07d41','{"collection":"event","id":"bound-51","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-52',1124,'bdec87a621ce55da46cf85d0fec2b0a52ab749cfadc2c876a8fe4813a3ce5bff','{"collection":"event","id":"bound-52","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-53',1121,'36258f8da335040699ce9abf29560793e3de4fddb69ad2a68b20236bfc1789a4','{"collection":"event","id":"bound-53","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-54',1118,'cad648258236f1488e5b689e6c22bf797e23bdd0395a3b04f1184f855391555f','{"collection":"event","id":"bound-54","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-55',1115,'023059826f63ba6f9229080afe842db530545e1c61e0c81d75f19a4667ecd013','{"collection":"event","id":"bound-55","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-56',1112,'a24f1307c6739f069717afc4ddbc5ce9c86a91403ea8e3b7063f12c93b86de60','{"collection":"event","id":"bound-56","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-57',1109,'05e4a7a86065b5c6850e9eb476631b176be8b4320c9cccf9fa67ac0eb3b24163','{"collection":"event","id":"bound-57","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-58',1106,'779e90ced3910081cd6f8683238a1959753516259862d2fd876a2ee79453abd1','{"collection":"event","id":"bound-58","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-59',1103,'b87fa1eddec604f9bff7805bdf79400a1934381977a0055892151b80af8f3c9e','{"collection":"event","id":"bound-59","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-60',1097,'fb2a36d903a26c02fb38580a0e5b6ab21c4e3658e53ed7bf4182e4cabbbcf592','{"collection":"event","id":"bound-60","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-61',1094,'4bf130bf73164a8609bfe958d3d84f96f91b475e713e2fa6e52e26fd60d7aa5f','{"collection":"event","id":"bound-61","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-62',1091,'aa0a518d6336345192780d30c7526107d4ad04a018e77744a814e9e15beafb55','{"collection":"event","id":"bound-62","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-63',1088,'7dc1441e304925ecfaa60fd16049f63191a1f7f3daf51ff4667a7c1b7ddd32cd','{"collection":"event","id":"bound-63","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-64',1085,'a47307d1675cd489e1ada6dc107cdd1f1e7aeca054b7a53e3334c2b00ed4767e','{"collection":"event","id":"bound-64","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-65',1082,'beebbc8c793086a6ed0d571b945d64ec565d6e347695d70b3bf20c0a36e14b65','{"collection":"event","id":"bound-65","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-66',1079,'e573290431eb9f9149ba1ba41c80dc096cd5529ca789e1031c5245440cb7c289','{"collection":"event","id":"bound-66","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-67',1076,'93820bf58fcfdc0572d56140bc65d60a378db657a64fb19ab223ed935a007525','{"collection":"event","id":"bound-67","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-68',1073,'239ade5c18b322c0d380280e9ca8b136c29e197f4eb0b0d6462c0729f151a8eb','{"collection":"event","id":"bound-68","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-69',1070,'2b5ff9a4f7beb5b2a7443e90491048bb74469ae8b28b2a320d6be49a718c5362','{"collection":"event","id":"bound-69","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-70',1064,'226f047f9a2e64bb87c74af9cad31cd5f1fb79e0b7777f70f78676289b8e2609','{"collection":"event","id":"bound-70","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-71',1061,'c8bad0d9e15f8fab141796fb7dc5ec70e25e8788e9d07b2ceaf6c740010f102f','{"collection":"event","id":"bound-71","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-72',1058,'e8410086cd186e8e7c7b98b83bb34579d5a255c6d8f35a840e4f60c3086728bd','{"collection":"event","id":"bound-72","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-73',1055,'f8c1a36ef75cef7452238abff57e2c73e6856a33cf0f6eb92715513ccc6cf80d','{"collection":"event","id":"bound-73","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-74',1052,'31533423432c37258da849885022c2345d69a18f2ab319e443688c9e6308c442','{"collection":"event","id":"bound-74","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-75',1049,'199b2f907b4663c5df3b5af8383186c6de889cb32cde67c1459a8bd5d124c326','{"collection":"event","id":"bound-75","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-76',1046,'8ac7ab52462313cfc7d26d164be3deef4451641d3e1b3a62c2f3950706087727','{"collection":"event","id":"bound-76","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-77',1043,'3013163d03bd3f46e071733ffba0e2eac1eadf691c94664a16802e214fb331e4','{"collection":"event","id":"bound-77","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-78',1040,'b7ae1d6ab90372458273573d75fbca9dc28076197a5291a273808d07bce0391b','{"collection":"event","id":"bound-78","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-79',1037,'43d0afad247ecc64fc39be1f99dda9888144fad6b3601fb91521d563ce98a37f','{"collection":"event","id":"bound-79","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-80',1031,'819cae36927059b86922897864a37fe4f1cd46ed66c0253b788b448ca6453c14','{"collection":"event","id":"bound-80","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-81',1028,'ff7bfd7d1988c19cfe610dddb34c5754c393358b658b2f11ff2cd95429282df3','{"collection":"event","id":"bound-81","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-82',1025,'7acd7b33c2f802edb58e268a1bfec847525cb37948f4858d01f842a5e12dcc45','{"collection":"event","id":"bound-82","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-83',1022,'6a08fb2f348d53361083bafe4f3cdf7f9265049595b4c10e956b9bb0bdd83ea0','{"collection":"event","id":"bound-83","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-84',1019,'f1b2345941ace8d38be6729bed501fc1e6ef6c0ef811bbd8bbfb7fdc469d5fc1','{"collection":"event","id":"bound-84","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-85',1016,'0722f89c15603f8f788edab29c807d51f744896ecb6ea8381f50fafebbee4b65','{"collection":"event","id":"bound-85","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-86',1013,'91c7f2f1db2ef6e614c73ab6007c000025b098942496ae7c59d9a6af4a9c7e19','{"collection":"event","id":"bound-86","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-87',1010,'82357190d3aecd56b94fd4170523df843a774be09e8c70be909b0e771c75a46f','{"collection":"event","id":"bound-87","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-88',1007,'bcadc925d8de1881fa295486dd0d803f323480c5b5e7875a21ad4fb7e43d2b11','{"collection":"event","id":"bound-88","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-89',1004,'3e9f2a7b59a0caa1cb8f689dee8fdbd2e56a485eecee67ed8e187c6843160970','{"collection":"event","id":"bound-89","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-90',998,'e91c8546cf656b1072b828f9ce180b8ef31e49fee2f221d1b85cd96072279d54','{"collection":"event","id":"bound-90","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-91',995,'36e5aa8a8dda92136ea38a1a80c3195a23b8586ea0e2fddf05a3e0e26ad095d3','{"collection":"event","id":"bound-91","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-92',992,'f1b0236a0d0e97dc2bb4f5bd43a98a896a188e63b7ced3ddf70be12301cf98f3','{"collection":"event","id":"bound-92","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-93',989,'b7f92cf0d8960734c97138de2c885fb90cc26bb5daffaa14cd61ba5a96074b00','{"collection":"event","id":"bound-93","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-94',986,'cffa539431cc5600802c2626eea0a991b99dba0668829393fc0dd32568d5e02a','{"collection":"event","id":"bound-94","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-95',983,'9344ce755094fe7e0d0a2601c77dc162261b27e0426b30ad5145761ddc02b54a','{"collection":"event","id":"bound-95","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-96',980,'67b3c1d539355ef0d24f5b1895c238ecbf6820dea3a501fc3d5c4e45844478b8','{"collection":"event","id":"bound-96","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-97',977,'30220aea16ba5333a7c125f30f1d4a236fed5529caa767805fec4ef8db28b500','{"collection":"event","id":"bound-97","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-98',974,'3dd8fab1131094dee1830beadb051d544286ba2f4a1d25a8ea815b4c16d4977f','{"collection":"event","id":"bound-98","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-99',971,'ff30f44cc4e91ae767ac6722247c73eab069a5f6aad2b8cfd6d7e4e045a8282a','{"collection":"event","id":"bound-99","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-100',771,'c4489235b6ec837ad6186dc2be384cee5e53ae94076a0584744a9a10caeb4b71','{"collection":"event","id":"bound-100","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-100","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-101',773,'7b33fca3cf9134d0b3158937be25747d75595002708215c66d2a433e09bdfdfb','{"collection":"event","id":"bound-101","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-101","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-102',775,'a12eee47839a1b3d2ebc29f68f4883e5698ae4ba2aaf380cbe7e54e023e28779','{"collection":"event","id":"bound-102","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-102","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-103',777,'9a42b5da8d56caf189252ec4996ba3a07df70ba6e68c1d1a2e72a5bd9931bce4','{"collection":"event","id":"bound-103","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-103","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-104',779,'03f3df9fdd5b7de7e92e0fa48b7e7d1ab45cfc8ee07a3a4870f16f305178e2b4','{"collection":"event","id":"bound-104","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-104","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-105',781,'171774731738789f4fe4d46a0443278edea829394dccc36152b8ee1a1fcfb305','{"collection":"event","id":"bound-105","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-105","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-106',783,'293cd655245009f2fe80b76baab5ba5b8f592ad9ef49a316823f324a99b3981b','{"collection":"event","id":"bound-106","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-106","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-107',785,'6cde1056facfc174cb26c77c467d8905623893fcba4d65199a460027597b384b','{"collection":"event","id":"bound-107","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-107","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-108',787,'22f76f22ed5d5a103c056024e49e13d6f2f970d5faab9e66ce4ca4d6054e7937','{"collection":"event","id":"bound-108","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-108","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-109',789,'dd84f57cb8eaa3e14a1cc3a5962cc7e323055ae8286e0cf23eccb2bbd8fce89c','{"collection":"event","id":"bound-109","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-109","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-110',791,'1f8195db1cca2cfe9e058a8dfb8abc2574d33dea9b067f97e9f3db6810703414','{"collection":"event","id":"bound-110","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-110","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-111',793,'2861a2bf97437942b21bb91f8d7da99d38df42ae58c1a277b2cefcd4cb9ca2d5','{"collection":"event","id":"bound-111","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-111","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-112',795,'afe3b64ae6b0ee78751c54f3017adddebd931b62e6d31ab7863d6456c70d2548','{"collection":"event","id":"bound-112","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-112","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-113',797,'ae4f49e5756393ec073a11ccce9dd6e5304837a16653961fbd4b9755fe681f93','{"collection":"event","id":"bound-113","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-113","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-114',799,'146167aa68a5bf3328ba9c5f8bcc321f611deb27c6fff3c5b2de87fb8a0784e5','{"collection":"event","id":"bound-114","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-114","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-115',801,'92f01019fbd0c7dc4dbe43d762a895a22791c9711bfccd9cd21b55645e4ce4a0','{"collection":"event","id":"bound-115","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-115","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-116',803,'9c66cc074356630225b2255148987d25bd7a08929b7bf2dacfa09bd8f0b0743c','{"collection":"event","id":"bound-116","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-116","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-117',805,'34a0b0dab906a4818d2570c8e0a814dffdda83c7edc367d511fe2a709d353d7c','{"collection":"event","id":"bound-117","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-117","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-118',807,'26849393078f83ddd5c9dd44ed07b4562005eb15fbb2c005ca3ae3cc73f6b12b','{"collection":"event","id":"bound-118","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-118","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-119',809,'34f69398750cf67aff79fc98bdce0be8104227b4738d83a84d8dae99c91ccbbf','{"collection":"event","id":"bound-119","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-119","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-120',811,'0163fcd132ffa529fbf5793a19efdab344bd15472b980bf9fbba1e674e12042e','{"collection":"event","id":"bound-120","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-120","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-121',813,'ff53a0ada684621ea3b6518879e8551f46c790244109905b8468ff0ba7435d2b','{"collection":"event","id":"bound-121","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-121","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-122',815,'95c5213950a48d03d5f45b82af4824c4beb3756db1af0532186760b5b4fcf840','{"collection":"event","id":"bound-122","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-122","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-123',817,'dc424f7611daf5c1779bdfff24083eae8f32aba821d10dda654e6f5af56e5cd0','{"collection":"event","id":"bound-123","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-123","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-124',819,'9ae3ff23cde01e7804541eea9582c61b3028bd295d52e758a15eb61f6313fd05','{"collection":"event","id":"bound-124","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-124","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-125',821,'923df19b3c3c953efe1070c8ede35bdf92920227818826ef395db20ef521d6a0','{"collection":"event","id":"bound-125","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-125","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-126',823,'be7ec8b0a8bc9944a532141f6048fc0f60c276d34684338b42d656e8872ecde9','{"collection":"event","id":"bound-126","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-126","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-127',825,'3f3eeffc4d0685cf553805313b7365554b4adc306af05372c96f034247178980','{"collection":"event","id":"bound-127","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-127","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-128',827,'7098994d25fa7123a5d5e137f029f286ba4bad5a04fd93aa0c5186184a0fd8b2','{"collection":"event","id":"bound-128","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-128","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-129',829,'c55ec95fb6ebcb409e3958d7a644005dc0036eb314fc324252abafdbf233cb14','{"collection":"event","id":"bound-129","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-129","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-130',831,'6d1203f61d30c86876c9b1eade1d1a40b8fb4268d9fa4091b022512cb078ca68','{"collection":"event","id":"bound-130","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-130","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-131',833,'5560545b02a2c0640469d7bb4516470f8699d6c1a2f728ba2e86c8aa263ac46f','{"collection":"event","id":"bound-131","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-131","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-132',835,'3c8a714f8b1ca09bbe465c66327a5b8e3deccba5b827a4a0e7c026919a6df100','{"collection":"event","id":"bound-132","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-132","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-133',837,'291fa879b00cc377b68b6a0013b3fb3f19a14f839532c629efc0b253dbfd905d','{"collection":"event","id":"bound-133","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-133","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-134',839,'7cde9e4dad4e63d8941d4f64d1aee57beb09760428244d8f77d50b4eb185c115','{"collection":"event","id":"bound-134","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-134","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-135',841,'0d2e567f1f82d16347bb79cbb83894ab40a95bcfa2d37410dffbf684991ec7a4','{"collection":"event","id":"bound-135","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-135","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-136',843,'b55d48f9ae8b70b3d48197883365b362eaef1750e17df8d732e1413316fec7be','{"collection":"event","id":"bound-136","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-136","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-137',845,'034e185a3c8e67e831c0652164bab1e4faf7e8ae7909da06cf79cb1c3fc9d0ba','{"collection":"event","id":"bound-137","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-137","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-138',847,'aba602a8205f046a799e81c121d2651bee8e605a6970a488ac1c4530711e2da6','{"collection":"event","id":"bound-138","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-138","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-139',849,'97f0bbb0ffc1a21a07eaf69310ba319b5aa421b532d0cd0edb0176336a10182e','{"collection":"event","id":"bound-139","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-139","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-140',851,'f9aa3cd753a00e3419aede16820194622c15b485aaa3b73ccb8194772da3a5b2','{"collection":"event","id":"bound-140","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-140","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-141',853,'2486b383a1d65536a2f7318bd723d9e8ce1b4d5073c077393ef4c7a5b6d08be1','{"collection":"event","id":"bound-141","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-141","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-142',855,'96646f593fdb36596834a2f051ba375b980c84805e6c19c3374d8f05cde389d9','{"collection":"event","id":"bound-142","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-142","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-143',857,'9870416f940292903a5de997268509825cae623ba871c1571f6333826e0adad6','{"collection":"event","id":"bound-143","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-143","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-144',859,'121df9d17f720fc95bf3b8733cbc895c30e3e0986814f792c7390716ee6ef5ff','{"collection":"event","id":"bound-144","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-144","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-145',861,'8419a2762d3f76f00c76f14c53ea3e3c2a535b3f7ed90d2dcad1b70a4db1042d','{"collection":"event","id":"bound-145","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-145","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-146',863,'2979fb91dc3c8fca66ffcd1e8e4c35b4e63d84133076054e2a0e9fc122c54bed','{"collection":"event","id":"bound-146","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-146","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-147',865,'a1a303b92ea1c80b594b46167b81f0583168b69986c7e3afebea33f851853fe8','{"collection":"event","id":"bound-147","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-147","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-148',867,'2dd5c349acf8771316c362426b39e59d722685b8f41a89c26b4dbd5059873893','{"collection":"event","id":"bound-148","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-148","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-149',869,'8a44b5151131cfc6e4886e15e76a2fb5d5f4743a15716511bf830984a0ac4395','{"collection":"event","id":"bound-149","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-149","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-150',871,'2893736b3b6ade4851353faa51cecbe0009e4d287b0f157023d08220fc213ca4','{"collection":"event","id":"bound-150","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-150","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-151',873,'d3aaeba59a9374e11cb6b7bc7fec637379fde8fc0aa5aacc663aa31529e8ade5','{"collection":"event","id":"bound-151","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-151","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-152',875,'63b6f53f17d168c9883f53168d38ab758df7579fb69393167d4cfb7e706141af','{"collection":"event","id":"bound-152","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-152","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-153',877,'b79986376dccff51d99ce7ccc9670c2bcb3ba808d50abde7317003c630f8379b','{"collection":"event","id":"bound-153","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-153","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-154',879,'beb16e6cafe28356d3bdd6ce691c13e32fb086b83634848799962103fc858d6a','{"collection":"event","id":"bound-154","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-154","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-155',881,'88740c3209c653bdcca9a9526c89cb74693f56cbdc19f712b647c456cbb5bbbe','{"collection":"event","id":"bound-155","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-155","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-156',883,'f3387557c04220537e3e49c04b8ddaac654b3f57d91f835675d1036682fdb074','{"collection":"event","id":"bound-156","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-156","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-157',885,'a25a6820621c049fe089ec249adc275b11c4b773b76fba61f84ea042dadc445a','{"collection":"event","id":"bound-157","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-157","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-158',887,'ab1ec88072544f04f366315e4a43977939950a90b15cb5696d4cdeaae8d0840f','{"collection":"event","id":"bound-158","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-158","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-159',889,'7ebb8409cded6297a70f9327d28be4d1161f1d1714258fbc6f4e67fb29ff716c','{"collection":"event","id":"bound-159","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-159","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-160',891,'79ccfebe6114c6bc0dd52d2c1b83935530eff246ebda2be08faddc745ea6767a','{"collection":"event","id":"bound-160","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-160","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-161',893,'305c6d2f771db52931b3c073881c6b0fcdaf1a723ee3a2929d6e1b8eac4fe7d2','{"collection":"event","id":"bound-161","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-161","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-162',895,'4e5e9a258857229b53572f35c8304535b003623df94532e7ac028046e9935aaf','{"collection":"event","id":"bound-162","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-162","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-163',897,'4c28b4c69e6ebc7bf29ad5fe7456d6fe27b1d37ac2dbae1f1de8906f86694afb','{"collection":"event","id":"bound-163","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-163","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-164',899,'eee136665a73ca4e14053cffa001289d0e110cdbd10d092575e63292af131234','{"collection":"event","id":"bound-164","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-164","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-165',901,'86f1f6594263c6204d33c9f0596dd8e046b5e415e34a88c36478ac85f17bc0f2','{"collection":"event","id":"bound-165","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-165","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-166',903,'c61620d0df7134671857ae0a5818aadc1b22cc9b78dacc9f592d62bbbca6c195','{"collection":"event","id":"bound-166","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-166","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-167',905,'098acac0dc79e17bdce2ec24e7a91ee3a8eaa5ceeda197bb46857e727b080eb6','{"collection":"event","id":"bound-167","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-167","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-168',907,'26ddc30f4f373d15935ef54d511b75af1cc03519bf8f2181d2532139ee498d0e','{"collection":"event","id":"bound-168","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-168","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-169',909,'532adee1ef1732989b8961fd72d86f320063f1ad1fa8d106eeb8ef71be36dde3','{"collection":"event","id":"bound-169","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-169","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-170',911,'c5f95595f25c35e178e15e56dadd15568baef0b9efe9186e053356898c063c10','{"collection":"event","id":"bound-170","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-170","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-171',913,'8ffbe15878b98c6e66b77d516f32a9616d8ab434feedb2152ca632705f737a8a','{"collection":"event","id":"bound-171","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-171","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-172',915,'9d3fd07509fa9f3eeef7820d1a777ccb531a25cc319a71745e91d22cb4f1509f','{"collection":"event","id":"bound-172","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-172","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-173',917,'3bebb368219fd36565d9cbf2a7b7fdb3385be5b54168695e6de02be03a9372a1','{"collection":"event","id":"bound-173","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-173","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-174',919,'983b1938f3cb96b5b1f587ae912cc449125a8dd301b5cd400862cfe91ed04555','{"collection":"event","id":"bound-174","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-174","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-175',921,'f85d747df07b1776532d4a06cd1a8b60769a744cdd5c8a0f1408ce57b7f07b8d','{"collection":"event","id":"bound-175","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-175","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-176',923,'469d955444d88bc4ad7ece0cd6b147a0ff4ba69a307fa5c08f1061fcfcfeb7a0','{"collection":"event","id":"bound-176","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-176","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-177',925,'d148cf3191aca432c967448d6444142e024a1c36ea8c82bef432b9b519205bab','{"collection":"event","id":"bound-177","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-177","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-178',927,'3379b09cb338be04aa602392c8780c6c60f1a864b64213eafe0f1c7079e50dcc','{"collection":"event","id":"bound-178","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-178","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-179',929,'cabb3c22fc90d4c00d80d5df59b516ede8d489ecf10103089b71dff453d5cefe','{"collection":"event","id":"bound-179","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-179","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-180',931,'94fc60a37df94c4eabb3cf33d91f35198db925c52ffd94318ffda852ad2a923a','{"collection":"event","id":"bound-180","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-180","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-181',933,'528497757c802299ef4d28648597536971da4454f3a6795746297b6e39fd86dc','{"collection":"event","id":"bound-181","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-181","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-182',935,'4e240c23bf64981995213c88927efadd9c8951bd109a16cdb5d40c8bd48ba16c','{"collection":"event","id":"bound-182","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-182","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-183',937,'5ec73f54703d608cdd5f0faf21e2509f9631146c482c7238bbfd11e01cf4f35a','{"collection":"event","id":"bound-183","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-183","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-184',939,'e238fda944d7678bf8e75658dd8759e08f38e32a3e78c778c0ba42f813dde476','{"collection":"event","id":"bound-184","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-184","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-185',941,'1e18d295f91829444c2e56a7761d8a5e06bdc337e94a1dbcea1c89528ee54632','{"collection":"event","id":"bound-185","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-185","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-186',943,'e5c5fb9462f57d8c7afa567f237172d64cbf32bc3753d3a971394a85791a48ae','{"collection":"event","id":"bound-186","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-186","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-187',945,'183f492b43e7af18d8fb6c4b04dc29279799ab8d0638168baec2aeb3b99c4455','{"collection":"event","id":"bound-187","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-187","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-188',947,'7734a4eca0fe3b47622107480ec2f0f6f5ac5c073127bc3f7d070ee02d75c916','{"collection":"event","id":"bound-188","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-188","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-189',949,'c27a1e4868b2edb841a0882e7072f4ce6aa58fd8e786d6f1d5bf4481be2114c4','{"collection":"event","id":"bound-189","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-189","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-190',951,'8891e00c43ed2ab8611b655615b6d8c4344a54261baa5b7200b3ee4122dca3e0','{"collection":"event","id":"bound-190","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-190","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-191',953,'a70b16259b69ef2534b6c99ace943dcd974293bfed3f361b6135b91f749d6f14','{"collection":"event","id":"bound-191","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-191","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-192',955,'e6c07b47751310a0257be3ae17ac905b9afc25618cab6a8517bd070b4272f868','{"collection":"event","id":"bound-192","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-192","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-193',957,'e467c446c0e9ea0ae88a6f0a5d1821c1d8fd6429bce8000fcaaab05b2010fc30','{"collection":"event","id":"bound-193","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-193","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-194',959,'a6f9005c6470ec8aee218a92bc7902fc5503a35f33a866460d96b83699a30b72','{"collection":"event","id":"bound-194","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-194","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-195',961,'03b1bb6c41a67bd94e066599c45268df5553ad0de2ac264dea17a1c2553d8e3a','{"collection":"event","id":"bound-195","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-195","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-196',963,'800f76b30eb320c25f55950755c3d2c54fe668ffa8168ed58e90f0f49b43f352','{"collection":"event","id":"bound-196","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-196","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-197',966,'8f08f8657259507fdd3ebbb9c162c1a26761567c002ee07cae14e86fd591010d','{"collection":"event","id":"bound-197","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-197","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-198',969,'381b1b0e6777b16618632e2d4b650508d659f1893c04cdc2c043682067845b26','{"collection":"event","id":"bound-198","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-198","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-199',972,'cc51fa24666179570eb706a2e647b93e4eb7c9c7fbc366c1e7984c7e0d7580a8','{"collection":"event","id":"bound-199","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-199","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-200',975,'eaa0a67f9f8690a3f643677326b0995965c9401a12ddc5b089966c6508640bf6','{"collection":"event","id":"bound-200","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-200","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-201',978,'78b59c85ee816d3bbeb135676aaf8fe3dd72dc91e6143713cdb83f186ea70533','{"collection":"event","id":"bound-201","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-201","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-202',981,'2e7839928ec36250772ecd97e506ba8042fd6a816beebfb07884b36ef7799916','{"collection":"event","id":"bound-202","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-202","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-203',984,'512560ac03abf83353bf01c15a0fc261534c9bcaec36d74cdb864747f3f22ed0','{"collection":"event","id":"bound-203","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-203","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-204',987,'de0372bd27fc18d17b8ff1742ba737b9609dcc87cfea233d0c13d5976485ff2e','{"collection":"event","id":"bound-204","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-204","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-205',990,'57439c078fc272382806fad4b622d1cf79fd756038675f48713b6c148feb287d','{"collection":"event","id":"bound-205","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-205","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-206',993,'838a66ed7205a13b2d76ce7631f8d81ed94e345f32566b615bc85e705d882b0a','{"collection":"event","id":"bound-206","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-206","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-207',996,'4b42dda0a99f14750177959e543a12ef136cd15f9c81772fff63e6ef81402d10','{"collection":"event","id":"bound-207","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-207","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-208',999,'1a88f4f6189cbd900a4e71008380bb185820a4aebf7f99afa145e8a81f33164d','{"collection":"event","id":"bound-208","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-208","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-209',1002,'660e36b2f89973846562180b510dd652947ccd2ccd8c8adf4d5ec751cb3b0e57','{"collection":"event","id":"bound-209","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-209","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-210',1005,'156398f78030249621fe5bda8d9b4d6d3010937969c8ce975236092b9c3ae177','{"collection":"event","id":"bound-210","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-210","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-211',1008,'1519268e3f280a79b0068fcca0b104e68e47c527bd497a79418acfc0965db228','{"collection":"event","id":"bound-211","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-211","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-212',1011,'08db443bd3be45b55c44411227e71f4efb6d8e2eb5ca61ddde19f0294e3db40a','{"collection":"event","id":"bound-212","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-212","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-213',1014,'20385c66bb602880fb7e9000872855be7a86a5dced8d10a5cadeca1e157b95d3','{"collection":"event","id":"bound-213","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-213","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-214',1017,'6b58c2f14b8b8301d06844b3fcafcb31f6715e0397ab03c541fe2f0980a5f12f','{"collection":"event","id":"bound-214","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-214","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-215',1020,'86af98f240bc28c8fab57f3345ec639576d2777b1ce14d8255b6ba5c35cf9b1e','{"collection":"event","id":"bound-215","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-215","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-216',1023,'286129c07d2a99b077847c31ccb2f0c0be1d587796d1764f02ed809114e26b87','{"collection":"event","id":"bound-216","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-216","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-217',1026,'116109449c887df75a74f486fbea3b23d6944bb8bfe7e7ef48b9fe39b67d3291','{"collection":"event","id":"bound-217","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-217","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-218',1029,'4fbe464723a826e3c39ea70857e3b66ef39c0d682b8679ce175768fbad1bf537','{"collection":"event","id":"bound-218","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-218","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-219',1032,'472adc24ab82a5931882de8f2b592eaca5c4a34af80c1b1b954de9a568cd0d45','{"collection":"event","id":"bound-219","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-219","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-220',1035,'b972a68eb0e463763cbaea75c729053052bd2d0ba462e941190ea01663f3cb38','{"collection":"event","id":"bound-220","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-220","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-221',1038,'3195423142a27aac760461e23875de85b5fe9a35e9dd0b95bc53eb70e8254960','{"collection":"event","id":"bound-221","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-221","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-222',1041,'603ea8aed28c4cd0a2642da306fbf1ddfe110b5530c5f3db040b9a76c36fe10e','{"collection":"event","id":"bound-222","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-222","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-223',1044,'764c3a184d2dc97cfef10bdbd1552be94a925d5bd3f382f3f82365282f40e2af','{"collection":"event","id":"bound-223","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-223","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-224',1047,'4c7e05747ac38f015ed0f3510892a5f52eafd5a3536d9b7b68ca2bf9ddc7df9e','{"collection":"event","id":"bound-224","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-224","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-225',1050,'791f46ca3b48840f2e71fdd4aec8993f59d9ee833509bd25e69eb337e309d62c','{"collection":"event","id":"bound-225","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-225","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-226',1053,'e687548ec24668e5b2e3f5afe21cf3e83da50bc4f7b1da84481414b151f25d74','{"collection":"event","id":"bound-226","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-226","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-227',1056,'a0ac92d145f7901df7936b628e048232268961d639503952ededaaec5cb00674','{"collection":"event","id":"bound-227","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-227","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-228',1059,'e6abae785f31deefb18fe819e1a14d31ce01184e98c55d1da64f897b47f2e2ce','{"collection":"event","id":"bound-228","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-228","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-229',1062,'91996cb649e96b4c80887981fd727410655924e9e7b96986345a65b666ffd585','{"collection":"event","id":"bound-229","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-229","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-230',1065,'b3f44b0018a47ddfa666f84e1a9ff4296cdc1deb9083d25f168d988cf208da7a','{"collection":"event","id":"bound-230","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-230","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-231',1068,'a6234d8f1f09b7d3079d5c588f40e305440cdcb17263de8975867259e1d381d7','{"collection":"event","id":"bound-231","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-231","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-232',1071,'5a9be2c6309eeb853aefed57cc8a2312d925b20347ea778d2fcd4302b5ee5ab4','{"collection":"event","id":"bound-232","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-232","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-233',1074,'8c0a5f3267367ba2ec09327eada3ce7cd7bd8d3529257e59292f28fae4a2bd97','{"collection":"event","id":"bound-233","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-233","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-234',1077,'26f8ebea866833ba75e438f597828572e36adbd2bb17c20c78a0724007742c7a','{"collection":"event","id":"bound-234","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-234","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-235',1080,'733f1a849cc0c90ba56e5a5bfe837cafa59176ddc33e54313e4077f5566f3fed','{"collection":"event","id":"bound-235","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-235","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-236',1083,'bdd66cdd9ab82596510dafadbef2e6145208b8d07dd70fdcbcc05438a4f69799','{"collection":"event","id":"bound-236","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-236","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-237',1086,'917da752863267d00cf961354f4c5dffe6efb4433997691b0801a1ffcec116b2','{"collection":"event","id":"bound-237","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-237","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-238',1089,'bdb66842e7c94087642b402d47b648cc61261b497a7feeba82024f3ca557a264','{"collection":"event","id":"bound-238","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-238","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-239',1092,'6e94a2b02f03c0673b4e3c886a6a1dc3d39512b708eab395fc522da8c60a2112','{"collection":"event","id":"bound-239","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-239","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-240',1095,'a110d1205e730aeb2a930ddef542dfea2924eedf7e2e58b80f420f5529422448','{"collection":"event","id":"bound-240","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-240","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-241',1098,'31a5f69b8bed14da3cd4454358b457a7debdcde883e417b3ffa9b673b9ad4d03','{"collection":"event","id":"bound-241","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-241","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-242',1101,'ce8f918b162ddb5653a55dd06e588379918fbb0aefd1d243ed18a50958349360','{"collection":"event","id":"bound-242","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-242","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-243',1104,'85b6b83c679912b687c13d1f0f32d9a8c3b8bcca085b4b069def87fdeae7b757','{"collection":"event","id":"bound-243","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-243","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-244',1107,'03bf7a182a281bc6e5ee3d4a23dadf137c918e380dbbd28fe87a3a3697f6870f','{"collection":"event","id":"bound-244","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-244","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-245',1110,'92ce0601bf4bd7601098926af4ecd6f0cdbd35ffa3adb3bb80caec30d79adaff','{"collection":"event","id":"bound-245","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-245","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-246',1113,'096bce6ac80e88dfc0f332a12223ead580f81b3d849f5176b5576588673e7b05','{"collection":"event","id":"bound-246","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-246","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-247',1116,'f8351d736a3c438a2ef680a81e5b917568921e8460eaf3c8ec56e9529935daee','{"collection":"event","id":"bound-247","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-247","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-248',1119,'f9a6aa6c448020a03883b1c407852c48d2e613129472f1d1b66a5d6ea495053e','{"collection":"event","id":"bound-248","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-248","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-249',1122,'f315586b1056443db766ff7913ec2d974a1cc0fbc6eac74e85a68212980b4d69','{"collection":"event","id":"bound-249","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-249","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-250',1125,'bff0ef6f77cae41c220ce94d6946abbf6d437a968224be94f80f07425f02ead3','{"collection":"event","id":"bound-250","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-250","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-251',1128,'da7f4e1f1186566dc172b56de827453a5eec8a6c30345b409d6b093efaad7bc8','{"collection":"event","id":"bound-251","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-251","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-252',1131,'d4e0ee8cff01b24616339600a18be8f03b7e5202bdb5c1d416389791f9b05d95','{"collection":"event","id":"bound-252","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-252","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-253',1134,'9eebc14cc9f543e54c1de553c79b20bacaf8538e6b0ef74822fdf29f54ea431c','{"collection":"event","id":"bound-253","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-253","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-254',1137,'d81d9bddc65256ce6592239f3d17069eec1da9a1b2c26c667e60cdf653bee89f','{"collection":"event","id":"bound-254","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-254","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-255',1140,'b7af12eba6cc6afdd594129a6ef8bcc585a55636d9c724ce8055fa278a87ce3e','{"collection":"event","id":"bound-255","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-255","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-256',1143,'05deeeaccac5d8049b75461e0d737fff4fc0fe4048b577696462ac98bf75cdf4','{"collection":"event","id":"bound-256","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-256","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-257',1146,'3ef26ee628ce16a715ca7c75684be882177d19c2785261e4b151c34f69e5366e','{"collection":"event","id":"bound-257","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-257","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-258',1149,'fcd277ca625cb4c791b841a67cae2d01b8f4e01cdc1ce9b3c8a029dfc229431a','{"collection":"event","id":"bound-258","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-258","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-259',1152,'32fac75571775b87e71e5564f95c5be81fe3fc43641bcaf5b2c9abe6a2cbd8fb','{"collection":"event","id":"bound-259","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-259","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-260',1155,'24e4bd08312ed9c4389c320aa236d126fba36ec33a640c74110607d1db0ca365','{"collection":"event","id":"bound-260","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-260","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-261',1158,'4e6369da605c5dc363a9f283d3330d24a0f3a19e36716dd862206bff222e2e58','{"collection":"event","id":"bound-261","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-261","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-262',1161,'43982244a9e176bcc150245308e71945faff2316cedb25b6eee8d2722199fcbd','{"collection":"event","id":"bound-262","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-262","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-263',1164,'56926c373d4b3d43c223144d0d5771b725ba7e988be72cd6c21c7dc0a3bc40e9','{"collection":"event","id":"bound-263","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-263","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-264',1167,'b1fcfd7b8fe4243600ca37dca6c28b47421e6935bf93be0e2144595a98560646','{"collection":"event","id":"bound-264","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-264","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-265',1170,'044ba91b066057e279f7883093dd519e78092b4c1cbd5f508b3c3a6692bdf824','{"collection":"event","id":"bound-265","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-265","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-266',1173,'1cba6a6e8a8aa52c4494927412748cac25026b1a41cc8d8b580d2b5fd2c32c3b','{"collection":"event","id":"bound-266","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-266","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-267',1176,'35d2722d58a4a98e241d2e481855df19997a7c9e0db4cebf7c613931d5bd29e3','{"collection":"event","id":"bound-267","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-267","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-268',1179,'1d71f3a464f3b97601a99500bfdea79ee606a0fda2c5a14bf02441a20cf9c634','{"collection":"event","id":"bound-268","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-268","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-269',1182,'b54702e58aad8c42554f7a76037efc1560fdf1c7bb2bd0c5fb0a664594f176b6','{"collection":"event","id":"bound-269","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-269","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-270',1185,'6c359bb7ace32b58ca46710b31ca86c3b4bd7817dac275ae6555e603805963ba','{"collection":"event","id":"bound-270","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-270","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-271',1188,'bb9a3a560da3b7034203b9c4be7736c63f7190330df420b42b3350b91dfd96c5','{"collection":"event","id":"bound-271","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-271","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-272',1191,'ee36ad11d169b45adedd61b88f67aa934fd2602d33afec3c543570559b8aa345','{"collection":"event","id":"bound-272","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-272","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-273',1194,'d1aa62c3ad8352e9a4654f420a1c3e27f654ac117028530be134ff95223ddc35','{"collection":"event","id":"bound-273","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-273","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-274',1197,'7adb4867850c2af585c52ddfa8751aac0f44564af414b0de68f53844d43722e9','{"collection":"event","id":"bound-274","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-274","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-275',1200,'9bd01f5d8b7ff40936e06c45a783253cabb48c3806a75af26efa33658cab4fae','{"collection":"event","id":"bound-275","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-275","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-276',1203,'3a9e79a78a5afabec00cf197499f6c790a48c100b55f25d4cae6d7c6bb209485','{"collection":"event","id":"bound-276","value":{"agent":"agent","created":900.0,"error":null,"id":"bound-276","kind":"user","status":"pending"}}',0);
INSERT INTO "sync_entities" VALUES('event','bound-277',2313,'1d8ed31ef11ddd805e81e9ca130cd428697e636f206759017a534e7790f39ec1','{"collection":"event","id":"bound-277","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-278',1211,'a26815f3f40624532d030f7c9abacd8e8245ec4cc2fcb8b74d875bace3fdbaed','{"collection":"event","id":"bound-278","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-279',1216,'adf62faef33d10b479abee6bd4f54db83670ddc9af9f4fde08aebd73d3afd81a','{"collection":"event","id":"bound-279","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-280',1221,'84aaeacfbb5338270fe103e693c1f42a76e80e227473d568c2decbe119784991','{"collection":"event","id":"bound-280","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-281',1226,'2e645a540398c6fdb943e8db05a4983e4f28f57268f837bfe9270e288d65d94d','{"collection":"event","id":"bound-281","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-282',1231,'9cf2684ef4cae115a2c8befcf88dce2d34d70b751c5f773a411a7514b2bf8408','{"collection":"event","id":"bound-282","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-283',1236,'9006ebfca17bd5e6d544c90194c58622602c1fb50e9505775b5ad72a307e7f56','{"collection":"event","id":"bound-283","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-284',1241,'40037092c8784a755e26977342a7e2f501971a59fa87009ab5c44f6ea0a6732c','{"collection":"event","id":"bound-284","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-285',1246,'b93ea41cc2a0785e594a97bfd1189f3f3463a902e7ec64526239397f1a03dc0f','{"collection":"event","id":"bound-285","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-286',1251,'7d9d7201d8e2586db415f86fa15099dc9300d1c6bcd3c54845719cae9937b07c','{"collection":"event","id":"bound-286","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-287',1256,'86e65fabbb03f6aaea26349391521c1c44b51b7167ef196584c80e969e853d47','{"collection":"event","id":"bound-287","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-288',1261,'74c319630295e4512d6e468d81d3d9328cac1886cf12e1a8f9f374dca1deb882','{"collection":"event","id":"bound-288","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-289',1266,'63d3ac934665aeabdddeede45e66c838922d498339e771dc98ac11216cb857cd','{"collection":"event","id":"bound-289","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-290',1271,'f338ef9dc4f158bb43fc5ea299e82e06c04f507cd793a01c3e758584e87aa314','{"collection":"event","id":"bound-290","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-291',1276,'6e5a705a432fa2a892bfa3333a4c85bbb766553ab8d852166fa3b43b4e6b3d22','{"collection":"event","id":"bound-291","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-292',1281,'813ff4861d26a2491c9aea42574f3fa11fa0022bb731312b4578144e02d9632f','{"collection":"event","id":"bound-292","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-293',1286,'25a87e787a1752c872ba75bb80fc2b818a34d5383b0c7e378c92f73bcad14df4','{"collection":"event","id":"bound-293","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-294',1291,'8f7d7f61cc100a4d2ebe2954be79841bd6ea48cef4aa173f7abf582456e2b320','{"collection":"event","id":"bound-294","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-295',1296,'29ba826394628ff1d888fa85e6b5a7b17169853ef6b408e553b82623579ae8fe','{"collection":"event","id":"bound-295","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-296',1301,'3443072754c3dce351d23f6c213ece144d3c91cada5ca9845679842053248aeb','{"collection":"event","id":"bound-296","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-297',1306,'672c3ef23f8737a3659914363efb6641fcd77d3ecceeb4322f3df462e57aa9cb','{"collection":"event","id":"bound-297","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-298',1311,'24dd6dc660cdb9497cf2fe36fd5e9c5d497c9613f6c3757d703d0f871d98531d','{"collection":"event","id":"bound-298","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-299',1316,'bf502066afafe5a97c176e91b0cc06e41dc837eb011d5360a58bba101c754e04','{"collection":"event","id":"bound-299","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-300',1321,'86bf42c950d2f7622b6afabb6573d67a0a0acd5fdccd547f9cb5b6b7a2a90258','{"collection":"event","id":"bound-300","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-301',1326,'0c85bb3e612ab86ad53d6258d7c53a87dc355c492c9e299bf935f1a7a8de187e','{"collection":"event","id":"bound-301","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-302',1331,'bf19aa24938926a9440bb07a5165234c435a512f9e551eef8cdb7aef378b5a75','{"collection":"event","id":"bound-302","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-303',1336,'0f526e5517288d5c730544408ad6b613fc2109ead43a79443b818c6307fc3c53','{"collection":"event","id":"bound-303","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-304',1341,'aa583882d1e303e788d8511029c771e91f0787eb27bd115079d0088707049bec','{"collection":"event","id":"bound-304","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-305',1346,'75f3696ca2f81d799b02ad5f3705e21804c99a9b85cdd3871deb441527d8e156','{"collection":"event","id":"bound-305","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-306',1351,'c5972a41ba6f818393628f0fe0191c2d282c68db18d41b218872115fe43c5f09','{"collection":"event","id":"bound-306","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-307',1356,'a3359957f11614ab72873653bcbe1c64945fea7dcc819e3c6761e78cf21d4ac9','{"collection":"event","id":"bound-307","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-308',1361,'e8fab03835f46a0ead6eab51c65ed4d55cc906b77285fc056535d608b7032202','{"collection":"event","id":"bound-308","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-309',1366,'802cb399d74e4fff6f453a0299fbdfa64b0bf080ff0250064546e964585652ea','{"collection":"event","id":"bound-309","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-310',1371,'1a961a4bee84a3eed1cedb4bb66261b9dd29d7a199676f10dacdf8ad6d40df2e','{"collection":"event","id":"bound-310","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-311',1376,'c2c442bd633547465440876d2da2f538e807f23569453680c7d6ad0a5f317c3d','{"collection":"event","id":"bound-311","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-312',1381,'0cf28cd8b2b57895eb138dd850507a985f9bad2f914208d4da76a0fe2f380477','{"collection":"event","id":"bound-312","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-313',1386,'d5e781941773ab286bed38a7331de5aa2d2c1596897d18cfcbb40c0d9ea3bdf0','{"collection":"event","id":"bound-313","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-314',1391,'b6a7245545f4e5c36c7ecbb835c26cb5b05e201d6f08ca530fab74a11d546a72','{"collection":"event","id":"bound-314","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-315',1396,'c6933560ae7d7b0cb345acea70b497fe62402bac93e15017fb5251eb3e257618','{"collection":"event","id":"bound-315","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-316',1401,'73f92e897deadd408421bd4b8656dc57991f51f38114f578e207fd7de7bdf340','{"collection":"event","id":"bound-316","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-317',1406,'6fc2d52718dd5c08824b9b2b937bfe1623166d957d9ac7b3d3a9a9d1ddeced7c','{"collection":"event","id":"bound-317","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-318',1411,'9a91e8ded618d56c8d1ea202ed58279f2bcecee4e771b3dacd3e9048d6245d07','{"collection":"event","id":"bound-318","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-319',1416,'8deaed533a7587dc62c67ddc9ac67005efc5ade7fa0bdbdead19a7ed8eba181a','{"collection":"event","id":"bound-319","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-320',1421,'a3d15b60d1744d30291e67cf5d62694ca427dbda2d0547e191be7740f2d7d701','{"collection":"event","id":"bound-320","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-321',1426,'beef1dd9229d3277ec9b2dc745f89c502952013a1b42be89ef5c609ad8cfd196','{"collection":"event","id":"bound-321","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-322',1431,'b0ea8c5924322c567a68b8f89af4355cf9b3c01f5b675752f384352d12c3dd77','{"collection":"event","id":"bound-322","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-323',1436,'cd2bba08299ad8c79712156558e98f97c7256c77661f7a070a739fe65ecb83b5','{"collection":"event","id":"bound-323","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-324',1441,'d68ddde1f11b8240ec87b48191e7c791cfc48906c3eeda1a06c5ce07a7c79d57','{"collection":"event","id":"bound-324","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-325',1446,'bc44907016c8113ab2288842086482a251f44421c4b7651669eb2e0455afb245','{"collection":"event","id":"bound-325","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-326',1451,'505de8f4cb50d11ad1b429a6b36e25bb4d76e676ce1d490f4be0231f42200b74','{"collection":"event","id":"bound-326","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-327',1456,'78cd57486371c70793ec4166c8480183e50188f22f789975a9fb60f8d0fe2556','{"collection":"event","id":"bound-327","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-328',1461,'79193bcf8ed50331010f4b190077fe69264abe3f054455a6335bd9e62bced2c7','{"collection":"event","id":"bound-328","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-329',1466,'f900dd4b90c6070ed96f8a2e6ce2de9955d57625d749c0defb5031642fa642b2','{"collection":"event","id":"bound-329","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-330',1471,'86170de46f3d529705955a862d191ef82c7f142ff8826f634aa78117ec305b3b','{"collection":"event","id":"bound-330","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-331',1476,'06865a949bfcc309c4604514a11534c2e93b138aac24e86f1c91b989d5085bcc','{"collection":"event","id":"bound-331","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-332',1481,'3e4b27211b1116bfa4881dcf1c6ce479889e8e582f844177ec39c774e7029703','{"collection":"event","id":"bound-332","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-333',1486,'f56952b614cc809315a9b5da4dfab35e69163c1fbed885e4af15b9af2d938d58','{"collection":"event","id":"bound-333","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-334',1491,'7e30e6f99de809b59d578186c74177c19db5ebf0d0e4315efcc92a0f8ce0860d','{"collection":"event","id":"bound-334","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-335',1496,'98bc41ca137f7602b4948ef1b8d89d7f73202b510a2df44fe8db76e4d3b9e4d5','{"collection":"event","id":"bound-335","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-336',1501,'819713b2ddcc3a66f3869cd1746bdcabd7145edb3d168167e4e95ecd741c5459','{"collection":"event","id":"bound-336","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-337',1506,'1be5b1a984af249be21266efbc834c98f525286333aa37fae09b6152db18136a','{"collection":"event","id":"bound-337","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-338',1511,'f7a4980f2e14f22092e6e3199e5bbe08c917e05b1525b0efdd8d475fb957dd90','{"collection":"event","id":"bound-338","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-339',1516,'dfb84ca037a6b5b8d877a9c7ed2a88cd50ef9602ead117a8f3824b50d26b3334','{"collection":"event","id":"bound-339","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-340',1521,'66703a0c614f21dff2f6a6c4565c78440b3520627da50e23c2023a644bd6f26d','{"collection":"event","id":"bound-340","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-341',1526,'74077e1549700aece4cce594183b45877447c4c4e16759fd394bbd7543b4e1b1','{"collection":"event","id":"bound-341","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-342',1531,'215cd9571098d19f738db1da1c8fa433a58b7a97adc302152064deb9d675a06f','{"collection":"event","id":"bound-342","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-343',1536,'9ae19c5a13ffc9c594ff4523133ed16ffc4ecb0dc2be43d45ea7a376ab568525','{"collection":"event","id":"bound-343","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-344',1541,'f23a018ca882a7dc66c20caad781dcdfb1fdd9410a9d982277f7b72c921a76da','{"collection":"event","id":"bound-344","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-345',1546,'73864a27bc22bcd7dda8000da3c87d860b785f114fda9b8128cbc2c07a6dbed6','{"collection":"event","id":"bound-345","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-346',1551,'02819c732ca2c00ec98999727212015cd68946ac298445ce519552b35ce43399','{"collection":"event","id":"bound-346","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-347',1556,'03df61f36334b7f7e1d2b9a34b3932a65541db371efdad2aa368f5a5f38cbbc5','{"collection":"event","id":"bound-347","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-348',1561,'9f41201047a3212e67f64681f17aac8fe2899494a3a40635bb6680e8b9f44aeb','{"collection":"event","id":"bound-348","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-349',1566,'fde83156eeceac2b0f7f661839ebba98d2bb59fe7b9b3de667ff024369febd11','{"collection":"event","id":"bound-349","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-350',1571,'50ebe9d5b82a17332a5d9883ac4b141146bc1464d6b740bbf4b215d0b90ced50','{"collection":"event","id":"bound-350","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-351',1576,'8da9925e27280d70a83d751527d433bab80eef11699f88a69a845e163c33bf40','{"collection":"event","id":"bound-351","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-352',1581,'894679dd2b2d81b2bc15e6ca1d9fe4acd1161e46b985620ecc04cd637e4889a3','{"collection":"event","id":"bound-352","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-353',1586,'7b1a7f964b5f51204492dfa9606beffc8b20eef65c3e2fb793a69578251c8aa0','{"collection":"event","id":"bound-353","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-354',1591,'0e19de6b46e7dc20e459ed8a5d68ac87058a88b69cab5ff036250f62ad31a859','{"collection":"event","id":"bound-354","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-355',1596,'84e72f3d95307878c5be286aae335c1198acfc44a665075516a8cbbbeb36321a','{"collection":"event","id":"bound-355","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-356',1601,'3005e01eb0d4106396ce679fbcf0607f3c880de2b332986eced3b6c04d8bdbff','{"collection":"event","id":"bound-356","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-357',1606,'64307f0e915168b679d91a3d22c6e4e0ad7e00115ecc6b0e6c5740996a8238a8','{"collection":"event","id":"bound-357","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-358',1611,'e1cc8f54eabf19c3f2fdf9fa38cfa9b968583ee3603c46addb0fb937d5c2ec27','{"collection":"event","id":"bound-358","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-359',1616,'e61b5182f80f6021a57d5db2c3f3e9a1bb2ce06a8327ea7636ad22fb288f029c','{"collection":"event","id":"bound-359","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-360',1621,'96d5f6691310cf7d14e5e8d9e6e9b83fc2d0807aff1284b1955c10b71def6844','{"collection":"event","id":"bound-360","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-361',1626,'c75e27acdffa101b1d9dd16aa6b4c0ec2af952e14bddf93b76384ae081e5d295','{"collection":"event","id":"bound-361","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-362',1631,'8ef53bf9670ea419d1fa7b7cffc8fed76f42733e5e45026118d4fd6d75e1971b','{"collection":"event","id":"bound-362","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-363',1636,'a0e983b857f7f42241e15887b05f7450e0db2010c20e779318b71baccaaeaec8','{"collection":"event","id":"bound-363","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-364',1641,'0ad6aa049a7f39e1bb0af75b6d3f359b89364cc6798cab450005febae5f394c6','{"collection":"event","id":"bound-364","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-365',1646,'cab33d164e7e9db6fd2bd6c93379fb37cfdf437094a85a97f6273c7a7397eb5b','{"collection":"event","id":"bound-365","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-366',1651,'5d7478628f3169334c2a99624db5175386107298ca6828220aaef96ba017b437','{"collection":"event","id":"bound-366","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-367',1656,'609c962d5256df695636365f9f0e00cecb4978790757c15f7f29502c38f2c7f8','{"collection":"event","id":"bound-367","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-368',1661,'f447e042b72a70a3fbbc6ea80efff0b31acca276e557724ee6fd9c4df41ab9a0','{"collection":"event","id":"bound-368","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-369',1666,'aa78e07146e091b3126baf3d99b57a34ccefac583bc1494c513d1681df560eaa','{"collection":"event","id":"bound-369","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-370',1671,'130fed9cf5ea29e587c3394a7018c95b53c99cf641ff2aa18d44f158d75476df','{"collection":"event","id":"bound-370","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-371',1676,'da1975c177c2fae3e490f423b49f96f6bceb9803dbe61c444281244d9edd9195','{"collection":"event","id":"bound-371","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-372',1681,'1fa11339c3acf8ea179bb86005a8da6078e99658463bde54b18420a149fee5a6','{"collection":"event","id":"bound-372","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-373',1686,'681ccb9f1d8f364ef0374000e065a27f0ff32d411010e8772a747914fb84d2c4','{"collection":"event","id":"bound-373","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-374',1691,'8a7018166eb8a7dc1eda8f7adb0782c4812328034d884845e366786403f8ea75','{"collection":"event","id":"bound-374","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-375',1696,'7d7848a626a1b119fa6c121810e9924da81e3f4392a589b03e91c84e80566e6f','{"collection":"event","id":"bound-375","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-376',1701,'255c34dfb8260233f4ad0d361a45a89423162aaab75efec3befe319380e8b96c','{"collection":"event","id":"bound-376","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-377',1706,'a9bebad640f8f7eeb11cdfec2e3f3275621371ebe885ad5d86302b6f0b094c28','{"collection":"event","id":"bound-377","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-378',1711,'ac83b270179e90dcd8cc8ef09792e1aa3d000dcd13194a063af93c056dd833b8','{"collection":"event","id":"bound-378","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-379',1716,'41bfa77876dffb40a0480759d0ab9c31d7e66f36098355d15618734f690ea627','{"collection":"event","id":"bound-379","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-380',1721,'d81f8b3737f731d09b086dc35ffb01a9ae440c3f3f96e9a96f1db45a4c83f648','{"collection":"event","id":"bound-380","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-381',1726,'50f2e59bdc4374315786595e74f6f3853898e0ff1fbfea9fc09da9371fc50e1d','{"collection":"event","id":"bound-381","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-382',1731,'ef440cb76075cdb544fd703e20cc7390a70bb9fd88192cab3d052f9d76abab6b','{"collection":"event","id":"bound-382","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-383',1736,'beefc3bc027ab5626780a70743a7ccc164dc1a75e11ce1158603e643b2108037','{"collection":"event","id":"bound-383","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-384',1741,'f98ec3c7c8813034bc0556971ac1468d1b1651e664e2e4a6325aa7d507d6b67a','{"collection":"event","id":"bound-384","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-385',1746,'d1f44cded5cd904d150a0182daf1d168410c477abe125a14eb4afd5d4c9b847a','{"collection":"event","id":"bound-385","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-386',1751,'1f802f760c8937e8299914c1b6e1ed2bad5ab93f658c9e26eccadacf04a4dade','{"collection":"event","id":"bound-386","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-387',1756,'0ad97c7180af6e1993cbc2f0d9711b679cd28c203aade09e76b9d297ce80dcc1','{"collection":"event","id":"bound-387","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-388',1761,'9450dde235ac4390850f635681594fe6c7d4e4e3703434790a7a369910af7d9d','{"collection":"event","id":"bound-388","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-389',1766,'65a34f06be8262a0147dfab1aa2c14a66f73d658da7e308febdac95bb6cde677','{"collection":"event","id":"bound-389","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-390',1771,'e2cbeaaa137ff75e4d6411a0214484a1592758089d6026c60aaf11984632f003','{"collection":"event","id":"bound-390","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-391',1776,'a513e4f136716786e334fa7892c066a0e0ba5f42e2881fa3c1b327b6826387a3','{"collection":"event","id":"bound-391","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-392',1781,'7aa1116092ea0370d4b12c2b12cd10ec484436aa8b48d5a45d8569f1bc06a4ba','{"collection":"event","id":"bound-392","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-393',1786,'9e4bbf399fb75d74073a75315bd659c99e77ad3b5c4f1fc7f3e8f987e6e31d0e','{"collection":"event","id":"bound-393","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-394',1791,'1b8523a8409743e1d3e8ea2882d9f8cd8f59445e6c5ba0b87cf9f38861647ba2','{"collection":"event","id":"bound-394","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-395',1796,'4670d66b3ac444d93d0409a64ccfbcd5350d404ff995e4ebfd77860ac289b39c','{"collection":"event","id":"bound-395","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-396',1801,'ecd03a1a66dad6015f5c52197084c4219dd83c3729c2834c81a2275a9414590f','{"collection":"event","id":"bound-396","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-397',1806,'52307744b6995645ef5e27089da906d7c294195109cf919e308da1e1d91619da','{"collection":"event","id":"bound-397","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-398',1811,'0800f7ae7be6bafa5943de0573c090767b393a8acd6b74cbf1de3e702727c1ff','{"collection":"event","id":"bound-398","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-399',1816,'812fb1f001a0d49f04048e78d73c1668b0a52df38b26f064e57d9d5e5e85c175','{"collection":"event","id":"bound-399","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-400',1821,'3ca3e1d736da092447e0e3406787e96fb0894c48b8316977af15bbfb7562269a','{"collection":"event","id":"bound-400","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-401',1826,'a34bb76fda64574d470dbdc46512779195b7621cfeb7c83bac4c4281955d4630','{"collection":"event","id":"bound-401","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-402',1831,'240f5df5d674abef7b710959e77504b152fa919f1dc2435bcc5e381ebd508c5c','{"collection":"event","id":"bound-402","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-403',1836,'1fba83c0511dee19cd60e6f3d87d9e811b4b421ac0cc98a602b0ce63f870540d','{"collection":"event","id":"bound-403","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-404',1841,'561b24f66c9b94f72aacaf9d314b883d1ab750edc947507e8d5c646c4ef66675','{"collection":"event","id":"bound-404","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-405',1846,'c0c6d9e962a8d8aaac3980a9b60a5f335754df8cce36789a6fd50a5ff2365444','{"collection":"event","id":"bound-405","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-406',1851,'060f2132cf33b01989554a23d961ac04ac7f5dbb108cc96e4166266b299c6d0e','{"collection":"event","id":"bound-406","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-407',1856,'48671588ee7c74637df09606baad98205ed25fbddbcb52e816a20829d9c5afd8','{"collection":"event","id":"bound-407","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-408',1861,'0185771e9cc63937f9dc519581abf57ce0d9117e1a79ed71f1cf41b4fc94e070','{"collection":"event","id":"bound-408","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-409',1866,'c5bf77a38ad593b6df8bed27496779aa7b5caf967b9368db19b5430f9a8e5c94','{"collection":"event","id":"bound-409","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-410',1871,'4b302a41a2a6b9a082cdf17f41271ebae7bfc208a532b301ccceab0cb0c4f969','{"collection":"event","id":"bound-410","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-411',1876,'0f1ea7de5423c103e7555eb782ffd40fde0cf5d563f2b78dbf6b781a8011dc7c','{"collection":"event","id":"bound-411","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-412',1881,'1841308130aba3038de0204d0d3052430e553b01ef9d7fde00de269092198b39','{"collection":"event","id":"bound-412","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-413',1886,'6947acf840645c7cb93a557f82d706454a43ddbc1629fb8489882f8b33b6fec7','{"collection":"event","id":"bound-413","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-414',1891,'5bb6ded443dd03f1d5e061372c7d01d01f2e83155ec54a9fbe818fb553d38b17','{"collection":"event","id":"bound-414","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-415',1896,'093434c9eeca4e55dc0b3fd85dc8ef2053d35717906dd439bfe603ba5fd5d709','{"collection":"event","id":"bound-415","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-416',1901,'84b671e12d5f0379c8695164d944deb242d17a3da42c1ccba718d7d40b3c1612','{"collection":"event","id":"bound-416","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-417',1906,'cc283dc294f3b32bc8c8040b58939caa826a673dcbcb99a62d735fdc1b547147','{"collection":"event","id":"bound-417","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-418',1911,'cf2e4c1dbc31c84c4e46aa6b5fc2ae9a3d297d8eef74c18c0563345e1178f499','{"collection":"event","id":"bound-418","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-419',1916,'1f7cfef5f20d4d69fa555343ab2be58782a2d742ec915c8b99f6d63af971a949','{"collection":"event","id":"bound-419","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-420',1921,'ebed14e5ba9665bd32beba5e5392fb2c23c191e7bf79632692139327b81e5ca8','{"collection":"event","id":"bound-420","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-421',1926,'c04c24a08f390930268dcc9b274b064307a9559e9071e058924c2bf0aa1eae0b','{"collection":"event","id":"bound-421","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-422',1931,'e8475bfa9bc9ae54728d64dca25a83532e31adb4c21a77d2daeba883ee26ac56','{"collection":"event","id":"bound-422","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-423',1936,'9b0708b56ec1a08f11ddcf41f05bebc55ae28b73fdeb0e59cdd10da2569aae9f','{"collection":"event","id":"bound-423","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-424',1941,'99482c0574fc81871d9fcd0132ddc1bbd141be482da9b4290bf1d0e46ae83845','{"collection":"event","id":"bound-424","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-425',1946,'1a3e2e9ec5edda8b4998d6bcc05bfa8872a0e76031bf5e068ba5c1f420283290','{"collection":"event","id":"bound-425","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-426',1951,'4da18e3ee0ae5f00f21115c26ffe28c0c91ebab8fb462035d278b2148e9db4bb','{"collection":"event","id":"bound-426","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-427',1956,'f4a7e510fec6e930d99c09b6654ba5c1eb042c58fd35d6aba582e7b65e6f7052','{"collection":"event","id":"bound-427","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-428',1961,'c8336140fe5cd37f4f4afc1082dcd7c7835b1acea76bbc71b642eaa8d0763971','{"collection":"event","id":"bound-428","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-429',1966,'16e2c8b509648c1554a3b6089cc5d7d8b8afbd6023d17ef04802c1ea48fd4082','{"collection":"event","id":"bound-429","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-430',1971,'35f943502531d23a88f987c254c73372d007ebdf9bcb49a20a23a258eea9b448','{"collection":"event","id":"bound-430","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-431',1976,'0a46d7b313826e1d9d007118dc73f50181833a0f188e3b0a5166ca6e40fe9602','{"collection":"event","id":"bound-431","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-432',1981,'5e37ac29c57bfcaeea6882db69bc97ead90f4451f8aca9930072d4ce2864121b','{"collection":"event","id":"bound-432","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-433',1986,'9b827d1c3867786ad6a35f63190912a79bafda954d14f85d9dd3cc4b89fc5a86','{"collection":"event","id":"bound-433","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-434',1991,'f0b043699f2e8c9bf069d9664372becea99f5c3b1898f39261d4bbe9195f7d65','{"collection":"event","id":"bound-434","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-435',1996,'4e19c0fa13a8c77ff14d43ececedd23fc1a374bbbbdde18c747d078e238da1e5','{"collection":"event","id":"bound-435","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-436',2001,'9bd1afffeec4b044a9e92a2dc686d158860345b4cc5ea53dcfa9c58092893178','{"collection":"event","id":"bound-436","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-437',2006,'7f779c1c20b6b408db59457b21a0069c12faf37946f27406fcdcc17e8b889780','{"collection":"event","id":"bound-437","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-438',2011,'57c29d1e9128b841cc218d826e27ce8ca21f2c1afce35234e497729484fd4e9d','{"collection":"event","id":"bound-438","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-439',2016,'a05c1d9779fe7f5df249ecb184dd7c1b6181afe43a2d2235fd00df42c7ae67f7','{"collection":"event","id":"bound-439","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-440',2021,'3400b66150575ad47b51274d3632ae22be45d2e7eed3c7ebac5c057d07172978','{"collection":"event","id":"bound-440","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-441',2026,'3ad1bd98c221ce11954aa04b37b169325d137c2747db1e8ac711e5c9e5eba144','{"collection":"event","id":"bound-441","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-442',2031,'1dc20630996333becb58196ed7e43b413abb2450c253750b172ba517c984e8bc','{"collection":"event","id":"bound-442","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-443',2036,'9928f1bdee541866a41c1a242c1194e8d5e82deb13830ff44e6a087748b413e8','{"collection":"event","id":"bound-443","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-444',2041,'c27a264437d9aaf7e84a05578e01c0c022f1343b099b5173063bc2e3d5abfa8c','{"collection":"event","id":"bound-444","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-445',2046,'28a347cfbe118f897f47034fd3ff27668577179866eecac58091b19e30462e8a','{"collection":"event","id":"bound-445","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-446',2051,'a482827e3aaf6f82fe9eb6bef446a15605cdf074e6ce3cb69831546d18824b93','{"collection":"event","id":"bound-446","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-447',2056,'f82476140a04349f54ab5c9f4e9f1f7fa7161fa3517caeba174177d02242829f','{"collection":"event","id":"bound-447","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-448',2061,'628000ec22abb64b01850f73016e4684a80ec7673b83047b851038eeedf0f449','{"collection":"event","id":"bound-448","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-449',2066,'97ec494373617ed24492e076d212aec7433de7b1bd26df7122855ad9e863b598','{"collection":"event","id":"bound-449","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-450',2071,'7a63b1c28f08245755526ffc59d4ffc44412c13221356134e64b0c6aab10b1cd','{"collection":"event","id":"bound-450","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-451',2076,'94d7947a1bb1e848f3632e3decd9540db60dfb046969da96079092670cad1389','{"collection":"event","id":"bound-451","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-452',2081,'1f0a5a634b2911b21b81f6078b9fd02bfbede3d835b519e3c594aa2895d9d5e1','{"collection":"event","id":"bound-452","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-453',2086,'33fdd439979ee59d63227e8f8e9d7bd19cd05a7bd040a6c7edcba0a6b1556b2d','{"collection":"event","id":"bound-453","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-454',2091,'b05e60c828d5525702cfa9f6389bb579143c486041ea94ecce0bd4d3fbff9b3a','{"collection":"event","id":"bound-454","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-455',2096,'4c79eb481e436648b1371b085918767466c62036d50a362310d04b98a793fad5','{"collection":"event","id":"bound-455","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-456',2101,'32904c5a90cd9e14211db5a1abf70cbe7c4e0fbb19aba5b647ef55942ad973ee','{"collection":"event","id":"bound-456","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-457',2106,'3b2ca42da08b705d5fc8020d9f4f0b2d52b297c8764ab362039530efb6a1463a','{"collection":"event","id":"bound-457","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-458',2111,'80850baff9e669a2b8ef6bda6c2b9a4e69030b92bde3d947e7d2837e2739cdce','{"collection":"event","id":"bound-458","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-459',2116,'5cfb368b76fd023b904364a683f324b4856c1b343fcd9c5ba2fb2c6fd4a0025f','{"collection":"event","id":"bound-459","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-460',2121,'d5044544feb91df43e38d6d7823165a8dd82f96cdcb94aabad2b88b41591921c','{"collection":"event","id":"bound-460","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-461',2126,'799718b80c776c10e44f6025a152d8b7f921de5aecdcdc6fa3afa04139ae5942','{"collection":"event","id":"bound-461","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-462',2131,'30ca68d5b950651290b93c33aecd6119bf9c4801d66836414a380943c10e3697','{"collection":"event","id":"bound-462","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-463',2136,'e6feb0b32fe1d177007e705d40fce50f49ee8cd2f95b62b5b72070807a7355a7','{"collection":"event","id":"bound-463","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-464',2141,'74facbb67e6ba66e1efaaf1bd34d8e202930187067f916db17f98fa1aba4550c','{"collection":"event","id":"bound-464","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-465',2146,'2d236f788cdf899e6b9daca19de0bba2a89fd8fab7ebf9af71891da3c9bb895c','{"collection":"event","id":"bound-465","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-466',2151,'08b855ef457683868ea5a759af7b4f97e543c08fcdc03015bdce5b07a4c0b2f6','{"collection":"event","id":"bound-466","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-467',2156,'38674cee3a04b2243e9cef057225936256e00f67628d3f9e7ca6682e50ca0169','{"collection":"event","id":"bound-467","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-468',2161,'6530a156a503a09316ec2a28b05c366a05856b4e3cbe176675f59e872027917e','{"collection":"event","id":"bound-468","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-469',2166,'158692246535445b370ee92712b528642fe61d702d452f79152decc9c91a5a5f','{"collection":"event","id":"bound-469","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-470',2171,'96b58a4de3cc608e798c42fe0ff13618d370af8ec1ba005dec5878ea67cabee1','{"collection":"event","id":"bound-470","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-471',2176,'eae626aef6dfd2347d25d61fc8523fd544a199696c96aabb434bbecc606cf5e8','{"collection":"event","id":"bound-471","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-472',2181,'a76b111adb94221763d80cef3b217e9fe61e35d114e1ee77e9b9f45ca1a81c84','{"collection":"event","id":"bound-472","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-473',2186,'fe5a4a654de8728f060a9593d27a9ba89948039b0ddcf1d421a378ae4a8d4153','{"collection":"event","id":"bound-473","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-474',2191,'3e4b81d5ed471b4d1648caa83b401bbdcc0db2418cbc7c2993c3fb2a47ea5705','{"collection":"event","id":"bound-474","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-475',2196,'e51b3cec42d008e8e5156f38724a38297a72869ee50dbb7395a784ac9c06af35','{"collection":"event","id":"bound-475","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-476',2201,'e40bd4659577c9f961910b6c9142543b30347f5e3415b6ef6b7e626784ea096f','{"collection":"event","id":"bound-476","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-477',2206,'f6ab0bed965715e7bb6fd98ccdc2e2a4d94e4de328d8894afe5696c38c5e5c8b','{"collection":"event","id":"bound-477","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-478',2211,'284d5f577fd0583af9bf1764ba520bbe8ea74a633353d8b7b8fdd662f043ae24','{"collection":"event","id":"bound-478","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-479',2216,'8a5f379a6938c6a4f8d5e90e13f7f0077cdf0ec84a89e40b370a2b2a50222ca2','{"collection":"event","id":"bound-479","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-480',2221,'a2ce546531dae8b885968431eb162230478ce0041d65d8df0a287293b8ce3b45','{"collection":"event","id":"bound-480","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-481',2226,'9a4ba57042532e1efde1904ad7eb6750bd2bc3285198b0240b3b6bc4ee8258c4','{"collection":"event","id":"bound-481","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-482',2231,'338d2022debb92b4c23bfc93065d7f7ca79892680acf7327b842eb90eb1d9058','{"collection":"event","id":"bound-482","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-483',2236,'9d645869c5db8cab9bc256b2878b9724b4ea3188967997138421e6c0e1effba7','{"collection":"event","id":"bound-483","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-484',2241,'d8be58028857cffc609430ffbcf113c04f1b66d5ce277b3b9164c17403dbb865','{"collection":"event","id":"bound-484","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-485',2246,'15bd38c9ca3f653dc820e4ebe9e3b3640adffcdfff157c50a42b7c612020b618','{"collection":"event","id":"bound-485","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-486',2251,'e09335f58aad23c12ec1671aaf9e547aeb82a9c96c041b0a38683f2042932e12','{"collection":"event","id":"bound-486","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-487',2256,'a39c508f06358d78e138f11224790c5f3d4d7f821a56f74edc339cd9a4c1a3c5','{"collection":"event","id":"bound-487","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-488',2261,'2047a2c3bab18c504991e001d2c650b01d7a75c6ceedf760ab42f3f7797d9451','{"collection":"event","id":"bound-488","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-489',2266,'5b10cadbddd1a488589eedae46ef5998ef474d1dc5c516f2a3f438296dac2305','{"collection":"event","id":"bound-489","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-490',2271,'51c4b1de9b7cf6ef92447e4e949a874b9f719eb2bb14f680d139c0084e5d2aac','{"collection":"event","id":"bound-490","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-491',2276,'d46037ba54268a5054db7375a02b8aac5650b323db5c5ef8a579d8606e7c5b60','{"collection":"event","id":"bound-491","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-492',2281,'b382ae9262239714a89cea57afbdac81d563ab96bd521c292284d21392b9dbce','{"collection":"event","id":"bound-492","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-493',2286,'b79e7d82aa9b303d6d9b9a56db70b1d2ae9955a3e09293b115639392998fb981','{"collection":"event","id":"bound-493","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-494',2291,'8b7316af603bb7a79ce022feefab5d25ff89138e16beb3132dbc233262ce782d','{"collection":"event","id":"bound-494","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-495',2296,'86ef53d3527dea8fbd26565bf04ecef3a640b91d57cc0a32d23383aa9a000692','{"collection":"event","id":"bound-495","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-496',2301,'04f390c06b4fae0e90bb7940673dc0d69235cbb402749198e16ab7f65ee0ed73','{"collection":"event","id":"bound-496","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-497',2306,'a1c712b5cc359f9d0a80d54d3b68c0b61d59467feb2ebed8777129ec3b6a87c8','{"collection":"event","id":"bound-497","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-498',2311,'1628dfd8802464ef5a3171cac4ecded1174594911e2435038bd0b2cc4ede529e','{"collection":"event","id":"bound-498","value":{}}',1);
INSERT INTO "sync_entities" VALUES('event','bound-over',2312,'2be720924cd722455ae670171970068b822b96d43ef34dccdf61f1d7441c739e','{"collection":"event","id":"bound-over","value":{"agent":"agent","created":1792689640.9669857,"error":null,"id":"bound-over","kind":"user","status":"pending"}}',0);
CREATE TABLE sync_entity_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
INSERT INTO "sync_entity_meta" VALUES('entity_tombstone_count','304');
INSERT INTO "sync_entity_meta" VALUES('task_window_migrated','1');
INSERT INTO "sync_entity_meta" VALUES('event_window_seq','2313');
INSERT INTO "sync_entity_meta" VALUES('seeded','1');
INSERT INTO "sync_entity_meta" VALUES('agent_organization_fields','2');
CREATE TABLE sync_generation (id INTEGER PRIMARY KEY, value INTEGER NOT NULL);
INSERT INTO "sync_generation" VALUES(1,0);
CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
INSERT INTO "sync_identity" VALUES('6f99fae733fe413c97db00d16663fa9d');
CREATE TABLE sync_versions (
        seq INTEGER PRIMARY KEY, scope TEXT NOT NULL UNIQUE,
        hash TEXT NOT NULL, deleted INTEGER NOT NULL, updated REAL NOT NULL);
CREATE INDEX sync_entities_seq ON sync_entities(seq);
CREATE INDEX sync_entities_collection_seq ON sync_entities(collection,seq);
CREATE INDEX sync_entities_non_transcript_seq ON sync_entities(seq)
        WHERE collection NOT LIKE 'transcript:%';
CREATE INDEX sync_entities_collection_deleted
        ON sync_entities(collection,deleted);
CREATE INDEX sync_entities_tombstone_order
        ON sync_entities(seq,collection,id)
        WHERE deleted=1 AND collection NOT LIKE 'transcript:%';
CREATE INDEX sync_scope_seq ON sync_documents(scope,seq);
CREATE TRIGGER sync_entity_tombstone_count_insert
      AFTER INSERT ON sync_entities
      WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+1
        WHERE key='entity_tombstone_count';
      END;
CREATE TRIGGER sync_entity_tombstone_count_update
      AFTER UPDATE OF deleted,collection ON sync_entities
      WHEN (OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%') !=
           (NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%') BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)+
          CASE WHEN NEW.deleted=1 AND NEW.collection NOT LIKE 'transcript:%' THEN 1 ELSE -1 END
        WHERE key='entity_tombstone_count';
      END;
CREATE TRIGGER sync_entity_tombstone_count_delete
      AFTER DELETE ON sync_entities
      WHEN OLD.deleted=1 AND OLD.collection NOT LIKE 'transcript:%' BEGIN
        UPDATE sync_entity_meta SET value=CAST(value AS INTEGER)-1
        WHERE key='entity_tombstone_count';
      END;
CREATE INDEX runtime_server_outbox_due
                    ON runtime_server_outbox(state,next_at);
CREATE INDEX runtime_server_links_home_parent
                    ON runtime_server_links(json_extract(record,'$.parent'))
                    WHERE json_extract(record,'$.side')='home';
CREATE INDEX runtime_server_output_tools_expiry
                    ON runtime_server_output_tools(expires);
CREATE INDEX runtime_agent_native_scope ON runtime_agents(
                    json_extract(record,'$.threadId'),
                    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
                         ELSE json_extract(record,'$.accountKey') END);
CREATE INDEX runtime_agent_native_reset_pending ON runtime_agents(id)
                    WHERE json_extract(record,'$.nativeRelease.resetPending')=1;
CREATE INDEX runtime_agent_account_scope ON runtime_agents(
                    CASE WHEN json_type(record,'$.accountKey') IS NULL THEN 'default'
                         ELSE json_extract(record,'$.accountKey') END);
CREATE INDEX runtime_agent_root ON runtime_agents(
                    json_extract(record,'$.rootId'));
CREATE INDEX runtime_agent_live_root ON runtime_agents(
                    json_extract(record,'$.rootId'),id)
                    WHERE json_extract(record,'$.deletedAt') IS NULL;
CREATE INDEX runtime_agent_parent ON runtime_agents(
                    json_extract(record,'$.parentId'));
CREATE INDEX runtime_agent_global_active ON runtime_agents(
                    json_extract(record,'$.status')) WHERE json_extract(record,'$.status') IN
                    ('running','starting','approval');
CREATE INDEX runtime_agent_inflight ON runtime_agents(
                    json_extract(record,'$.inFlight')) WHERE json_extract(record,'$.inFlight')=1;
CREATE INDEX runtime_agent_reservation_cwd ON runtime_agents(
                    json_extract(record,'$.cwd'))
                    WHERE json_type(record,'$.workspaceOperation')='text'
                      AND json_extract(record,'$.workspaceOperation')!='';
CREATE INDEX runtime_agent_capacity_retry_state_due_v2 ON runtime_agents(
                    json_extract(record,'$.capacityRetry.status'),
                    json_extract(record,'$.capacityRetry.dueAt'));
CREATE INDEX runtime_usage_resume_agent ON runtime_usage_resumes(agent);
CREATE INDEX runtime_usage_resume_due ON runtime_usage_resumes(json_extract(record,'$.status'),json_extract(record,'$.dueAt'));
CREATE INDEX runtime_usage_resume_account ON runtime_usage_resumes(json_extract(record,'$.accountKey'),json_extract(record,'$.status'));
CREATE INDEX runtime_event_queue ON runtime_events(status, agent, created);
CREATE INDEX runtime_event_repair_candidates_v2 ON runtime_events(agent,created)
                  WHERE status='delivered' AND kind IN ('monitor_exit','agent_message','work_review','work_decision');
CREATE INDEX runtime_event_user_turns ON runtime_events(agent,kind,status,turn_id);
CREATE INDEX runtime_context_manifest_latest
                  ON runtime_context_manifests(agent,sequence DESC);
CREATE INDEX runtime_item_agent ON runtime_items(agent, created);
CREATE INDEX runtime_events_agent_epoch_status_created
                  ON runtime_events(agent,epoch,status,created,id);
CREATE INDEX runtime_task_status ON runtime_tasks(json_extract(record,'$.status'), json_extract(record,'$.created'));
CREATE INDEX runtime_task_history ON runtime_tasks(json_extract(record,'$.created') DESC, json_extract(record,'$.agent')) WHERE json_extract(record,'$.status')!='running';
CREATE INDEX runtime_task_agent_created_id ON runtime_tasks(
                    json_extract(record,'$.agent'), json_extract(record,'$.created') DESC, id DESC);
CREATE INDEX runtime_task_agent_updated_id ON runtime_tasks(
                    json_extract(record,'$.agent'),
                    CASE WHEN COALESCE(json_extract(record,'$.finished'),0) > COALESCE(json_extract(record,'$.created'),0)
                         THEN json_extract(record,'$.finished') ELSE json_extract(record,'$.created') END,
                    id);
CREATE INDEX runtime_monitor_status ON runtime_monitors(json_extract(record,'$.status'),json_extract(record,'$.created'));
CREATE INDEX runtime_monitor_agent_status ON runtime_monitors(
                    json_extract(record,'$.agent'),json_extract(record,'$.status'));
CREATE INDEX runtime_monitor_stall_due ON runtime_monitors(
                    COALESCE(json_extract(record,'$.activityAt'),json_extract(record,'$.created'),1e99)+
                    COALESCE(json_extract(record,'$.stallTimeoutSeconds'),1800))
                    WHERE json_extract(record,'$.status')='running';
CREATE INDEX runtime_request_native_question ON runtime_requests(
                    json_extract(record,'$.agent'),json_extract(record,'$.params.itemId'),
                    json_extract(record,'$.status'))
                    WHERE json_valid(record) AND json_extract(record,'$.method')='item/tool/requestUserInput';
CREATE INDEX runtime_request_native_approval ON runtime_requests(
                    json_extract(record,'$.agent'),json_extract(record,'$.params.itemId'),
                    json_extract(record,'$.status'))
                    WHERE json_valid(record) AND json_extract(record,'$.method') IN
                    ('item/commandExecution/requestApproval','item/fileChange/requestApproval');
CREATE INDEX runtime_chat_room ON runtime_chat_messages(room, seq);
CREATE INDEX runtime_scheduler_agent_change_generation ON runtime_scheduler_agent_changes(generation);
CREATE TRIGGER runtime_agent_record_generation_insert AFTER INSERT ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_update AFTER UPDATE ON runtime_agents WHEN OLD.record IS NOT NEW.record OR OLD.id IS NOT NEW.id OR OLD.rowid IS NOT NEW.rowid BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1 AND OLD.id IS NOT NEW.id; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 AND OLD.id IS NOT NEW.id ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=NEW.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=NEW.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT NEW.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE TRIGGER runtime_agent_record_generation_delete AFTER DELETE ON runtime_agents BEGIN UPDATE runtime_agent_record_generation SET value=value+1 WHERE id=1; UPDATE runtime_scheduler_agent_change_state SET entries=entries+CASE WHEN EXISTS(SELECT 1 FROM runtime_scheduler_agent_changes WHERE id=OLD.id) THEN 0 ELSE 1 END,total=total-COALESCE((SELECT generation FROM runtime_scheduler_agent_changes WHERE id=OLD.id),0)+(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; INSERT INTO runtime_scheduler_agent_changes(id,generation) SELECT OLD.id,value FROM runtime_agent_record_generation WHERE id=1 ON CONFLICT(id) DO UPDATE SET generation=excluded.generation; UPDATE runtime_scheduler_agent_change_state SET generation=(SELECT value FROM runtime_agent_record_generation WHERE id=1) WHERE id=1; END;
CREATE INDEX runtime_turn_item_link_scope_v1 ON runtime_turn_item_links_v1(agent,turn_id,epoch,created,id);
CREATE TRIGGER runtime_turn_item_link_insert_v1 AFTER INSERT ON runtime_items BEGIN INSERT INTO runtime_turn_item_links_v1(id,agent,turn_id,created,epoch) VALUES (NEW.id,NEW.agent,json_extract(NEW.record,'$.turnId'),NEW.created,COALESCE((SELECT epoch FROM runtime_turn_item_link_state_v1 WHERE id=1),'')) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,created=excluded.created,epoch=excluded.epoch WHERE runtime_turn_item_links_v1.agent IS NOT excluded.agent OR runtime_turn_item_links_v1.turn_id IS NOT excluded.turn_id OR runtime_turn_item_links_v1.created IS NOT excluded.created OR runtime_turn_item_links_v1.epoch IS NOT excluded.epoch; END;
CREATE TRIGGER runtime_turn_item_link_update_v1 AFTER UPDATE ON runtime_items WHEN OLD.id IS NOT NEW.id OR OLD.agent IS NOT NEW.agent OR OLD.record IS NOT NEW.record OR OLD.created IS NOT NEW.created OR OLD.rowid IS NOT NEW.rowid BEGIN DELETE FROM runtime_turn_item_links_v1 WHERE id=OLD.id AND OLD.id IS NOT NEW.id; INSERT INTO runtime_turn_item_links_v1(id,agent,turn_id,created,epoch) VALUES (NEW.id,NEW.agent,json_extract(NEW.record,'$.turnId'),NEW.created,COALESCE((SELECT epoch FROM runtime_turn_item_link_state_v1 WHERE id=1),'')) ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,turn_id=excluded.turn_id,created=excluded.created,epoch=excluded.epoch WHERE runtime_turn_item_links_v1.agent IS NOT excluded.agent OR runtime_turn_item_links_v1.turn_id IS NOT excluded.turn_id OR runtime_turn_item_links_v1.created IS NOT excluded.created OR runtime_turn_item_links_v1.epoch IS NOT excluded.epoch; END;
CREATE TRIGGER runtime_turn_item_link_delete_v1 AFTER DELETE ON runtime_items BEGIN DELETE FROM runtime_turn_item_links_v1 WHERE id=OLD.id;  END;
CREATE INDEX runtime_execution_run_agent ON runtime_execution_runs(agent,created DESC);
CREATE INDEX runtime_execution_run_created ON runtime_execution_runs(created DESC);
CREATE INDEX runtime_execution_run_message ON runtime_execution_runs(agent,thread,turn);
CREATE INDEX runtime_execution_run_native ON runtime_execution_runs(agent,account,thread,turn);
CREATE INDEX runtime_execution_attempt_run ON runtime_execution_attempts(run);
CREATE INDEX runtime_execution_input_event ON runtime_execution_inputs(event);
CREATE INDEX runtime_execution_effect_run ON runtime_execution_effects(run);
CREATE INDEX runtime_execution_node_run ON runtime_execution_nodes(run);
CREATE INDEX runtime_federation_outbox_peer
          ON runtime_federation_outbox(peer_id);
CREATE INDEX runtime_federation_nonce_age
          ON runtime_federation_nonces(created);
CREATE INDEX runtime_event_created_id ON runtime_events(created DESC,id);
CREATE TRIGGER sync_entity_event_INSERT
          AFTER INSERT ON runtime_events  BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
               sync_entity_hash(sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0)),sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0),0
        FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200) e
        WHERE 0 OR NOT EXISTS (SELECT 1 FROM sync_entities s
            WHERE s.collection='event' AND s.id=e.id AND s.deleted=0)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',s.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY s.id),
               sync_entity_hash(sync_entity_payload('event',s.id,'{}',1)),sync_entity_payload('event',s.id,'{}',1),1
        FROM sync_entities s
        WHERE s.collection='event' AND s.deleted=0
          AND s.id NOT IN (SELECT id FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200))
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE TRIGGER sync_entity_event_UPDATE
          AFTER UPDATE OF id,agent,kind,status,created,error ON runtime_events WHEN (OLD.id IS NOT NEW.id OR OLD.agent IS NOT NEW.agent OR OLD.kind IS NOT NEW.kind OR OLD.status IS NOT NEW.status OR OLD.created IS NOT NEW.created OR OLD.error IS NOT NEW.error) AND NOT (CASE
        WHEN OLD.id IS NEW.id AND OLD.created IS NEW.created THEN 0
        ELSE (WITH old_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM (
        SELECT id,created FROM runtime_events WHERE id!=NEW.id
        UNION ALL SELECT OLD.id,OLD.created
    ) ORDER BY created DESC,id LIMIT 200) e WHERE e.id=OLD.id) AS present),
                   new_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM runtime_events ORDER BY created DESC,id LIMIT 200) e WHERE e.id=NEW.id) AS present)
              SELECT CASE WHEN OLD.id IS NEW.id
                          THEN old_member.present != new_member.present
                          ELSE old_member.present OR new_member.present
                     END
              FROM old_member,new_member)
    END) BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
               sync_entity_hash(sync_entity_payload('event',NEW.id,'{}',1)),sync_entity_payload('event',NEW.id,'{}',1),1
        WHERE NOT EXISTS (SELECT 1 FROM (SELECT id FROM runtime_events ORDER BY created DESC,id LIMIT 200) e WHERE e.id=NEW.id)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE TRIGGER sync_entity_event_UPDATE_WINDOW
          AFTER UPDATE OF id,agent,kind,status,created,error ON runtime_events WHEN CASE
        WHEN OLD.id IS NEW.id AND OLD.created IS NEW.created THEN 0
        ELSE (WITH old_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM (
        SELECT id,created FROM runtime_events WHERE id!=NEW.id
        UNION ALL SELECT OLD.id,OLD.created
    ) ORDER BY created DESC,id LIMIT 200) e WHERE e.id=OLD.id) AS present),
                   new_member AS (SELECT EXISTS (SELECT 1 FROM (SELECT id FROM runtime_events ORDER BY created DESC,id LIMIT 200) e WHERE e.id=NEW.id) AS present)
              SELECT CASE WHEN OLD.id IS NEW.id
                          THEN old_member.present != new_member.present
                          ELSE old_member.present OR new_member.present
                     END
              FROM old_member,new_member)
    END BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',NEW.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0)),sync_entity_payload('event',NEW.id,json_object('id',NEW.id,'agent',NEW.agent,'kind',NEW.kind,'status',NEW.status,'created',NEW.created,'error',NEW.error),0),0)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
               sync_entity_hash(sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0)),sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0),0
        FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200) e
        WHERE 0 OR NOT EXISTS (SELECT 1 FROM sync_entities s
            WHERE s.collection='event' AND s.id=e.id AND s.deleted=0)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',s.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY s.id),
               sync_entity_hash(sync_entity_payload('event',s.id,'{}',1)),sync_entity_payload('event',s.id,'{}',1),1
        FROM sync_entities s
        WHERE s.collection='event' AND s.deleted=0
          AND s.id NOT IN (SELECT id FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200))
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE TRIGGER sync_entity_event_DELETE
          AFTER DELETE ON runtime_events  BEGIN
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
          VALUES ('event',OLD.id,(SELECT max(seq)+1 FROM (
                    SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
                    SELECT COALESCE(MAX(seq),0) FROM sync_versions)),
                  sync_entity_hash(sync_entity_payload('event',OLD.id,'{}',1)),sync_entity_payload('event',OLD.id,'{}',1),1)
          ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
          WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',e.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY e.created DESC,e.id),
               sync_entity_hash(sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0)),sync_entity_payload('event',e.id,json_object('id',e.id,'agent',e.agent,'kind',e.kind,'status',e.status,'created',e.created,'error',e.error),0),0
        FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200) e
        WHERE 0 OR NOT EXISTS (SELECT 1 FROM sync_entities s
            WHERE s.collection='event' AND s.id=e.id AND s.deleted=0)
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted)
        SELECT 'event',s.id,(SELECT max(seq) FROM (
        SELECT COALESCE(MAX(seq),0) seq FROM sync_entities UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_documents UNION ALL
        SELECT COALESCE(MAX(seq),0) FROM sync_versions))+ROW_NUMBER() OVER (ORDER BY s.id),
               sync_entity_hash(sync_entity_payload('event',s.id,'{}',1)),sync_entity_payload('event',s.id,'{}',1),1
        FROM sync_entities s
        WHERE s.collection='event' AND s.deleted=0
          AND s.id NOT IN (SELECT id FROM (SELECT id,agent,kind,status,created,error FROM runtime_events
        ORDER BY created DESC,id LIMIT 200))
        ON CONFLICT(collection,id) DO UPDATE SET
            seq=excluded.seq,hash=excluded.hash,payload=excluded.payload,deleted=excluded.deleted
        WHERE sync_entities.hash!=excluded.hash OR sync_entities.deleted!=excluded.deleted;
          INSERT INTO sync_entity_meta(key,value)
        VALUES('event_window_seq',CAST((SELECT COALESCE(MAX(seq),0) FROM sync_entities
            WHERE collection='event') AS TEXT))
        ON CONFLICT(key) DO UPDATE SET value=excluded.value
        WHERE sync_entity_meta.value!=excluded.value;
        END;
CREATE INDEX runtime_budget_usage_agent ON runtime_budget_usage(agent,kind);
CREATE INDEX runtime_work_root_status
                ON runtime_work(json_extract(record,'$.rootId'),json_extract(record,'$.status'));
CREATE INDEX runtime_work_owner_status
                ON runtime_work(json_extract(record,'$.owner'),json_extract(record,'$.status'));
CREATE INDEX runtime_work_owner_root_status_id ON runtime_work(
                json_extract(record,'$.owner'),json_extract(record,'$.rootId'),
                json_extract(record,'$.status'),id);
CREATE INDEX runtime_work_archive_due ON runtime_work(
                COALESCE(json_extract(record,'$.archiveIntent.nextAttemptAt'),0),id)
                WHERE json_extract(record,'$.archiveIntent.status')='pending';
CREATE INDEX runtime_payload_refs_hash ON runtime_payload_refs(sha256);
CREATE TRIGGER runtime_payload_checkpoints_insert AFTER INSERT ON runtime_checkpoints BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_checkpoints_update AFTER UPDATE OF record ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_checkpoints',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_checkpoints_delete AFTER DELETE ON runtime_checkpoints BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_checkpoints' AND record_id=OLD.id;
        END;
CREATE TRIGGER runtime_payload_requests_insert AFTER INSERT ON runtime_tool_requests BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_requests_update AFTER UPDATE OF record ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_requests',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.record,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_requests_delete AFTER DELETE ON runtime_tool_requests BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_requests' AND record_id=OLD.id;
        END;
CREATE TRIGGER runtime_payload_results_insert AFTER INSERT ON runtime_tool_results BEGIN
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_results_update AFTER UPDATE OF result ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
            INSERT OR REPLACE INTO runtime_payload_refs(table_name,record_id,field,sha256)
            SELECT 'runtime_tool_results',NEW.id,j.key,json_extract(j.value,'$.sha256')
            FROM json_each(json_extract(NEW.result,'$._payloadBlobs')) AS j
            WHERE json_type(j.value,'$.sha256')='text';
        END;
CREATE TRIGGER runtime_payload_results_delete AFTER DELETE ON runtime_tool_results BEGIN
            DELETE FROM runtime_payload_refs WHERE table_name='runtime_tool_results' AND record_id=OLD.id;
        END;
CREATE INDEX runtime_tool_request_actor
                ON runtime_tool_requests(json_extract(record,'$.agent'), json_extract(record,'$.updated') DESC);
CREATE INDEX runtime_workspace_operation_phase ON runtime_workspace_operations(
                json_extract(record,'$.phase'), json_extract(record,'$.agent'));
CREATE INDEX runtime_rule_ready_tick ON runtime_rules(json_extract(record,'$.status')) WHERE NOT COALESCE(json_extract(record,'$.inFlight'),0);
CREATE INDEX runtime_request_agent_status ON runtime_requests(json_extract(record,'$.agent'),json_extract(record,'$.status'));
CREATE INDEX runtime_rule_agent_inflight ON runtime_rules(json_extract(record,'$.agent'),json_extract(record,'$.inFlight'));
CREATE INDEX runtime_conversion_rules_rootId ON runtime_rules(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_workspace_operation_agent_phase ON runtime_workspace_operations(json_extract(record,'$.agent'),json_extract(record,'$.phase'));
CREATE INDEX runtime_room_root ON runtime_rooms(json_extract(record,'$.rootId'));
CREATE INDEX runtime_room_member_first ON runtime_rooms(json_extract(record,'$.members[0]'));
CREATE INDEX runtime_room_member_second ON runtime_rooms(json_extract(record,'$.members[1]'));
CREATE INDEX runtime_room_many_members ON runtime_rooms(json_array_length(record,'$.members')) WHERE json_array_length(record,'$.members')>2;
CREATE INDEX runtime_conversion_plans_rootId ON runtime_plans(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_annotations_rootId ON runtime_annotations(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_complaints_leadId ON runtime_complaints(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
CREATE INDEX runtime_conversion_event_meta_rootId ON runtime_event_meta(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.rootId') IS NOT NULL;
CREATE INDEX runtime_conversion_event_meta_leadId ON runtime_event_meta(json_extract(record,'$.leadId')) WHERE json_extract(record,'$.leadId') IS NOT NULL;
CREATE INDEX runtime_agent_dispatch_active ON runtime_agents(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.inFlight')=1 OR json_extract(record,'$.status') IN ('running','starting','approval');
CREATE INDEX runtime_agent_dispatch_busy_input ON runtime_agents(json_extract(record,'$.rootId')) WHERE json_extract(record,'$.startAttempt.activeAtReservation')=1;
CREATE INDEX runtime_agent_dispatch_workspace ON runtime_agents(json_extract(record,'$.cwd')) WHERE json_type(record,'$.workspaceOperation')='text' AND json_extract(record,'$.workspaceOperation')!='';
CREATE INDEX runtime_account_transfer_status ON runtime_account_transfers(json_extract(record,'$.status'));
CREATE TRIGGER sync_transcript_revision_runtime_agents_insert AFTER INSERT ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_agents_update AFTER UPDATE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.id,1 WHERE OLD.id!=NEW.id
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_agents_delete AFTER DELETE ON runtime_agents BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.id,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_insert AFTER INSERT ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_update AFTER UPDATE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (NEW.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1;
                                    INSERT INTO runtime_transcript_revisions(agent,revision)
                                    SELECT OLD.agent,1 WHERE OLD.agent!=NEW.agent
                                    ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
CREATE TRIGGER sync_transcript_revision_runtime_items_delete AFTER DELETE ON runtime_items BEGIN INSERT INTO runtime_transcript_revisions(agent,revision)
                                    VALUES (OLD.agent,1) ON CONFLICT(agent) DO UPDATE SET revision=revision+1; END;
PRAGMA writable_schema=OFF;
DELETE FROM "sqlite_sequence";
INSERT INTO "sqlite_sequence" VALUES('runtime_chat_messages',5);
COMMIT;
