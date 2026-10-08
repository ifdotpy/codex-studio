"""Checks the stored agent declaration against its creation and entity contract."""

import ast
import unittest
from enum import Enum
from pathlib import Path
from typing import Any, Literal, get_args, get_origin, get_type_hints

from pydantic import BaseModel

from codex_records import (
    AgentModeValue,
    AgentProviderValue,
    AgentRecord,
    AgentRoleValue,
    AgentStatusValue,
    ComplaintStatusValue,
    NativeNameSyncedRecord,
    UsageResumeCauseValue,
    UsageResumeStatusValue,
    WorkStatusValue,
    WorkspaceOperationPhase,
)
from studio_api.sync.models import AgentEntityDto, AgentMode, AgentProvider, AgentRole, AgentStatus


# This is the one explicit partition of the stored shape and the entity view.
# Additions to either declaration must be assigned to exactly one group here.
AGENT_FIELD_GROUPS = {
    "shared": frozenset("""
        accountKey accountTransfer activity agentMode agentModeRevision agentModeSupported
        archived autoWake compactions concurrency connectionCheck contextUsage convertedFromLead
        created cwd daybreakEnabled deletedAt effort environment error fastMode id imageWorkspace
        imageWorkspaceBaseRef imageWorkspaceBaseRepo imageWorkspaceCreatedAt imageWorkspaceError
        imageWorkspaceHasGit imageWorkspacePhase imageWorkspaceReady imageWorkspaceRepo
        imageWorkspaceSubpath inFlight isLead
        lastAnswer lastCompletedTurn manualName model name nativeLimitErrorAt nativeRelease
        nativeSafetyBuffering nativeSafetyRetry nativeStatus nativeThreadBlock nativeTurnError parentId parkedEvent
        pendingSettings pendingSettingsAccountKey pinned project projectFolder projectFolderRevision
        provider quickCreate readState reviewDefaults role rootId sharedRoomId startAttempt status
        subagentConcurrencyVersion tail threadId tokensUsed turnId updated workerDefaults worktree
        worktreePreparation yoloMode
        accountTransferId capacityRetry contextRepairWait epoch lastCompletedTurnStatus lastEvent
        usageResume workspaceOperation remoteWorker remoteOrigin remoteAnchor
    """.split()),
    "private": frozenset("""
        accountHistory accountId accountTransferState activeTools activityPhase
        agentArchive agentModeChangedAt agentModeChangedBy approvalPolicy browserRecovery budgetActionWait
        branch budgetBlocked budgetStartWait cancelledPark capacityRetryCount checkpointError
        checkpointHistoryHead complaintMisses complaintsPresented
        claudeInputRequest claudeOptions claudePreInputRetry cleanedWorktree compactionsObservedOnly
        connectionRecovery contextRepair contextRepairHistory cyberAccessProgram
        deliveredMode deliveredModeVersion disconnectRecovery emptyTransferRecovery events
        executionSettingsAccountKey imageWorkspaceCollect imageWorkspaceMount imageWorkspaceNoticeError
        imageWorkspaceNoticeId imageWorkspaceNoticeSent imageWorkspaceNoticeText imageWorkspaceHandoffText
        imageWorkspaceSnapshotCommit importedFrom
        lastBudgetWait lastCompletedTurnError lastContextRepairCheck
        lastContextRepairWait lastUpdated lastWorkspaceWait lazyAccountTransfer liveSteerAttempt
        liveSteerRejectedTurnId maxAgents maxAgentsExplicit nativeEffort nativeFailureHold nativeName
        nativeNameFailure nativeNameSyncError nativeNameSynced nativeOperation nativeResponseTurn
        nativeReview nativeToolCatalog needsAttention needsTitle parkAfterTurn parkReceipt parkSequence
        prepareAttempt preparedContext profile profileId profileInstructions prompt queueNotice quickCreateRequest
        restartRecovery restoredCheckpoint reviewArchiveAttempts reviewArchiveError reviewArchiveNextAt
        reviewArchiveScheduled
        sandbox startOutcomeHold steerRejectedTurnId supervisorRestore tokenBudget tokenUsageAccounting
        turnEpoch turnRecovery usageResumeEnabled workerBaseBehindMain workerBaseCommit
        workerBaseMainRef workerBaseRef workerBaseStatus worktreeCleanup worktreeReady
        worktreeWarning authResumeAttempt cleanedImageWorkspace imageWorkspaceCleanupResult
        imageWorkspaceBaseError nativeToolRefreshId nativeToolUpdate portableHistory
        queueMutationRevision workspaceReservationId forkedFrom sourceMessage draft
        remoteEpoch remoteControlEpoch remoteStateSequence remoteReservation remoteAdmission
        remoteAdmissionRequest remoteLastAdmission remoteStopRequest
    """.split()),
    "derived": frozenset("""
        canSend empty hasApproval hasQuestion hasUnread kind lastReadAt launcherAlive nativeError
        nextTurnSettingsSupported orchestratorId orchestratorName overview
        panelDataVersion panelVersion queuedSettings readStateSupported retryAt source statusDetail
        turnStatus unreadCount voiceState
    """.split()),
}

