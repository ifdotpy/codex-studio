"""Bounded in-process fanout for committed typed resource invalidations."""

from __future__ import annotations

import asyncio
from collections.abc import Iterable, Sequence
from contextlib import closing
import logging
from pathlib import Path
import sqlite3
import threading
import time
from typing import Callable, Literal, Protocol
from uuid import uuid4

from pydantic import ValidationError

from studio_api.models import SyncEntity
from studio_api.sync.entity_response import response_entity_document

from studio_api.sync.resources.models import (
    MAX_SAFE_REVISION,
    MAX_ENTITY_CHANGE_BYTES,
    MAX_ENTITY_CHANGE_DOCUMENTS,
    EntityChangeBatch,
    AccountsResource,
    CostsResource,
    DesktopResource,
    DraftsResource,
    LimitsResource,
    ModelsResource,
    PanelResource,
    QueueResource,
    ReceiptsResource,
    RoomResource,
    ResourceChangeEvent,
    ResourceHeartbeatEvent,
    ResourceRef,
    ResourceRefValue,
    ResourceRevisionEntry,
    ResourceTokenRatesEvent,
    SessionCostResource,
    StateResource,
    TranscriptsResource,
    TaskResource,
    TasksResource,
    TerminalResource,
    TerminalsResource,
    TranscriptResource,
    TokenRateSnapshot,
    VoiceResource,
    WorkspaceResource,
)

MAX_SUBSCRIBED_RESOURCES = 256
MAX_PENDING_RESOURCES = 128
MAX_RESOURCE_REVISION_ENTRIES = 4096
ENTITY_SEQUENCE_THROTTLE_SECONDS = 0.1
# Bound the trailing quiet window so continuous entity writes still reach peers.
MAX_ENTITY_SEQUENCE_DELAY_SECONDS = 0.5
MAX_ENTITY_SEQUENCE_IDS = 512
_LOGGER = logging.getLogger(__name__)

ResourceKey = tuple[str, str | None]


class ProgressWatchdog(Protocol):
    def subscribe(self, agent_id: str, on_change: Callable[[], None]) -> Callable[[], None]: ...


class LazyProgressWatchdog:
    """Load the native progress watcher only when a panel has a live subscriber."""

    def __init__(self, state_dir: str | Path) -> None:
        self._state_dir = Path(state_dir)
        self._watchdog: ProgressWatchdog | None = None
        self._lock = threading.Lock()

    def subscribe(self, agent_id: str, on_change: Callable[[], None]) -> Callable[[], None]:
        with self._lock:
            if self._watchdog is None:
                from codex_progress_watch import ProgressFileWatchdog

                self._watchdog = ProgressFileWatchdog(self._state_dir)
            watchdog = self._watchdog
        return watchdog.subscribe(agent_id, on_change)


def _key(resource: ResourceRef) -> ResourceKey:
    value: ResourceRefValue = resource.root
    match value:
        case PanelResource(agentId=identity):
            return "panel", identity
        case QueueResource(agentId=identity):
            return "queue", identity
        case ReceiptsResource(agentId=identity):
            return "receipts", identity
        case TerminalResource(terminalId=identity):
            return "terminal", identity
        case LimitsResource(accountKey=identity):
            return "limits", identity
        case TasksResource(agentId=identity):
            return "tasks", identity
        case TaskResource(taskId=identity):
            return "task", identity
        case WorkspaceResource(agentId=identity):
            return "workspace", identity
        case VoiceResource(agentId=identity):
            return "voice", identity
        case SessionCostResource(agentId=identity):
            return "session-cost", identity
        case RoomResource(roomId=identity):
            return "room", identity
        case (TerminalsResource() | AccountsResource() | ModelsResource() | CostsResource()
              | DesktopResource() | StateResource() | DraftsResource() | TranscriptsResource()):
            return value.kind, None
        case TranscriptResource(agentId=identity):
            return "transcript", identity
    raise TypeError("Unsupported resource reference")


def _panel_agent(key: ResourceKey) -> str | None:
    kind, identity = key
    return identity if kind == "panel" else None


