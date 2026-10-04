"""Typed resource publications for account and provider state changes."""

from __future__ import annotations

from importlib import import_module
from pathlib import Path
from typing import Protocol, cast

from studio_api.sync.resources.models import (
    AccountsResource,
    LimitsResource,
    ModelsResource,
    ResourceRef,
)


class ResourcePublisher(Protocol):
    def __call__(self, state_dir: str | Path, *resources: ResourceRef) -> None: ...


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
    """Use the optional sync hub when installed; older runtimes remain valid."""
    try:
        hub = import_module("studio_api.sync.resources.hub")
    except ModuleNotFoundError as error:
        if error.name != "studio_api.sync.resources.hub":
            raise
        return
    publish_resources = cast(ResourcePublisher, getattr(hub, "publish_resources"))
    publish_resources(state_dir, *resources)