# These are measured storage/wire disagreements, not permission to coerce rows.
KNOWN_WIRE_DISAGREEMENTS = {
    "nativeNameSynced": "stored identity receipt; not a renderer entity field",
    "workerBaseBehindMain": "stored commit count; not a renderer entity field",
    "error": "stored provider dict/string/null; entity DTO declares NativeProviderError/string/null",
    "worktree": "stored bool; entity DTO also accepts legacy string",
    "startAttempt": "full stored attempt; entity projection forwards only prepareError, responseError, retiredEvents",
    "nativeRelease": "full stored release receipt; entity projection forwards only phase and resetPending",
    "status": "stored JSON strings use the same closed values as the AgentStatus enum",
}


try:
    from types import UnionType
except ImportError:  # pragma: no cover - supported Python starts at 3.11
    UnionType = object  # type: ignore[assignment,misc]


def _enum_or_literal_values(annotation: Any) -> set[object] | None:
    if get_origin(annotation) is Literal:
        return set(get_args(annotation))
    if isinstance(annotation, type) and issubclass(annotation, Enum):
        return {member.value for member in annotation}
    return None


def _union_members(annotation: Any) -> tuple[Any, ...]:
    if get_origin(annotation) in (UnionType, __import__("typing").Union):
        return get_args(annotation)
    return (annotation,)


def _compatible(source: Any, target: Any) -> bool:
    """Check whether each stored JSON value fits the declared DTO annotation."""
    if target is Any:
        return True
    if get_origin(target) is __import__("typing").Annotated:
        return _compatible(source, get_args(target)[0])
    source_members = _union_members(source)
    target_members = _union_members(target)
    for source_member in source_members:
        if source_member is type(None):
            if type(None) not in target_members:
                return False
            continue
        if not any(_compatible_one(source_member, target_member) for target_member in target_members):
            return False
    return True


def _compatible_one(source: Any, target: Any) -> bool:
    if target is Any:
        return True
    if getattr(target, "__name__", None) == "JsonValue":
        return source in (str, int, float, bool, type(None)) or get_origin(source) in (list, dict)
    if get_origin(target) is __import__("typing").Annotated:
        return _compatible(source, get_args(target)[0])
    source_values = _enum_or_literal_values(source)
    target_values = _enum_or_literal_values(target)
    if source_values is not None:
        if target_values is not None:
            return source_values <= target_values
        return target is str and all(isinstance(value, str) for value in source_values)
    if target_values is not None:
        return False
    source_origin, target_origin = get_origin(source), get_origin(target)
    if source_origin is list and target_origin is list:
        return _compatible(get_args(source)[0], get_args(target)[0])
    if source_origin is dict and target_origin is dict:
        return _compatible(get_args(source)[0], get_args(target)[0]) and _compatible(
            get_args(source)[1], get_args(target)[1]
        )
    if isinstance(source, type) and getattr(source, "__required_keys__", None) is not None:
        if isinstance(target, type) and issubclass(target, BaseModel):
            source_hints = get_type_hints(source)
            target_hints = get_type_hints(target)
            for key, source_annotation in source_hints.items():
                model_field = target.model_fields.get(key)
                if model_field is None or not _compatible(source_annotation, target_hints[key]):
                    return False
            required = {key for key, field in target.model_fields.items() if field.is_required()}
            return required <= set(getattr(source, "__required_keys__", ()))
    if get_origin(source) is dict and isinstance(target, type) and issubclass(target, BaseModel):
        return target.model_config.get("extra") == "allow" and _compatible(
            get_args(source)[1], Any
        )
    return source == target or (
        isinstance(source, type) and isinstance(target, type) and issubclass(source, target)
    )


def _creation_literal_keys() -> set[str]:
    runtime_path = Path(__file__).with_name("codex_runtime.py")
    tree = ast.parse(runtime_path.read_text(encoding="utf-8"))
    matches = [
        node for node in ast.walk(tree)
        if isinstance(node, ast.Dict)
        and {key.value for key in node.keys if isinstance(key, ast.Constant)
             and isinstance(key.value, str)} >= {"id", "rootId", "epoch", "status"}
    ]
    if len(matches) != 1:
        raise AssertionError(f"Expected one agent creation literal, found {len(matches)}")
    return {
        key.value for key in matches[0].keys
        if isinstance(key, ast.Constant) and isinstance(key.value, str)
    }


