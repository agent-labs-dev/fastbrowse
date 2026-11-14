"""Every third-party module a shipped module imports at import time is a declared dependency.

The wheel installs `fastbrowse` and nothing else: a module that imports a distribution the metadata does not
declare fails at import for anyone who installs the package, however well it works in a dev checkout where a
dev dependency happened to pull the distribution in (#764, `fastbrowse.evals.bench` importing `packaging`).
"""

import ast
import sys
import tomllib
from pathlib import Path

PYPROJECT = Path(__file__).resolve().parents[1] / "pyproject.toml"
PACKAGE = PYPROJECT.parent / "src" / "fastbrowse"

# Distributions whose import name is not their project name.
IMPORT_NAMES = {"pyyaml": "yaml", "pillow": "pil", "beautifulsoup4": "bs4"}

# Modules a declared distribution always installs, so declaring the parent covers importing the child.
ARRIVES_WITH = {"pydantic_core": "pydantic"}


def import_name(requirement: str) -> str:
    """The module a requirement's distribution is imported as."""
    project_name = requirement.split(";")[0].strip()
    for separator in ("[", " ", "=", "<", ">", "!", "~", "("):
        project_name = project_name.split(separator)[0]
    project_name = project_name.lower().replace("-", "_").replace(".", "_")
    return IMPORT_NAMES.get(project_name, project_name)


def declared_import_names() -> set[str]:
    """The import name of every distribution the metadata declares, runtime and extras alike.

    A distribution's own hard dependency is declared through it: `pydantic` always installs `pydantic_core`,
    which `fastbrowse/native.py` imports, so installing the wheel covers that import.
    """
    project = tomllib.loads(PYPROJECT.read_text(encoding="utf-8"))["project"]
    requirements = list(project.get("dependencies", ()))
    for extra in project.get("optional-dependencies", {}).values():
        requirements.extend(extra)
    return {import_name(requirement) for requirement in requirements}


def imported_modules() -> dict[str, list[str]]:
    """The third-party modules imported outside a function body, by file, low case."""
    declared = declared_import_names()
    imported: dict[str, list[str]] = {}
    for path in sorted(PACKAGE.rglob("*.py")):
        for node in ast.parse(path.read_text(encoding="utf-8")).body:
            if isinstance(node, ast.Import):
                roots = [alias.name.split(".")[0] for alias in node.names]
            elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                roots = [node.module.split(".")[0]]
            else:
                continue
            for root in roots:
                if root == "fastbrowse" or root in sys.stdlib_module_names:
                    continue
                if root.lower() in ARRIVES_WITH and ARRIVES_WITH[root.lower()] in declared:
                    continue
                imported.setdefault(root.lower(), []).append(str(path.relative_to(PYPROJECT.parent)))
    return imported


def test_every_module_level_import_is_a_declared_dependency() -> None:
    undeclared = {
        module: files for module, files in imported_modules().items() if module not in declared_import_names()
    }
    assert not undeclared, (
        "a module the wheel ships imports a distribution the metadata does not declare, so it fails at import "
        f"from an install: {undeclared}"
    )


def test_the_scan_sees_the_imports_it_is_meant_to_cover() -> None:
    imported = imported_modules()
    assert "pydantic" in imported
    assert any(file.endswith("evals/bench.py") for file in imported["packaging"])
