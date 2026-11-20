"""The oracle: the workspace a correct run leaves behind, built without a desktop.

`sales.ods`, `report.odt` and `report.pdf` beside this file are real LibreOffice saves of the correct report, made
by `build_artifacts.py` in the same LibreOffice the desktop box installs. The oracle copies them into `out/`.
"""

import shutil
from pathlib import Path

HERE = Path(__file__).parent


async def solve(step: str, text: str, world: object, workspace: Path) -> str:
    out = workspace / "out"
    out.mkdir(exist_ok=True)
    for name in ("sales.ods", "report.odt", "report.pdf"):
        shutil.copyfile(HERE / name, out / name)
    return "Charted revenue by region in Calc and exported the report to out/report.pdf; the total is 75,025."
