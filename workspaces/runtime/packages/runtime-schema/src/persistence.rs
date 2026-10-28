//! Closed persistence vocabularies and the request identity/age contract.
//! SQLite DDL and language consumers derive their values from these owners.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::VariantNames;
use uuid::Uuid;

/// Maximum request-result age. Unit: milliseconds (30 days).
pub const REQUEST_RESULT_TTL_MS: i64 = 2_592_000_000;
/// Maximum accepted UUIDv7 issuance time ahead of the clock. Unit: milliseconds.
pub const REQUEST_ID_FUTURE_SKEW_MS: i64 = 120_000;

/// The current provider process identity captured by an operation owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderScope {
    pub generation: u64,
    pub connection: String,
}

/// A retained operation either captured its exact scope or cannot prove it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "state",
    content = "scope",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum CapturedScope {
    Known(ProviderScope),
    Unproven,
}

macro_rules! domain {
    ($(#[$meta:meta])* $name:ident { $($(#[$variant_meta:meta])* $variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, strum::VariantNames)]
        $(#[$meta])*
        pub enum $name {
            $($(#[$variant_meta])* #[serde(rename = $wire)] #[strum(serialize = $wire)] $variant),+
        }
        impl $name {
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
        }
        #[cfg(feature = "sqlite")]
        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                // Serialize directly to a string at the SQL boundary; no Value carrier.
                let json = serde_json::to_string(self)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                let wire: String = serde_json::from_str(&json)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                Ok(rusqlite::types::ToSqlOutput::Owned(wire.into()))
            }
        }
        #[cfg(feature = "sqlite")]
        impl rusqlite::types::FromSql for $name {
            fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                Self::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(value.as_str()?))
                    .map_err(|error| rusqlite::types::FromSqlError::Other(Box::new(error)))
            }
        }
    };
}

