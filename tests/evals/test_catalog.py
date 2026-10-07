import pytest
from pydantic import ValidationError

from fastbrowse.evals.catalog import CATALOG, Catalog


def test_catalog_rejects_a_grader_identity_not_in_the_version_lock() -> None:
    data = CATALOG.model_dump(mode="json")
    data["suites"]["core"][0]["source_fingerprint"] = "0" * 16
    with pytest.raises(ValidationError, match="task-version lock"):
        Catalog.model_validate(data)


def test_catalog_rejects_unknown_arms() -> None:
    data = CATALOG.model_dump(mode="json")
    data["suites"]["core"][0]["arms"] = ["invented-agent"]
    with pytest.raises(ValidationError, match="arm"):
        Catalog.model_validate(data)
