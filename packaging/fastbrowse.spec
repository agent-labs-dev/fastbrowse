# The one build definition for the native executable, on every target:
#
#     uv sync --locked --no-dev --group freeze
#     uv run --no-sync pyinstaller packaging/fastbrowse.spec --noconfirm
#
# It writes dist/fastbrowse/, holding `fastbrowse` (`fastbrowse.exe` on Windows) and `_internal/` beside it.
# PyInstaller cannot build for another platform, so each target is built on a machine of its own kind.
#
# A directory and not one file: a one-file build unpacks itself to a temporary directory on every launch, and
# `serve` is started once per SDK client.

from PyInstaller.utils.hooks import collect_data_files, copy_metadata

# The MCP server and the browser-use eval arm need extras the executable does not carry. Excluding them here
# keeps them out even when the build environment happens to have the extras installed.
LEFT_OUT = ("mcp", "browser_use_sdk", "fastbrowse.mcp_server")

datas = [
    # The page scripts and the autoconsent bundle, which `fastbrowse.datafiles` reads from the bundle at the
    # path they have in the package.
    *collect_data_files("fastbrowse", includes=["browser/**"]),
    # `--version` and the `initialize` reply read the version from the distribution's metadata.
    *copy_metadata("fastbrowse"),
]

a = Analysis(
    ["entry.py"],
    datas=datas,
    # `serve --run-task` names this module at run time, so no import statement leads the analysis to it. It is
    # what the smoke test in `scripts/smoke_binary.py` drives the executable with.
    hiddenimports=["fastbrowse.scripted"],
    excludes=list(LEFT_OUT),
)


def _left_out(name):
    return any(name == module or name.startswith(module + ".") for module in LEFT_OUT)


carried = sorted(entry[0] for entry in (*a.pure, *a.binaries, *a.datas) if _left_out(entry[0].replace("/", ".")))
if carried:
    raise SystemExit(f"the build carries what it was meant to leave out: {carried}")

pyz = PYZ(a.pure)
exe = EXE(pyz, a.scripts, exclude_binaries=True, name="fastbrowse", console=True)
COLLECT(exe, a.binaries, a.datas, name="fastbrowse")
