"""The public recipe validation seam reaches the file loader's Rust contract."""

import json
from pathlib import Path

import pytest

from fastbrowse.recipes import recipe_schema, validate_recipe


def test_saved_recipe_schema_is_exported_by_the_core():
    committed = Path(__file__).parents[1] / "protocol" / "recipe.schema.json"
    assert recipe_schema() == json.loads(committed.read_text())


def test_recipe_validation_normalizes_defaults_without_executing():
    recipe = {"version": 1, "task": "Review", "steps": []}
    expected = {**recipe, "start": None}
    assert validate_recipe(recipe) == expected
    assert validate_recipe(json.dumps(recipe).encode()) == expected


@pytest.mark.parametrize("text", ["live", {"live": None}, {"literal": "hello"}, {"input": "name"}, {"secret": "key"}])
def test_recipe_text_variants_follow_the_file_format(text):
    recipe = {
        "version": 1,
        "task": "Review",
        "steps": [{"operation": "fill", "page": {"origin": "https://example.test", "path": "/"}, "text": text}],
    }
    step = validate_recipe(recipe)["steps"][0]
    assert step["text"] == ("live" if text == {"live": None} else text)


def test_positional_recipe_files_keep_their_existing_defaults():
    recipe = validate_recipe('[1,"Review",null,[["click",["https://example.test","/"],["button","Go"]]]]')
    step = recipe["steps"][0]
    assert step["target"]["label"] == "Go"
    assert step["page"]["query"] is None
    assert step["replay"] is True


@pytest.mark.parametrize(
    "change",
    [{"version": 2}, {"path": "/tmp/recipe.json"}, {"steps": [{"operation": "invent"}]}],
)
def test_recipe_validation_refuses_unsupported_versions_and_fields(change):
    with pytest.raises(ValueError):
        validate_recipe({"version": 1, "task": "Review", "steps": [], **change})
