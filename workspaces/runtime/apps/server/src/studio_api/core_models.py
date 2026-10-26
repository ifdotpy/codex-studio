"""Typed response models for core-owned API routes."""
from __future__ import annotations

from studio_api.models import ResponseModel


class SessionResponse(ResponseModel):
    token: str
