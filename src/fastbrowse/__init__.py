"""fastbrowse: Jev picks, an LLM reads, code verifies.

`run_task` is the whole public surface for embedding: it assembles a browser, the model clients and
the agent, and returns what the run could prove. Everything under it is importable, and stable only
in the sense that the tests cover it.
"""

from fastbrowse.models import Citation, RunResult
from fastbrowse.run import connect_cdp, resolve_cdp_port, run_task

__all__ = ["Citation", "RunResult", "connect_cdp", "resolve_cdp_port", "run_task"]
