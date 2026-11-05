"""Validate saved recipes and export their file schema through the Rust core."""

import importlib
import json
from typing import Any

_core: Any = importlib.import_module("fastbrowse._native")


def validate_recipe(value: str | bytes | dict[str, Any]) -> dict[str, Any]:
    """Return a normalized recipe, or raise `ValueError` for an unsupported or invalid document.

    This checks the file format, including its version and unknown fields. It does not establish that
    a target exists on a live page or authorize an action. Execution still makes those checks.
    """
    if isinstance(value, bytes):
        value = value.decode("utf-8")
    text = value if isinstance(value, str) else json.dumps(value, allow_nan=False)
    return json.loads(_core.validate_recipe(text))


def recipe_schema() -> dict[str, Any]:
    """Return the JSON Schema for the recipe format this core reads."""
    return json.loads(_core.recipe_schema())