class AgentRecordContractTests(unittest.TestCase):
    def test_record_entity_field_partition_is_explicit_and_complete(self) -> None:
        record_fields = set(AgentRecord.__annotations__)
        entity_fields = set(AgentEntityDto.model_fields)
        shared = AGENT_FIELD_GROUPS["shared"]
        private = AGENT_FIELD_GROUPS["private"]
        derived = AGENT_FIELD_GROUPS["derived"]
        self.assertFalse(shared & private)
        self.assertFalse(shared & derived)
        self.assertFalse(private & derived)
        self.assertEqual(shared | private, record_fields)
        self.assertEqual(shared | derived, entity_fields)

    def test_derived_group_cannot_contain_stored_fields(self) -> None:
        self.assertFalse(AGENT_FIELD_GROUPS["derived"] & set(AgentRecord.__annotations__))

    def test_entity_shared_fields_accept_the_declared_stored_types(self) -> None:
        stored = get_type_hints(AgentRecord)
        # The entity narrows these three stored objects to the keys the renderer
        # reads; the projection omits and reports a value that does not fit.
        narrowed = {"capacityRetry", "contextRepairWait", "usageResume"}
        exceptions = {"nativeRelease", "startAttempt"} | narrowed
        for name in sorted(AGENT_FIELD_GROUPS["shared"] - exceptions):
            with self.subTest(field=name):
                self.assertTrue(
                    _compatible(stored[name], AgentEntityDto.model_fields[name].annotation),
                    f"stored {name}: {stored[name]!r} is incompatible with "
                    f"AgentEntityDto: {AgentEntityDto.model_fields[name].annotation!r}",
                )

    def test_entity_compatibility_does_not_drop_stored_none(self) -> None:
        self.assertFalse(_compatible(str | None, str))
        self.assertTrue(_compatible(str | None, str | None))
        exceptions = {"nativeRelease", "startAttempt"}
        for name, annotation in get_type_hints(AgentRecord).items():
            if (name not in exceptions and type(None) in _union_members(annotation)
                    and name in AgentEntityDto.model_fields):
                self.assertTrue(
                    _compatible(annotation, AgentEntityDto.model_fields[name].annotation),
                    f"DTO narrows nullable stored field {name}",
                )

    def test_closed_literals_match_wire_enums_and_status_sets(self) -> None:
        def literal_values(alias: Any) -> set[str]:
            return set(get_args(alias))

        self.assertEqual(literal_values(AgentStatusValue), {member.value for member in AgentStatus})
        self.assertEqual(literal_values(AgentProviderValue), {member.value for member in AgentProvider})
        self.assertEqual(literal_values(AgentRoleValue), {member.value for member in AgentRole})
        self.assertEqual(literal_values(AgentModeValue), {member.value for member in AgentMode})
        self.assertEqual(literal_values(WorkStatusValue), {"ready", "running", "blocked", "review", "accepted", "cancelled"})
        self.assertEqual(literal_values(UsageResumeStatusValue), {"scheduled", "started", "cancelled", "unknown"})
        self.assertEqual(literal_values(UsageResumeCauseValue), {"usage_limit", "rate_limit", "auth"})
        self.assertEqual(literal_values(ComplaintStatusValue), {"open", "in_progress", "resolved", "declined"})
        self.assertEqual(
            literal_values(WorkspaceOperationPhase),
            {"capture_pending", "capture_running", "completed", "failed", "local_mutation", "reserved", "running",
             "provider_pending", "provider_ready", "recovery_required"},
        )

    def test_creation_literal_keys_are_declared(self) -> None:
        self.assertEqual({"id", "rootId", "epoch", "status"} & AgentRecord.__required_keys__,
                         {"id", "rootId", "epoch", "status"})
        self.assertTrue(set(_creation_literal_keys()) <= set(AgentRecord.__annotations__))
        self.assertEqual(AgentRecord.__optional_keys__, set(AgentRecord.__annotations__) - AgentRecord.__required_keys__)

    def test_measured_mismatches_are_declared_as_stored(self) -> None:
        hints = get_type_hints(AgentRecord)
        self.assertEqual(hints["nativeNameSynced"], NativeNameSyncedRecord | None)
        self.assertEqual(hints["workerBaseBehindMain"], int | None)
        self.assertNotIn("nativeNameSynced", AgentEntityDto.model_fields)
        self.assertNotIn("workerBaseBehindMain", AgentEntityDto.model_fields)
        self.assertEqual(
            KNOWN_WIRE_DISAGREEMENTS.keys(),
            {"nativeNameSynced", "workerBaseBehindMain", "error", "worktree", "startAttempt", "nativeRelease", "status"},
        )


if __name__ == "__main__":
    unittest.main()
