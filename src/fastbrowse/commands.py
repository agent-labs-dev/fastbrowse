"""The `fastbrowse` and `fastbrowse-mcp` commands: the Rust core's CLI with `FASTBROWSE_RUST_CORE=1`, else Python's."""

import os

from fastbrowse import native


def fastbrowse() -> None:
    if os.environ.get("FASTBROWSE_RUST_CORE") == "1":
        native.command("fastbrowse")
    from fastbrowse.cli import main

    main()


def fastbrowse_mcp() -> None:
    if os.environ.get("FASTBROWSE_RUST_CORE") == "1":
        native.command("fastbrowse-mcp")
    from fastbrowse.mcp_server import main

    main()
