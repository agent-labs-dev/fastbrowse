"""The suites test the Python core, which runs only with FASTBROWSE_PYTHON_CORE=1 now that Rust is the default.

Set before any test imports the package: `fastbrowse.origins` and `fastbrowse.citations` pick their core at import.
The Rust core's own run of the suites that cover it sets FASTBROWSE_PYTHON_CORE=0 (`just rust`, the `rust` CI job).
"""

import os

os.environ.setdefault("FASTBROWSE_PYTHON_CORE", "1")