def _merge_entity_changes(
    previous: EntityChangeBatch | None, current: EntityChangeBatch | None,
) -> EntityChangeBatch | None:
    if previous is None or current is None or previous.through != current.after:
        return None
    documents = {document.id: document for document in previous.documents}
    documents.update((document.id, document) for document in current.documents)
    if len(documents) > MAX_ENTITY_CHANGE_DOCUMENTS:
        return None
    try:
        return EntityChangeBatch(
            after=previous.after,
            through=current.through,
            documents=sorted(documents.values(), key=lambda document: document.seq),
        )
    except ValidationError:
        return None


def _read_entity_changes(
    database: sqlite3.Connection, after: int, through: int,
    lengths: Sequence[tuple[int, int | None, int]],
) -> EntityChangeBatch | None:
    """Load bounded rows only after checking their stored byte lengths."""
    if (len(lengths) > MAX_ENTITY_CHANGE_DOCUMENTS
            or any(length is None for _, length, _ in lengths)
            or sum((length or 0) + identity_bytes + 100
                   for _, length, identity_bytes in lengths) > MAX_ENTITY_CHANGE_BYTES):
        return None
    rows = database.execute(
        "SELECT collection,id,seq,payload,deleted FROM sync_entities "
        "WHERE collection NOT LIKE 'transcript:%' AND seq>? AND seq<=? ORDER BY seq",
        (after, through),
    ).fetchall()
    from codex_sync_entities import _report_bad_entity, validate_stored_entity_payload

    documents: list[SyncEntity] = []
    for collection, identity, sequence, payload, deleted in rows:
        try:
            validate_stored_entity_payload(payload, collection, identity, bool(deleted))
        except (TypeError, ValueError) as error:
            _report_bad_entity(collection, identity, error)
            return None
        document = response_entity_document({
            "id": f"entity:{collection}:{identity}", "payload": payload,
            "seq": sequence, "_deleted": bool(deleted),
        })
        if document is None:
            return None
        documents.append(SyncEntity.model_validate(document))
    try:
        return EntityChangeBatch(after=after, through=through, documents=documents)
    except ValidationError:
        return None


class ResourceSubscription:
    """One bounded stream subscription with coalesced pending invalidations."""

    def __init__(
        self,
        hub: ResourceHub,
        loop: asyncio.AbstractEventLoop,
        resources: dict[ResourceKey, ResourceRef],
    ) -> None:
        self._hub = hub
        self._loop = loop
        self._resources = resources
        self._pending: dict[ResourceKey, ResourceRef] = {}
        self._pending_entity_sequences: set[int] = set()
        self._pending_entity_sequence_reset = False
        self._pending_entity_changes: EntityChangeBatch | None = None
        self._overflow = False
        self._pending_token_rates: TokenRateSnapshot | None = None
        self._wake = asyncio.Event()
        self._wake_scheduled = False
        self._closed = False
        self._watch_detach: list[Callable[[], None]] = []
        self.initial: ResourceChangeEvent
        self.initial_token_rates: ResourceTokenRatesEvent

    async def next_event(
        self, timeout: float | None = None
    ) -> ResourceChangeEvent | ResourceTokenRatesEvent | None:
        """Wait for a change; return None only when the supplied deadline expires."""
        if timeout is None:
            await self._wake.wait()
        else:
            try:
                await asyncio.wait_for(self._wake.wait(), timeout)
            except TimeoutError:
                return None
        with self._hub._lock:
            if self._closed:
                return None
            if self._overflow:
                reason: Literal["initial", "change", "reconnect", "overflow", "workspace"] = "overflow"
                changed = list(self._resources.values())
                self._overflow = False
                self._pending.clear()
                self._pending_entity_sequences.clear()
                self._pending_entity_sequence_reset = False
                self._pending_entity_changes = None
                event: ResourceChangeEvent | ResourceTokenRatesEvent = self._hub._event(reason, changed)
            elif self._pending_token_rates is not None:
                snapshot = self._pending_token_rates
                self._pending_token_rates = None
                event = self._hub._token_rates_event(snapshot)
            elif self._pending:
                changed = list(self._pending.values())
                self._pending.clear()
                entity_sequences = sorted(self._pending_entity_sequences)
                entity_sequence_reset = self._pending_entity_sequence_reset
                entity_changes = self._pending_entity_changes
                self._pending_entity_sequences.clear()
                self._pending_entity_sequence_reset = False
                self._pending_entity_changes = None
                event = self._hub._event(
                    "change",
                    changed,
                    entity_sequences=entity_sequences,
                    entity_sequence_reset=entity_sequence_reset,
                    entity_changes=entity_changes,
                )
            else:
                self._wake.clear()
                return None
            if self._pending or self._overflow or self._pending_token_rates is not None:
                self._hub._schedule_wake(self)
            else:
                self._wake.clear()
            return event

    def heartbeat(self) -> ResourceHeartbeatEvent:
        with self._hub._lock:
            return self._hub._heartbeat()

    def close(self) -> None:
        self._hub._close(self)

    def _signal(self) -> None:
        with self._hub._lock:
            self._wake_scheduled = False
            if not self._closed:
                self._wake.set()


