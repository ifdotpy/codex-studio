"""Preserve replay request order while allowing recorded concurrent turns."""

from __future__ import annotations

from collections.abc import Callable, Iterable, Collection
from concurrent.futures import ThreadPoolExecutor
import json


def dispatch_rpc_lines(
    lines: Iterable[str],
    handler: Callable[[str], None],
    concurrent_methods: Collection[str],
    on_line: Callable[[str], None] | None = None,
) -> None:
    """Handle ordinary requests in wire order; run declared concurrent RPCs separately."""
    futures = []
    first_error: Exception | None = None
    with ThreadPoolExecutor(max_workers=16) as pool:
        for line in lines:
            if on_line is not None:
                on_line(line)
            message = json.loads(line)
            if message.get("method") in concurrent_methods:
                futures.append(pool.submit(handler, line))
            else:
                try:
                    handler(line)
                except Exception as error:
                    if first_error is None:
                        first_error = error
        for future in futures:
            try:
                future.result()
            except Exception as error:
                if first_error is None:
                    first_error = error
    if first_error is not None:
        raise first_error
