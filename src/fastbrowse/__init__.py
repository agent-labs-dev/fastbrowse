"""fastbrowse: Jev picks, an LLM reads, code verifies.

`run_task` is the whole public surface for embedding: it assembles a browser, the model clients and
the agent, and returns what the run could prove. `traced` collects the trace of the runs made inside it, as
`--trace` writes it for the command. Everything under them is importable, and stable only in the sense that
the tests cover it.
"""

from fastbrowse.models import Citation, RunResult
from fastbrowse.run import connect_cdp, resolve_cdp_port, run_task
from fastbrowse.telemetry import traced

__all__ = ["Citation", "RunResult", "connect_cdp", "resolve_cdp_port", "run_task", "traced"]