class ResourceHub:
    """Publish committed resource changes and retain only bounded live fanout."""

    def __init__(
        self,
        workspace_id: str,
        progress_watchdog: ProgressWatchdog | None = None,
        token_rates: TokenRateSnapshot | None = None,
        entity_sequence: int = 0,
    ) -> None:
        if not workspace_id:
            raise ValueError("Workspace identity must not be empty")
        self.workspace_id = workspace_id
        self.epoch = uuid4().hex
        self._revision = 0
        self._entity_sequence = entity_sequence
        self._published_entity_sequence = entity_sequence
        self._entity_sequence_reset_pending = False
        self._resource_revisions: dict[ResourceKey, int] = {}
        self._progress_watchdog = progress_watchdog
        self._token_rates = token_rates or TokenRateSnapshot(rates={}, teams={})
        self._token_rates_published = token_rates is not None
        self._lock = threading.RLock()
        self._subscriptions: set[ResourceSubscription] = set()
        self._orphaned_subscriptions: set[ResourceSubscription] = set()

    def subscribe(
        self,
        resources: Sequence[ResourceRef],
        *,
        loop: asyncio.AbstractEventLoop,
        reconnect: bool = False,
        token_rates: TokenRateSnapshot | None = None,
        progress_agent_ids: frozenset[str] | None = None,
    ) -> ResourceSubscription:
        keyed: dict[ResourceKey, ResourceRef] = {}
        for resource in resources:
            key = _key(resource)
            if key[1] == "":
                raise ValueError("Resource identity must not be empty")
            keyed[key] = resource
        if not keyed:
            raise ValueError("At least one resource subscription is required")
        if len(keyed) > MAX_SUBSCRIBED_RESOURCES:
            raise ValueError("Too many resource subscriptions")

        subscription = ResourceSubscription(self, loop, keyed)
        with self._lock:
            for key in keyed:
                if key[0] != "state":
                    revision = self._resource_revisions.pop(key, self._revision)
                    self._resource_revisions[key] = revision
            while len(self._resource_revisions) > MAX_RESOURCE_REVISION_ENTRIES:
                self._resource_revisions.pop(next(iter(self._resource_revisions)))
            self._subscriptions.add(subscription)

        # Never enter the native watchdog while holding the hub lock. Its
        # observer thread may be dispatching a progress callback concurrently.
        try:
            for key, resource in keyed.items():
                agent_id = _panel_agent(key)
                if agent_id is None or self._progress_watchdog is None:
                    continue
                if progress_agent_ids is not None and agent_id not in progress_agent_ids:
                    continue
                detach = self._progress_watchdog.subscribe(
                    agent_id,
                    lambda resource=resource: self.publish(resource),  # type: ignore[arg-type,misc]
                )
                if not callable(detach):
                    raise RuntimeError("Progress watcher returned no detach callback")
                subscription._watch_detach.append(detach)
        except Exception:
            with self._lock:
                subscription._closed = True
                self._subscriptions.discard(subscription)
                detach_callbacks, subscription._watch_detach = subscription._watch_detach, []
            self._detach_all(detach_callbacks)
            raise

        with self._lock:
            if subscription._closed:
                detach_callbacks, subscription._watch_detach = subscription._watch_detach, []
            else:
                detach_callbacks = []
            if subscription._closed:
                reason: Literal["initial", "change", "reconnect", "overflow", "workspace"] = "initial"
            else:
                reason = "reconnect" if reconnect else "initial"
                subscription.initial = self._event(reason, list(keyed.values()))
                if self._entity_sequence_reset_pending:
                    for resource, version in zip(
                        subscription.initial.resources,
                        subscription.initial.resourceVersions,
                        strict=True,
                    ):
                        if _key(resource)[0] == "state":
                            version.entitySequenceReset = True
                if token_rates is not None and not self._token_rates_published:
                    self._token_rates = token_rates
                    self._token_rates_published = True
                subscription.initial_token_rates = self._token_rates_event(self._token_rates)
                # Watch callbacks during registration are covered by this full baseline.
                subscription._pending.clear()
                subscription._pending_entity_sequences.clear()
                subscription._pending_entity_sequence_reset = False
                subscription._pending_entity_changes = None
                subscription._overflow = False
                subscription._wake.clear()
        if subscription._closed:
            self._detach_all(detach_callbacks)
            raise RuntimeError("Resource subscription closed during registration")
        return subscription

    @staticmethod
    def _detach_all(detach_callbacks: Sequence[Callable[[], None]]) -> None:
        for detach in detach_callbacks:
            try:
                detach()
            except Exception:
                _LOGGER.exception("Unable to detach progress watcher")

    def publish(self, resource: ResourceRef) -> int:
        return self.publish_many((resource,))

    def publish_entity_sequence(
        self,
        sequence: int,
        entity_sequences: Sequence[int] = (),
        *,
        reset: bool = False,
        entity_changes: EntityChangeBatch | None = None,
    ) -> int:
        """Publish one committed state interval, optionally with its complete rows."""
        if sequence <= 0 and not reset:
            return self._revision
        with self._lock:
            unpublished = {
                value
                for value in entity_sequences
                if value > self._published_entity_sequence
            }
            if sequence <= self._published_entity_sequence and not unpublished and not reset:
                return self._revision
            if not unpublished and not reset:
                return self._revision
            previous_sequence = self._published_entity_sequence
            if (entity_changes is not None and (
                entity_changes.after != previous_sequence or entity_changes.through != sequence
            )):
                entity_changes = None
            self._entity_sequence = max(self._entity_sequence, sequence)
            self._published_entity_sequence = max(
                self._published_entity_sequence, sequence
            )
            committed_sequences = sorted(unpublished)
            reset = reset or len(committed_sequences) > MAX_ENTITY_SEQUENCE_IDS
            if reset:
                committed_sequences = []
                self._entity_sequence_reset_pending = True
            elif committed_sequences:
                self._entity_sequence_reset_pending = False
            resource = ResourceRef(StateResource(kind="state"))
            key = _key(resource)
            self._advance_revision()
            merged_batches: dict[int, EntityChangeBatch | None] = {}
            for subscription in self._subscriptions:
                if key not in subscription._resources:
                    continue
                if subscription._overflow:
                    continue
                state_was_pending = key in subscription._pending
                subscription._pending[key] = resource
                if reset:
                    subscription._pending_entity_sequences.clear()
                    subscription._pending_entity_sequence_reset = True
                    subscription._pending_entity_changes = None
                elif not subscription._pending_entity_sequence_reset:
                    subscription._pending_entity_sequences.update(committed_sequences)
                    if len(subscription._pending_entity_sequences) > MAX_ENTITY_SEQUENCE_IDS:
                        subscription._pending_entity_sequences.clear()
                        subscription._pending_entity_sequence_reset = True
                    if state_was_pending:
                        previous = subscription._pending_entity_changes
                        if id(previous) not in merged_batches:
                            merged_batches[id(previous)] = _merge_entity_changes(previous, entity_changes)
                        subscription._pending_entity_changes = merged_batches[id(previous)]
                    else:
                        subscription._pending_entity_changes = entity_changes
                    if subscription._pending_entity_sequence_reset:
                        subscription._pending_entity_changes = None
                if len(subscription._pending) > MAX_PENDING_RESOURCES:
                    subscription._pending.clear()
                    subscription._pending_entity_sequences.clear()
                    subscription._pending_entity_sequence_reset = False
                    subscription._pending_entity_changes = None
                    subscription._overflow = True
                self._schedule_wake(subscription)
            revision = self._revision
        self._close_orphaned_subscriptions()
        return revision

    def _resource_revision(self, resource: ResourceRef) -> int:
        key = _key(resource)
        if key[0] == "state":
            return self._entity_sequence
        return self._resource_revisions.get(key, self._revision)

    def close(self) -> None:
        """Release all live subscriptions without entering watchers under lock."""
        with self._lock:
            subscriptions = list(self._subscriptions | self._orphaned_subscriptions)
            self._orphaned_subscriptions.clear()
        for subscription in subscriptions:
            subscription.close()

    def _close_orphaned_subscriptions(self) -> None:
        with self._lock:
            subscriptions = list(self._orphaned_subscriptions)
            self._orphaned_subscriptions.clear()
        for subscription in subscriptions:
            subscription.close()

    def publish_token_rates(self, snapshot: TokenRateSnapshot) -> int:
        with self._lock:
            if snapshot == self._token_rates:
                return self._revision
            self._token_rates = snapshot
            self._token_rates_published = True
            self._advance_revision()
            for subscription in self._subscriptions:
                subscription._pending_token_rates = snapshot
                self._schedule_wake(subscription)
            revision = self._revision
        self._close_orphaned_subscriptions()
        return revision

    def publish_many(self, resources: Iterable[ResourceRef]) -> int:
        keyed: dict[ResourceKey, ResourceRef] = {}
        for resource in resources:
            key = _key(resource)
            if key[1] == "":
                raise ValueError("Resource identity must not be empty")
            if key[0] == "state":
                raise ValueError("State resources must publish an entity sequence")
            keyed[key] = resource
        if not keyed:
            with self._lock:
                return self._revision
        with self._lock:
            self._advance_revision()
            for key in keyed:
                if key[0] != "state":
                    self._resource_revisions.pop(key, None)
                    self._resource_revisions[key] = self._revision
            while len(self._resource_revisions) > MAX_RESOURCE_REVISION_ENTRIES:
                self._resource_revisions.pop(next(iter(self._resource_revisions)))
            for subscription in self._subscriptions:
                matching = ((key, value) for key, value in keyed.items() if key in subscription._resources)
                for key, value in matching:
                    if subscription._overflow:
                        break
                    subscription._pending[key] = value
                    if len(subscription._pending) > MAX_PENDING_RESOURCES:
                        subscription._pending.clear()
                        subscription._pending_entity_sequences.clear()
                        subscription._pending_entity_sequence_reset = False
                        subscription._pending_entity_changes = None
                        subscription._overflow = True
                        break
                if subscription._pending or subscription._overflow:
                    self._schedule_wake(subscription)
            revision = self._revision
        self._close_orphaned_subscriptions()
        return revision

    def publish_overflow(self) -> int:
        """Force each live reader to re-read its complete subscribed set."""
        with self._lock:
            self._advance_revision()
            for subscription in self._subscriptions:
                for key in subscription._resources:
                    if key[0] != "state":
                        self._resource_revisions[key] = self._revision
            while len(self._resource_revisions) > MAX_RESOURCE_REVISION_ENTRIES:
                self._resource_revisions.pop(next(iter(self._resource_revisions)))
            for subscription in self._subscriptions:
                subscription._pending.clear()
                subscription._pending_entity_sequences.clear()
                subscription._pending_entity_sequence_reset = False
                subscription._pending_entity_changes = None
                subscription._overflow = True
                self._schedule_wake(subscription)
            revision = self._revision
        self._close_orphaned_subscriptions()
        return revision

    def _schedule_wake(self, subscription: ResourceSubscription) -> None:
        if subscription._wake_scheduled:
            return
        subscription._wake_scheduled = True
        try:
            subscription._loop.call_soon_threadsafe(subscription._signal)
        except RuntimeError:
            subscription._wake_scheduled = False
            self._orphaned_subscriptions.add(subscription)

    def _event(
        self,
        reason: Literal["initial", "change", "reconnect", "overflow", "workspace"],
        resources: list[ResourceRef],
        *,
        entity_sequences: Sequence[int] = (),
        entity_sequence_reset: bool = False,
        entity_changes: EntityChangeBatch | None = None,
    ) -> ResourceChangeEvent:
        return ResourceChangeEvent(
            protocol=3,
            workspaceId=self.workspace_id,
            epoch=self.epoch,
            revision=self._revision,
            reason=reason,
            resources=resources,
            resourceVersions=[
                ResourceRevisionEntry(
                    revision=self._resource_revision(resource),
                    entitySequences=(
                        list(entity_sequences)
                        if _key(resource)[0] == "state" and entity_sequences
                        else None
                    ),
                    entityChanges=(
                        entity_changes if _key(resource)[0] == "state" else None
                    ),
                    entitySequenceReset=(
                        entity_sequence_reset
                        if _key(resource)[0] == "state"
                        else None
                    ),
                )
                for resource in resources
            ],
        )

    def _heartbeat(self) -> ResourceHeartbeatEvent:
        return ResourceHeartbeatEvent(
            protocol=3,
            workspaceId=self.workspace_id,
            epoch=self.epoch,
            revision=self._revision,
        )

    def _token_rates_event(self, snapshot: TokenRateSnapshot) -> ResourceTokenRatesEvent:
        return ResourceTokenRatesEvent(
            protocol=3,
            workspaceId=self.workspace_id,
            epoch=self.epoch,
            revision=self._revision,
            rates=snapshot.rates,
            teams=snapshot.teams,
        )

    def _advance_revision(self) -> None:
        if self._revision >= MAX_SAFE_REVISION:
            self.epoch = uuid4().hex
            self._revision = 0
            for subscription in self._subscriptions:
                subscription._pending.clear()
                subscription._overflow = True
        self._revision += 1

    def _close(self, subscription: ResourceSubscription) -> None:
        with self._lock:
            if subscription._closed:
                return
            subscription._closed = True
            self._subscriptions.discard(subscription)
            detach_callbacks, subscription._watch_detach = subscription._watch_detach, []
        self._detach_all(detach_callbacks)


