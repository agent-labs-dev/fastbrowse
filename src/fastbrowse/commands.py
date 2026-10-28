"""The `fastbrowse` and `fastbrowse-mcp` commands, on the Rust core (crates/fastbrowse-cli)."""

from fastbrowse import native


def fastbrowse() -> None:
    native.command("fastbrowse")


def fastbrowse_mcp() -> None:
    native.command("fastbrowse-mcp")
