"""Where the package's data files are: the page scripts and the autoconsent bundle."""

import sys
from pathlib import Path


def data_file(*parts: str) -> Path:
    """The file at `parts` below the package, as in `data_file("browser", "snapshot.js")`.

    A PyInstaller build keeps data files under its bundle directory, at the path they have in the package.
    """
    bundle = getattr(sys, "_MEIPASS", None) if getattr(sys, "frozen", False) else None
    package = Path(bundle) / "fastbrowse" if bundle else Path(__file__).parent
    return package.joinpath(*parts)