_registry_lock = threading.RLock()
_hub_registry: dict[str, ResourceHub] = {}


def _root_key(state_dir: str | Path) -> str:
    return str(Path(state_dir).absolute())


def register_resource_hub(state_dir: str | Path, hub: ResourceHub) -> None:
    """Register the application-owned hub for post-commit producers."""
    with _registry_lock:
        _hub_registry[_root_key(state_dir)] = hub


def unregister_resource_hub(state_dir: str | Path, hub: ResourceHub) -> None:
    """Remove a hub only if it is still the registration for this state root."""
    with _registry_lock:
        key = _root_key(state_dir)
        if _hub_registry.get(key) is hub:
            _hub_registry.pop(key, None)


class EntityPublicationScheduler:
    """One process-wide worker coalesces committed entity sequence scans."""

    def __init__(
        self,
        *,
        clock: Callable[[], float] = time.monotonic,
        start_worker: bool = True,
    ) -> None:
        self._condition = threading.Condition()
        self._pending: dict[
            str, tuple[Path, float, float, int, set[int], bool]
        ] = {}
        self._worker: threading.Thread | None = None
        self._clock = clock
        self._start_worker = start_worker
        self._failure_attempts: dict[str, int] = {}

    def schedule(
        self,
        state_dir: str | Path,
        database_path: str | Path,
        sequence: int = 0,
        entity_sequences: Sequence[int] = (),
    ) -> None:
        root = _root_key(state_dir)
        database = Path(database_path)
        with self._condition:
            current = self._pending.get(root)
            now = self._clock()
            deadline = (
                current[2]
                if current is not None
                else now + MAX_ENTITY_SEQUENCE_DELAY_SECONDS
            )
            pending_sequence = max(sequence, current[3] if current else 0)
            pending_ids = set(current[4]) if current else set()
            pending_reset = current[5] if current else False
            if not pending_reset:
                pending_ids.update(entity_sequences)
                if len(pending_ids) > MAX_ENTITY_SEQUENCE_IDS:
                    pending_ids.clear()
                    pending_reset = True
            self._pending[root] = (
                database,
                min(now + ENTITY_SEQUENCE_THROTTLE_SECONDS, deadline),
                deadline,
                pending_sequence,
                pending_ids,
                pending_reset,
            )
            if self._start_worker and (
                self._worker is None or not self._worker.is_alive()
            ):
                self._worker = threading.Thread(
                    target=self._run,
                    name="codex-entity-publications",
                    daemon=True,
                )
                self._worker.start()
            self._condition.notify()

    def flush_due(self) -> int:
        """Publish currently due batches; exposed for deterministic clock tests."""
        due: list[tuple[str, tuple[Path, float, float, int, set[int], bool]]] = []
        now = self._clock()
        with self._condition:
            for root, pending in tuple(self._pending.items()):
                if pending[1] <= now:
                    due.append((root, pending))
                    del self._pending[root]
        for root, pending in due:
            try:
                self._publish(root, *pending)
            except Exception:
                self._retry_failed(root, pending)
            else:
                self._failure_attempts.pop(root, None)
        return len(due)

    def _retry_failed(
        self,
        root: str,
        pending: tuple[Path, float, float, int, set[int], bool],
    ) -> None:
        attempts = self._failure_attempts.get(root, 0) + 1
        self._failure_attempts[root] = attempts
        if attempts == 1:
            _LOGGER.exception("Unable to publish committed entity sequence")
        else:
            _LOGGER.warning(
                "Retrying committed entity publication (attempt %s)", attempts
            )
        database_path, _, _deadline, sequence, ids, reset = pending
        now = self._clock()
        retry_delay = min(0.25 * (2 ** min(attempts - 1, 5)), 5.0)
        with self._condition:
            current = self._pending.get(root)
            pending_sequence = max(sequence, current[3] if current else 0)
            pending_ids = set(ids) | (set(current[4]) if current else set())
            pending_reset = reset or bool(current and current[5])
            self._pending[root] = (
                database_path,
                now + retry_delay,
                now + MAX_ENTITY_SEQUENCE_DELAY_SECONDS,
                pending_sequence,
                pending_ids,
                pending_reset,
            )
            self._condition.notify()

    def _run(self) -> None:
        while True:
            with self._condition:
                while not self._pending:
                    self._condition.wait()
                root, pending = min(self._pending.items(), key=lambda item: item[1][1])
                remaining = pending[1] - self._clock()
                if remaining > 0:
                    self._condition.wait(remaining)
                    continue
                del self._pending[root]
            try:
                self._publish(root, *pending)
            except Exception:
                self._retry_failed(root, pending)
            else:
                self._failure_attempts.pop(root, None)

    @staticmethod
    def _publish(
        root: str,
        database_path: Path,
        _due_at: float,
        _deadline: float,
        explicit_sequence: int,
        explicit_sequences: set[int],
        explicit_reset: bool,
    ) -> None:
        with _registry_lock:
            hub = _hub_registry.get(root)
        if hub is None:
            return
        with hub._lock:
            watermark = hub._published_entity_sequence
            include_entity_changes = any(
                ("state", None) in subscription._resources and not subscription._overflow
                for subscription in hub._subscriptions
            )
        lengths: list[tuple[int, int | None, int]] = []
        sequence = explicit_sequence
        entity_changes: EntityChangeBatch | None = None
        floor = 0
        read_failed = False
        try:
            database_uri = database_path.resolve().as_uri() + "?mode=ro"
            with closing(sqlite3.connect(database_uri, uri=True, timeout=0.2)) as database:
                # One read snapshot proves both the interval and its complete rows.
                database.execute("BEGIN")
                row = database.execute(
                    "SELECT COALESCE(MAX(seq),0) FROM sync_entities "
                    "WHERE collection NOT LIKE 'transcript:%'",
                ).fetchone()
                from codex_sync_entities import entity_tombstone_floor

                floor = entity_tombstone_floor(database)
                snapshot_sequence = max(int(row[0]) if row else 0, floor)
                sequence = max(sequence, snapshot_sequence)
                # Inspect lengths first: oversized rows never enter Python memory.
                lengths = database.execute(
                    "SELECT seq,LENGTH(CAST(payload AS BLOB)),"
                    "LENGTH(CAST(collection AS BLOB))+LENGTH(CAST(id AS BLOB)) "
                    "FROM sync_entities WHERE collection NOT LIKE 'transcript:%' "
                    "AND seq>? AND seq<=? ORDER BY seq LIMIT ?",
                    (watermark, sequence, MAX_ENTITY_SEQUENCE_IDS + 1),
                ).fetchall()
                if (include_entity_changes and sequence > watermark and floor <= watermark
                        and sequence == snapshot_sequence and not explicit_reset):
                    entity_changes = _read_entity_changes(
                        database, watermark, sequence, lengths,
                    )
        except sqlite3.Error:
            entity_changes = None
            read_failed = True
            if sequence <= watermark and not explicit_reset:
                raise
        sequences = explicit_sequences | {int(item[0]) for item in lengths}
        sequences = {value for value in sequences if value > watermark}
        reset = explicit_reset or floor > watermark or len(lengths) > MAX_ENTITY_SEQUENCE_IDS
        reset = reset or len(sequences) > MAX_ENTITY_SEQUENCE_IDS
        reset = reset or (read_failed and sequence > watermark and not sequences)
        if not sequences and not reset:
            return
        if sequence <= watermark and not reset:
            return
        hub.publish_entity_sequence(
            sequence,
            () if reset else sorted(sequences),
            reset=reset,
            entity_changes=None if reset else entity_changes,
        )


