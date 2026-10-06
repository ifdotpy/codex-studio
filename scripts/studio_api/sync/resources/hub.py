"""Bounded in-process fanout for committed typed resource invalidations."""

from __future__ import annotations

import asyncio
from collections.abc import Iterable, Sequence
import logging
from pathlib import Path
import threading
from typing import Callable, Literal, Protocol
from uuid import uuid4

from studio_api.sync.resources.models import (
    MAX_SAFE_REVISION,
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
    TaskResource,
    TasksResource,
    TerminalResource,
    TerminalsResource,
    TranscriptResource,
    TokenRateSnapshot,
    VoiceResource,
    WorktreeDiskResource,
    WorkspaceResource,
)

MAX_SUBSCRIBED_RESOURCES = 256
MAX_PENDING_RESOURCES = 128
MAX_RESOURCE_REVISION_ENTRIES = 4096
ENTITY_SEQUENCE_THROTTLE_SECONDS = 0.025
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
        case WorktreeDiskResource(agentId=identity):
            return "worktree-disk", identity
        case RoomResource(roomId=identity):
            return "room", identity
        case (TerminalsResource() | AccountsResource() | ModelsResource() | CostsResource()
              | DesktopResource() | StateResource() | DraftsResource()):
            return value.kind, None
        case TranscriptResource(agentId=identity):
            return "transcript", identity
    raise TypeError("Unsupported resource reference")


def _panel_agent(key: ResourceKey) -> str | None:
    kind, identity = key
    return identity if kind == "panel" else None


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
                event: ResourceChangeEvent | ResourceTokenRatesEvent = self._hub._event(reason, changed)
            elif self._pending_token_rates is not None:
                snapshot = self._pending_token_rates
                self._pending_token_rates = None
                event = self._hub._token_rates_event(snapshot)
            elif self._pending:
                changed = list(self._pending.values())
                self._pending.clear()
                entity_sequences = sorted(self._pending_entity_sequences)
                self._pending_entity_sequences.clear()
                event = self._hub._event(
                    "change", changed, entity_sequences=entity_sequences
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
        self._pending_entity_sequences: set[int] = set()
        self._entity_sequence_timer: threading.Timer | None = None
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
            self._subscriptions.add(subscription)
            self._prune_resource_revisions()

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
                if token_rates is not None and not self._token_rates_published:
                    self._token_rates = token_rates
                    self._token_rates_published = True
                subscription.initial_token_rates = self._token_rates_event(self._token_rates)
                # Watch callbacks during registration are covered by this full baseline.
                subscription._pending.clear()
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
        self, sequence: int, entity_sequences: Sequence[int] = ()
    ) -> int:
        """Schedule a coalesced state invalidation for a committed entity advance."""
        if sequence <= 0:
            return self._revision
        with self._lock:
            unpublished = {
                value
                for value in entity_sequences
                if value > self._published_entity_sequence
            }
            if sequence <= self._published_entity_sequence and not unpublished:
                return self._revision
            self._entity_sequence = max(self._entity_sequence, sequence)
            committed_sequences = unpublished
            if not committed_sequences and sequence > self._published_entity_sequence:
                committed_sequences.add(sequence)
            if not committed_sequences:
                return self._revision
            self._pending_entity_sequences.update(committed_sequences)
            if self._entity_sequence_timer is None:
                timer = threading.Timer(
                    ENTITY_SEQUENCE_THROTTLE_SECONDS,
                    self._flush_entity_sequence,
                )
                timer.daemon = True
                self._entity_sequence_timer = timer
                timer.start()
            return self._revision

    def _flush_entity_sequence(self) -> None:
        resource = ResourceRef(StateResource(kind="state"))
        key = _key(resource)
        with self._lock:
            self._entity_sequence_timer = None
            if self._entity_sequence <= self._published_entity_sequence:
                return
            self._published_entity_sequence = self._entity_sequence
            entity_sequences = tuple(sorted(self._pending_entity_sequences))
            self._pending_entity_sequences.clear()
            self._advance_revision()
            for subscription in self._subscriptions:
                if key not in subscription._resources:
                    continue
                if subscription._overflow:
                    continue
                subscription._pending[key] = resource
                subscription._pending_entity_sequences.update(entity_sequences)
                if len(subscription._pending) > MAX_PENDING_RESOURCES:
                    subscription._pending.clear()
                    subscription._pending_entity_sequences.clear()
                    subscription._overflow = True
                self._schedule_wake(subscription)

    def _resource_revision(self, resource: ResourceRef) -> int:
        key = _key(resource)
        if key[0] == "state":
            return self._entity_sequence
        return self._resource_revisions.get(key, self._revision)

    def _prune_resource_revisions(self) -> None:
        active = {
            key
            for subscription in self._subscriptions
            for key in subscription._resources
            if key[0] != "state"
        }
        inactive = [key for key in self._resource_revisions if key not in active]
        while len(self._resource_revisions) > MAX_RESOURCE_REVISION_ENTRIES and inactive:
            del self._resource_revisions[inactive.pop(0)]

    def close(self) -> None:
        """Release all live subscriptions without entering watchers under lock."""
        with self._lock:
            if self._entity_sequence_timer is not None:
                self._entity_sequence_timer.cancel()
                self._entity_sequence_timer = None
            self._pending_entity_sequences.clear()
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
            for subscription in self._subscriptions:
                matching = ((key, value) for key, value in keyed.items() if key in subscription._resources)
                for key, value in matching:
                    if subscription._overflow:
                        break
                    subscription._pending[key] = value
                    if len(subscription._pending) > MAX_PENDING_RESOURCES:
                        subscription._pending.clear()
                        subscription._pending_entity_sequences.clear()
                        subscription._overflow = True
                        break
                if subscription._pending or subscription._overflow:
                    self._schedule_wake(subscription)
            revision = self._revision
            self._prune_resource_revisions()
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
                subscription._pending.clear()
                subscription._pending_entity_sequences.clear()
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
                    resource=resource,
                    revision=self._resource_revision(resource),
                    entitySequences=(
                        list(entity_sequences)
                        if _key(resource)[0] == "state" and entity_sequences
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
            self._prune_resource_revisions()
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


def publish_resources(state_dir: str | Path, *resources: ResourceRef) -> None:
    """Publish after a successful commit; pre-API writers safely have no hub."""
    with _registry_lock:
        hub = _hub_registry.get(_root_key(state_dir))
    if hub is not None:
        try:
            hub.publish_many(resources)
        except Exception:
            _LOGGER.exception("Unable to publish committed resource changes")


def entity_sequence_watermark(state_dir: str | Path) -> int | None:
    """Return the active hub's last observed entity high-water mark, if any."""
    with _registry_lock:
        hub = _hub_registry.get(_root_key(state_dir))
    if hub is None:
        return None
    with hub._lock:
        return hub._entity_sequence


def publish_entity_sequence(
    state_dir: str | Path,
    sequence: int,
    entity_sequences: Sequence[int] = (),
) -> None:
    """Notify the active hub of a committed sync entity high-water mark."""
    with _registry_lock:
        hub = _hub_registry.get(_root_key(state_dir))
    if hub is not None:
        try:
            hub.publish_entity_sequence(sequence, entity_sequences)
        except Exception:
            _LOGGER.exception("Unable to schedule committed entity changes")


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
