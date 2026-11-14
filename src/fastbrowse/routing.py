"""The OpenRouter provider routing a run sends for a model, built by the Rust core that sends it."""

import importlib
import json
from typing import Any

# Compiled, so the checker has no types for it.
_core: Any = importlib.import_module("fastbrowse._native")


def openrouter_routing(model: str) -> dict[str, Any]:
    """The `provider` object a run keyed with OPENROUTER_API_KEY sends for `model`, endpoint pins included."""
    return json.loads(_core.openrouter_routing(model))
