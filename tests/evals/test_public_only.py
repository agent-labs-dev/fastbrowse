"""The evals reach fastbrowse only as an embedder does, so they measure whichever core the package runs."""

import ast
from pathlib import Path

import fastbrowse

ROOT = Path(__file__).parents[2]
SOURCES = sorted([*(ROOT / "src" / "fastbrowse" / "evals").rglob("*.py"), *(ROOT / "tests" / "evals").rglob("*.py")])

PUBLIC_MODULES = {"fastbrowse.models", "fastbrowse.config", "fastbrowse.protocol"}
"""The pydantic models and `Config` a `run_task` call is made of, and the `serve` protocol's messages."""

CORE_LIBRARIES = ("cdp_use", "tests")
"""The Python core's CDP library and the core's own test helpers."""


def _reach_ins(path: Path) -> list[str]:
    found = []
    for node in ast.walk(ast.parse(path.read_text(encoding="utf-8"))):
        if isinstance(node, ast.Import):
            modules = [(alias.name, ()) for alias in node.names]
        elif isinstance(node, ast.ImportFrom) and node.module and not node.level:
            modules = [(node.module, tuple(alias.name for alias in node.names))]
        elif (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute | ast.Name)
            and (node.func.attr if isinstance(node.func, ast.Attribute) else node.func.id)
            in {"import_module", "__import__"}
            and node.args
            and isinstance(node.args[0], ast.Constant)
            and isinstance(node.args[0].value, str)
        ):
            modules = [(node.args[0].value, ())]
        else:
            continue
        for module, names in modules:
            where = f"{path.relative_to(ROOT)}:{node.lineno} {module}"
            if module.split(".")[0] in CORE_LIBRARIES and not module.startswith("tests.evals"):
                found.append(where)
            elif module == "fastbrowse":
                found += [f"{where}.{name}" for name in names if name not in fastbrowse.__all__]
            elif module.startswith("fastbrowse.") and not module.startswith("fastbrowse.evals"):
                if module not in PUBLIC_MODULES:
                    found.append(where)
                else:
                    found += [f"{where}.{name}" for name in names if name.startswith("_")]
    return found


def test_the_evals_import_nothing_of_either_core() -> None:
    assert SOURCES
    assert [reach for path in SOURCES for reach in _reach_ins(path)] == []