domain!(ExecutionReceiptKind { Submission=>"submission", StartResult=>"startResult", Unknown=>"unknown", ThreadPrepared=>"threadPrepared", Terminal=>"terminal", TerminalInput=>"terminalInput" });
domain!(ContextRepairPhase { Preparing=>"preparing", Submitted=>"submitted", Unknown=>"unknown", Ready=>"ready", Completed=>"completed", Failed=>"failed", Unchanged=>"unchanged" });
domain!(SteerStage { Reserved=>"reserved", Submitted=>"submitted", Unconfirmed=>"unconfirmed" });
domain!(MonitorInputStatus { Pending=>"Pending", Acknowledged=>"Acknowledged", Rejected=>"Rejected", Unknown=>"Unknown", Expired=>"Expired" });
domain!(NativeActionAdmissionStatus { Accepted=>"accepted" });
domain!(VoiceStopOutcome { Submitted=>"submitted", Written=>"written", NotSent=>"not_sent", Unknown=>"unknown", Rejected=>"rejected", Expired=>"expired" });
domain!(TerminalInputDelivery { Sent=>"sent", Uncertain=>"uncertain" });
domain!(ServerDeliveryState { Queued=>"queued", Running=>"running", Waiting=>"waiting", Complete=>"complete" });
domain!(FederationDeliveryState { Queued=>"queued", Delivered=>"delivered" });
domain!(FederationAckState { Pending=>"pending", Sent=>"sent", Confirmed=>"confirmed" });
domain!(FederationMessageKind { ChatMessage=>"chat-message", RoomInvite=>"room-invite", RoomAccept=>"room-accept" });
domain!(AccessOutboundState { Unknown=>"unknown", Complete=>"complete", Redacted=>"redacted" });
domain!(RemoteOutcome { Applied=>"applied", NotApplied=>"not_applied", Unknown=>"unknown" });
domain!(RequestCardStatus { Pending=>"pending", Answering=>"answering", Answered=>"answered", Uncertain=>"uncertain", Blocked=>"blocked", Resolved=>"resolved", Expired=>"expired", Deleted=>"deleted" });
domain!(TerminalActionFailure { UnknownOutcome=>"unknown_outcome", Rejected=>"rejected", Disconnected=>"disconnected" });
domain!(NativeReleasePhase { Checking=>"checking", Unsubscribing=>"unsubscribing", Unknown=>"unknown", Released=>"released", Blocked=>"blocked", Resumed=>"resumed" });
domain!(ServerFetchState { Preparing=>"preparing", Fetching=>"fetching", Complete=>"complete" });
domain!(ServerOperationKind { Event=>"event", Input=>"input", Message=>"message", MoveSend=>"move_send", MoveStatus=>"move_status", LocationInfo=>"location_info", LocationMatches=>"location_matches", Stop=>"stop", ExecEvent=>"exec_event", Projects=>"projects", Folders=>"folders", Git=>"git", Export=>"export", Chunk=>"chunk", ExecRead=>"exec_read", ExecInput=>"exec_input", ExecCancel=>"exec_cancel", Release=>"release", UpdateOffer=>"update.offer", UpdateExport=>"update.export", UpdateChunk=>"update.chunk", UpdateRelease=>"update.release", Task=>"task", Complaint=>"complaint", Context=>"context", Directory=>"directory", ChatRead=>"chat_read", MoveManage=>"move_manage", MoveInterrupt=>"move_interrupt", MoveSpawn=>"move_spawn", Exec=>"exec", ExecReceipt=>"exec_receipt", BrowserCallback=>"browser_callback", AddLocation=>"add_location", RemoveLocation=>"remove_location", MoveValidate=>"move_validate", MoveBegin=>"move_begin" });
domain!(RecoveryReceiptStatus { Unknown=>"unknown", Unconfirmed=>"unconfirmed", Blocked=>"blocked", Skipped=>"skipped", Superseded=>"superseded", HistoryMissing=>"history_missing", InputRestored=>"input_restored", Held=>"held", Reconciled=>"reconciled", Adopted=>"adopted", Accepted=>"accepted", Checked=>"checked", Released=>"released" });
domain!(NativeSettingsStatus { Submitted=>"submitted", Acknowledged=>"acknowledged", NotSent=>"not_sent", Unknown=>"unknown", Rejected=>"rejected" });
domain!(SafetyRetryStage { Turns=>"turns", Items=>"items", Interrupt=>"interrupt", VerifyTurns=>"verify_turns", VerifyItems=>"verify_items", Fork=>"fork", Start=>"start", Running=>"running", Unknown=>"unknown", Failed=>"failed", Cancelled=>"cancelled" });
domain!(SafetyRetryDispatch { Ready=>"ready", Intent=>"intent", Received=>"received", Rejected=>"rejected", Unknown=>"unknown", Cancelled=>"cancelled" });
domain!(ResetCreditStatus { Pending=>"pending", Uncertain=>"uncertain", Completed=>"completed" });
domain!(ResetCreditCertainty { NotSent=>"not_sent", Written=>"written", Unknown=>"unknown" });
domain!(ResetCreditOutcome { Reset=>"reset", NothingToReset=>"nothingToReset", NoCredit=>"noCredit", AlreadyRedeemed=>"alreadyRedeemed" });
domain!(NativeReviewThreadStatus { Submitted=>"submitted", Accepted=>"accepted", Rejected=>"rejected", NotSent=>"not_sent", Unknown=>"unknown" });
domain!(SafetyRetryCertainty { Written=>"written", NotSent=>"not_sent", Unknown=>"unknown", Refused=>"refused" });
domain!(WorkStatus { Ready=>"ready", Blocked=>"blocked", Running=>"running", Review=>"review", Accepted=>"accepted", Cancelled=>"cancelled" });
domain!(OwnerKind { Agent=>"agent", Task=>"task", Account=>"account", Vm=>"vm", Project=>"project", Monitor=>"monitor", Terminal=>"terminal", Message=>"message", Request=>"request", Server=>"server" });
domain!(OutboxKind { ProviderCall=>"provider_call", ProviderCallback=>"provider_callback", JournalEntry=>"journal_entry", Publication=>"publication", StreamFlush=>"stream_flush", FollowupEffect=>"followup_effect", Steer=>"steer", VoiceStop=>"voice_stop", VoiceDelete=>"voice_delete", WorkArchive=>"work_archive", WorkVm=>"work_vm", TitleGeneration=>"title_generation", NativeAction=>"native_action", TerminalAction=>"terminal_action", MonitorInput=>"monitor_input", McpReconnect=>"mcp_reconnect", ProjectRegistration=>"project_registration", RemoteDelivery=>"remote_delivery", ManagedTool=>"managed_tool" });
domain!(OutboxStatus { Pending=>"pending", Submitted=>"submitted", Unknown=>"unknown", Committed=>"committed", Unrecognized=>"unrecognized" });
domain!(HoldStatus { Clear=>"clear", Invalid=>"invalid", Unrecognized=>"unrecognized" });
domain!(ToolRequestKind { ManagedTool=>"managed_tool", ProviderCallback=>"provider_callback", Unrecognized=>"unrecognized" });
domain!(ToolRequestStatus { Queued=>"queued", Running=>"running", Cancelled=>"cancelled", Interrupted=>"interrupted", Failed=>"failed", Unknown=>"unknown", Completed=>"completed" });
domain!(ToolRequestOutcome { Pending=>"pending", Applied=>"applied", NotApplied=>"not_applied", Unknown=>"unknown" });
domain!(ExecutionStatus { Unsent=>"unsent", Pending=>"pending", Accepted=>"accepted", Observed=>"observed", Running=>"running", Unknown=>"unknown", Rejected=>"rejected", Completed=>"completed", Failed=>"failed", Interrupted=>"interrupted", Cancelled=>"cancelled" });
domain!(AttemptSubmission { Unsent=>"unsent", Pending=>"pending", Accepted=>"accepted", Observed=>"observed", Rejected=>"rejected", Unknown=>"unknown" });
domain!(ExecutionEffectKind { Spawn=>"spawn", ToolRequests=>"tool_requests", Requests=>"requests", Monitors=>"monitors", TaskSubmit=>"task_submit", Unrecognized=>"unrecognized" });
domain!(ExecutionEffectStatus { Unknown=>"unknown", Idle=>"idle", Queued=>"queued", Starting=>"starting", Running=>"running", Waiting=>"waiting", WaitingOnMonitor=>"waiting_on_monitor", Approval=>"approval", Completed=>"completed", Interrupted=>"interrupted", Failed=>"failed", Blocked=>"blocked", Paused=>"paused", Parked=>"parked", Abandoned=>"abandoned", CapacityRetry=>"capacity-retry", Pending=>"pending", Cancelled=>"cancelled", Answering=>"answering", Answered=>"answered", Uncertain=>"uncertain", Resolved=>"resolved", Lost=>"lost", Expired=>"expired", Deleted=>"deleted" });
domain!(ExecutionNodeStatus { Unknown=>"unknown", Idle=>"idle", Queued=>"queued", Starting=>"starting", Running=>"running", Waiting=>"waiting", WaitingOnMonitor=>"waiting_on_monitor", Approval=>"approval", Completed=>"completed", Interrupted=>"interrupted", Failed=>"failed", Blocked=>"blocked", Paused=>"paused", Parked=>"parked", Abandoned=>"abandoned", CapacityRetry=>"capacity-retry" });
domain!(WorkEffectStatus { Completed=>"completed", Terminal=>"terminal", Pending=>"pending", Retryable=>"retryable", Submitted=>"submitted", Unknown=>"unknown", NotSent=>"not_sent", Complete=>"complete", Applied=>"applied", Failed=>"failed", Cancelled=>"cancelled" });
domain!(TitleStatus { Scheduled=>"scheduled", Written=>"written", Invalid=>"invalid", Discarded=>"discarded", Pending=>"pending", Submitted=>"submitted", Unknown=>"unknown", NotSent=>"not_sent", Applied=>"applied", Failed=>"failed" });
domain!(VmLaunchStatus { Preparing=>"preparing", Submitted=>"submitted", Unknown=>"unknown", Ready=>"ready", Failed=>"failed" });
domain!(IdleEpisodeOutcome { Compacted=>"compacted", Disabled=>"disabled", CompactionOff=>"compaction_off", IdleCompactionOff=>"idle_compaction_off", PrefixChanged=>"prefix_changed", Started=>"started", Dispatched=>"dispatched", Failed=>"failed", NotOneHour=>"not_one_hour", Lapsed=>"lapsed", Small=>"small", NewerRequest=>"newer_request", NearLimit=>"near_limit", Active=>"active", QueuedInput=>"queued_input", Stopped=>"stopped", NoHost=>"no_host", WouldCompact=>"would_compact" });
domain!(ProjectRemovalReason { Delete=>"delete" });
domain!(EventKind { User=>"user", Followup=>"followup", AgentMessage=>"agent_message", ChildResult=>"child_result", ChildStopped=>"child_stopped", Complaint=>"complaint", ComplaintResponse=>"complaint_response", ChatReview=>"chat_review", WorkReview=>"work_review", WorkDecision=>"work_decision", WorkReady=>"work_ready", MonitorExit=>"monitor_exit", MonitorStall=>"monitor_stall", Rule=>"rule", RuleMissed=>"rule_missed", RuleStall=>"rule_stall", RadioTurn=>"radio_turn", BrowserDeclined=>"browser_declined", BrowserJoined=>"browser_joined", BrowserOutcome=>"browser_outcome", BrowserUnanswered=>"browser_unanswered", EventWake=>"event_wake", MonitorCancelled=>"monitor_cancelled", MonitorOutput=>"monitor_output", PlanUpdate=>"plan_update", ShellPolicy=>"shell_policy", WorkReleased=>"work_released", WorkResultInvalid=>"work_result_invalid" });
domain!(EventStatus { StoredOnly=>"stored_only", Pending=>"pending", Reserved=>"reserved", Dispatching=>"dispatching", Delivered=>"delivered", Uncertain=>"uncertain", Cancelled=>"cancelled", Failed=>"failed" });
domain!(DeliveryStatus { Queued=>"queued", StoredOnly=>"stored_only", Accepted=>"accepted", Sent=>"sent", Cancelled=>"cancelled", Pending=>"pending", Delivered=>"delivered", Failed=>"failed", Uncertain=>"uncertain" });
domain!(MigrationStatus { Pending=>"pending", Running=>"running", WaitingForSpace=>"waitingForSpace", Error=>"error", Complete=>"complete" });
domain!(ReceiptMigrationPhase { Prepare=>"prepare", Move=>"move", Finish=>"finish" });
domain!(
    /// Why work needs attention. The service owns classification and publication.
    #[schemars(rename = "StudioErrorKind")] IncidentKind {
    MonitorOutcomeLost=>"monitor_outcome_lost", MonitorNotRecorded=>"monitor_not_recorded",
    InputUnconfirmed=>"input_unconfirmed", InputUncertain=>"input_uncertain", InputStuck=>"input_stuck",
    TurnDisconnected=>"turn_disconnected", WorkspaceFailed=>"workspace_failed", HttpFailure=>"http_failure",
    RecoveryFailed=>"recovery_failed", StoredRecordInvalid=>"stored_record_invalid", UnclassifiedFailure=>"unclassified_failure",
    /// A running turn whose provider is silent while no tool runs.
    TurnStalled=>"turn_stalled",
    /// An owner message that waits while its chat does not start.
    InputNotStarted=>"input_not_started",
    /// Chats that wait for an account session that does not start.
    SessionNotStarted=>"session_not_started",
    /// A usage resume, capacity retry or account transfer that did not move.
    ScheduleStalled=>"schedule_stalled"
});

