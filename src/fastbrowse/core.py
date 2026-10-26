"""Which core a call runs on: the Rust core in `fastbrowse._native`, unless `FASTBROWSE_PYTHON_CORE=1`.

The Python core is the fallback for one release (docs/adr/0004) and goes in 0.9.0, the switch with it.
"""

import os


def python_core() -> bool:
    """Whether `FASTBROWSE_PYTHON_CORE=1` keeps this call on the Python core."""
    return os.environ.get("FASTBROWSE_PYTHON_CORE") == "1"
