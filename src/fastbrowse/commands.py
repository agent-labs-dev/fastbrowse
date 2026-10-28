"""The `fastbrowse` and `fastbrowse-mcp` commands: the Rust core's CLI, in this process."""

from fastbrowse import native


def fastbrowse() -> None:
    native.command("fastbrowse")


def fastbrowse_mcp() -> None:
    native.command("fastbrowse-mcp")