/// Generated language/SQL parity registry; variants always come from the enum derive.
macro_rules! contracts {
    ($($name:ident),+ $(,)?) => {
        pub const ENUM_CONTRACTS: &[(&str, &[&str])] = &[$((stringify!($name), $name::VARIANTS)),+];
        #[cfg(all(test, feature="sqlite"))]
        #[test]
        fn receipt_enum_sql_serde_parity() {
            $(tests::check_sql_enum::<$name>();)+
        }
    };
}
contracts!(
    SafetyRetryCertainty,
    NativeReviewThreadStatus,
    ResetCreditStatus,
    ResetCreditCertainty,
    ResetCreditOutcome,
    SafetyRetryStage,
    SafetyRetryDispatch,
    NativeSettingsStatus,
    ServerFetchState,
    ServerOperationKind,
    RecoveryReceiptStatus,
    RequestCardStatus,
    TerminalActionFailure,
    NativeReleasePhase,
    RemoteOutcome,
    VoiceStopOutcome,
    TerminalInputDelivery,
    ServerDeliveryState,
    FederationDeliveryState,
    FederationAckState,
    FederationMessageKind,
    AccessOutboundState,
    ExecutionReceiptKind,
    ContextRepairPhase,
    SteerStage,
    MonitorInputStatus,
    NativeActionAdmissionStatus,
    WorkStatus,
    OwnerKind,
    OutboxKind,
    OutboxStatus,
    HoldStatus,
    ToolRequestKind,
    ToolRequestStatus,
    ToolRequestOutcome,
    ExecutionStatus,
    AttemptSubmission,
    ExecutionEffectKind,
    ExecutionEffectStatus,
    ExecutionNodeStatus,
    WorkEffectStatus,
    TitleStatus,
    VmLaunchStatus,
    IdleEpisodeOutcome,
    ProjectRemovalReason,
    EventKind,
    EventStatus,
    DeliveryStatus,
    MigrationStatus,
    ReceiptMigrationPhase,
    IncidentKind
);

