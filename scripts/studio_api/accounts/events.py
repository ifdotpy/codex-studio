"""Typed resource publications for account and provider state changes."""

from __future__ import annotations

import logging
from pathlib import Path

from studio_api.sync.resources.models import (
    AccountsResource,
    LimitsResource,
    ModelsResource,
    ResourceRef,
)

_LOGGER = logging.getLogger(__name__)


def publish_account_change(state_dir: str | Path) -> None:
    """Notify subscribers that saved accounts or login receipts changed."""
    _publish(state_dir, ResourceRef(AccountsResource(kind="accounts")))


def publish_limits_change(state_dir: str | Path, account_key: str) -> None:
    """Notify subscribers that one account's usage limits changed."""
    _publish(state_dir, ResourceRef(LimitsResource(kind="limits", accountKey=account_key)))


def publish_models_change(state_dir: str | Path) -> None:
    """Notify subscribers that the provider model catalog changed."""
    _publish(state_dir, ResourceRef(ModelsResource(kind="models")))


def _publish(state_dir: str | Path, *resources: ResourceRef) -> None:
    """Resolve the sync publisher only when a producer emits a change."""
    from studio_api.sync.resources.hub import publish_resources

    try:
        publish_resources(state_dir, *resources)
    except Exception:
        _LOGGER.exception("Could not publish a UI resource invalidation")