_entity_publication_scheduler = EntityPublicationScheduler()


def publish_resources(state_dir: str | Path, *resources: ResourceRef) -> None:
    """Publish after a successful commit; pre-API writers safely have no hub."""
    with _registry_lock:
        hub = _hub_registry.get(_root_key(state_dir))
    if hub is not None:
        try:
            hub.publish_many(resources)
        except Exception:
            _LOGGER.exception("Unable to publish committed resource changes")


def schedule_entity_publication(state_dir: str | Path, database_path: str | Path) -> None:
    """Queue an instrumented commit for asynchronous sequence inspection."""
    _entity_publication_scheduler.schedule(state_dir, database_path)


def has_resource_hub(state_dir: str | Path) -> bool:
    """Avoid importing/starting publication machinery in standalone writers."""
    with _registry_lock:
        return _root_key(state_dir) in _hub_registry


def publish_resource_overflow(state_dir: str | Path) -> None:
    """Request a full subscribed-resource reconciliation after bounded staging overflows."""
    with _registry_lock:
        hub = _hub_registry.get(_root_key(state_dir))
    if hub is not None:
        try:
            hub.publish_overflow()
        except Exception:
            _LOGGER.exception("Unable to publish resource reconciliation")


def publish_token_rates(state_dir: str | Path, snapshot: TokenRateSnapshot) -> None:
    """Publish an actual changed token-rate snapshot without polling."""
    with _registry_lock:
        hub = _hub_registry.get(_root_key(state_dir))
    if hub is not None:
        try:
            hub.publish_token_rates(snapshot)
        except Exception:
            _LOGGER.exception("Unable to publish committed token-rate changes")