/// One recipient's delivery state. Error text is separate from the closed status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipientDelivery {
    pub status: DeliveryStatus,
    pub error: Option<String>,
}

/// CHECK expression for a trusted schema identifier, generated solely from the enum.
pub fn enum_check<T: VariantNames>(column: &str) -> String {
    let values = T::VARIANTS
        .iter()
        .map(|value| format!("'{}'", value.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    format!("CHECK(\"{}\" IN ({values}))", column.replace('"', "\"\""))
}

/// A collision-free tuple. The UUID root is explicit, never inferred from a key prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestIdentity {
    id: String,
    issued_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestIdentityError {
    #[error("new request id must be a UUIDv7: {0}")]
    InvalidUuidV7(String),
    #[error("request id expired (issued {issued_ms} <= {expired_before_ms} milliseconds)")]
    Expired {
        issued_ms: i64,
        expired_before_ms: i64,
    },
    #[error("request id issuance {issued_ms} exceeds future limit {latest_ms} milliseconds")]
    FutureTimestamp { issued_ms: i64, latest_ms: i64 },
}

impl RequestIdentity {
    pub fn new(
        namespace: &str,
        authority: &[&str],
        root_request_id: &str,
        operation: &[&str],
    ) -> Result<Self, RequestIdentityError> {
        let uuid = Uuid::parse_str(root_request_id)
            .ok()
            .filter(|uuid| {
                uuid.get_version_num() == 7 && uuid.get_variant() == uuid::Variant::RFC4122
            })
            .ok_or_else(|| RequestIdentityError::InvalidUuidV7(root_request_id.into()))?;
        let timestamp = uuid
            .get_timestamp()
            .ok_or_else(|| RequestIdentityError::InvalidUuidV7(root_request_id.into()))?;
        let (seconds, nanos) = timestamp.to_unix();
        let issued_ms = (seconds * 1_000 + u64::from(nanos) / 1_000_000) as i64;
        // All inputs are strings, so tuple serialization cannot fail.
        let id = serde_json::to_string(&(namespace, authority, uuid.to_string(), operation))
            .expect("string tuple serializes");
        Ok(Self { id, issued_ms })
    }

    pub fn issue_root() -> String {
        Uuid::now_v7().to_string()
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn issued_ms(&self) -> i64 {
        self.issued_ms
    }
    pub fn validate(
        &self,
        now_ms: i64,
        expired_before_ms: i64,
    ) -> Result<(), RequestIdentityError> {
        let floor = now_ms
            .saturating_sub(REQUEST_RESULT_TTL_MS)
            .max(expired_before_ms);
        if self.issued_ms <= floor {
            return Err(RequestIdentityError::Expired {
                issued_ms: self.issued_ms,
                expired_before_ms: floor,
            });
        }
        let latest_ms = now_ms.saturating_add(REQUEST_ID_FUTURE_SKEW_MS);
        if self.issued_ms > latest_ms {
            return Err(RequestIdentityError::FutureTimestamp {
                issued_ms: self.issued_ms,
                latest_ms,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_provider_scope_round_trips_writer_bytes() {
        for scope in [
            CapturedScope::Known(ProviderScope {
                generation: 42,
                connection: "connection".into(),
            }),
            CapturedScope::Unproven,
        ] {
            let bytes = serde_json::to_vec(&scope).unwrap();
            let decoded: CapturedScope = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(serde_json::to_vec(&decoded).unwrap(), bytes);
        }
    }
    #[test]
    fn receipt_recipient_delivery_schema_matches_nullable_owner() {
        let openapi: serde_json::Value =
            serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
        let document = serde_json::json!({"$ref":"#/components/schemas/RecipientDelivery","components":openapi["components"]});
        let validator = jsonschema::validator_for(&document).expect("recipient schema");
        let record = RecipientDelivery {
            status: DeliveryStatus::Queued,
            error: None,
        };
        assert!(
            validator.is_valid(&serde_json::to_value(record).expect("typed recipient")),
            "wire schema accepts owner null error"
        );
        assert!(!validator.is_valid(&serde_json::json!("queued")));
        assert!(!validator.is_valid(&serde_json::json!({"status":"future","error":null})));
        assert!(!validator.is_valid(&serde_json::json!({"status":"failed:reason","error":null})));
    }
    #[cfg(feature = "sqlite")]
    pub(super) fn check_sql_enum<T>()
    where
        T: VariantNames
            + Serialize
            + serde::de::DeserializeOwned
            + rusqlite::types::ToSql
            + rusqlite::types::FromSql
            + std::fmt::Debug
            + Eq,
    {
        let db = rusqlite::Connection::open_in_memory().expect("fresh database");
        db.execute_batch(&format!(
            "CREATE TABLE witness(value TEXT NOT NULL {})",
            enum_check::<T>("value")
        ))
        .expect("generated CHECK");
        let ddl: String = db
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name='witness'",
                [],
                |row| row.get(0),
            )
            .expect("actual schema");
        assert!(ddl.contains(&enum_check::<T>("value")));
        for wire in T::VARIANTS {
            let parsed: T =
                serde_json::from_str(&format!("\"{wire}\"")).expect("serde matches strum");
            assert_eq!(
                serde_json::to_string(&parsed).expect("serde"),
                format!("\"{wire}\"")
            );
            db.execute("INSERT INTO witness VALUES(?1)", [&parsed])
                .expect("enum SQL codec");
            let decoded: T = db
                .query_row(
                    "SELECT value FROM witness ORDER BY rowid DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .expect("SQL decode");
            assert_eq!(decoded, parsed);
        }
        assert!(
            db.execute("INSERT INTO witness VALUES('bogus')", [])
                .is_err()
        );
        assert!(serde_json::from_str::<T>("\"bogus\"").is_err());
    }
    fn at(ms: i64) -> RequestIdentity {
        let uuid = Uuid::from_u128(((ms as u128) << 80) | (7 << 76) | (2 << 62));
        RequestIdentity::new("mutation", &["client"], &uuid.to_string(), &[]).expect("v7")
    }
    #[test]
    fn receipt_identity_frozen_clock_boundaries() {
        let now = REQUEST_RESULT_TTL_MS * 2;
        assert!(at(now - REQUEST_RESULT_TTL_MS + 1).validate(now, 0).is_ok());
        for ms in [now - REQUEST_RESULT_TTL_MS, now - REQUEST_RESULT_TTL_MS - 1] {
            assert!(matches!(
                at(ms).validate(now, 0),
                Err(RequestIdentityError::Expired { .. })
            ));
        }
        assert!(at(now + REQUEST_ID_FUTURE_SKEW_MS).validate(now, 0).is_ok());
        assert!(matches!(
            at(now + REQUEST_ID_FUTURE_SKEW_MS + 1).validate(now, 0),
            Err(RequestIdentityError::FutureTimestamp { .. })
        ));
    }
    #[test]
    fn receipt_identity_scopes_are_collision_free() {
        let root = RequestIdentity::issue_root();
        let a = RequestIdentity::new("access", &["a:b", "c"], &root, &["x"]).expect("v7");
        for (namespace, scope, operation) in [
            ("access", vec!["a", "b:c"], vec!["x"]),
            ("operation", vec!["a:b", "c"], vec!["x"]),
            ("access", vec!["a:b", "c"], vec!["y"]),
        ] {
            assert_ne!(
                a,
                RequestIdentity::new(namespace, &scope, &root, &operation).expect("v7")
            );
        }
        assert!(RequestIdentity::new("access", &[], "old-id", &[]).is_err());
    }
}
