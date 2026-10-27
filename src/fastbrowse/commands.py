"""The `fastbrowse` and `fastbrowse-mcp` commands: the Rust core's CLI, or Python's with `FASTBROWSE_PYTHON_CORE=1`."""

from fastbrowse import native
from fastbrowse.core import python_core


def fastbrowse() -> None:
    if not python_core():
        native.command("fastbrowse")
    from fastbrowse.cli import main

    main()


def fastbrowse_mcp() -> None:
    if not python_core():
        native.command("fastbrowse-mcp")
    from fastbrowse.mcp_server import main

    main()
