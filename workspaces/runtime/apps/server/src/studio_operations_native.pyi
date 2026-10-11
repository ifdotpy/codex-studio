from typing import Literal

EventName = Literal[
    "Admit",
    "ClaimDispatch",
    "Cancel",
    "ServerRestarted",
    "CommittedOperationReceipt",
    "AppliedResponse",
    "DefinitiveRejection",
    "Failure",
    "ReceiptMissing",
]
ProofName = Literal[
    "QueuedCancellation",
    "ReadOnlyRejection",
    "ExactPreWriteRejection",
    "ReviewChildAbsent",
]
ResultKind = Literal[
    "Transitioned",
    "ReturnExisting",
    "NoEffect",
    "ReceiptMissingUnknown",
    "Rejected",
]
PersistedStage = Literal[
    "queued", "running", "cancelled", "interrupted", "failed", "unknown", "completed"
]
PersistedOutcome = Literal["pending", "not_applied", "applied", "unknown"]

PROTOCOL_VERSION: int
EVENT_NAMES: list[EventName]
PROOF_NAMES: list[ProofName]
RESULT_NAMES: list[ResultKind]
PERSISTED_STAGES: list[PersistedStage]
PERSISTED_OUTCOMES: list[PersistedOutcome]

class TransitionResult:
    @property
    def kind(self) -> ResultKind: ...
    @property
    def rejection_code(self) -> str | None: ...
    @property
    def rejection_message(self) -> str | None: ...
    @property
    def stage(self) -> PersistedStage | None: ...
    @property
    def outcome(self) -> PersistedOutcome | None: ...
    @property
    def cancel_requested(self) -> bool | None: ...

def transition_record(
    record_json: str,
    event: EventName,
    fingerprint: bytes,
    *,
    proof: ProofName | None = ...,
    empty: bool = ...,
    generation: int | None = ...,
    admission_fingerprint: bytes | None = ...,
) -> TransitionResult: ...
