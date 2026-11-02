"""The exact-origin grant `allowed_origins` names, parsed by the rule the run applies (the Rust core's)."""

import importlib
from typing import Any

# Compiled, so the checker has no types for it.
_core: Any = importlib.import_module("fastbrowse._native")


def origin_of(url: str) -> str | None:
    """`scheme://host[:port]` as Chrome serializes a document's origin, or None for anything not http(s)."""
    return _core.origin_of(url)


def parse_origin(value: str) -> str:
    """The normalized origin an `allowed_origins` entry names; `ValueError` for a wildcard, userinfo or path."""
    return _core.parse_origin(value)
