"""fastbrowse: Jev picks, an LLM reads, code verifies.

`run_task` is the whole public surface for embedding: it assembles a browser, the model clients and
the agent, and returns what the run could prove. `traced` collects the trace of the runs made inside it, as
`--trace` writes it for the command. `open_cloud_browser` and `stop_cloud_browser` keep a Browser Use Cloud
browser across runs. Everything under them is importable, and stable only in the sense that the tests cover it.
"""

from fastbrowse.models import Citation, CloudBrowser, CloudBrowserError, CloudBrowserUnavailable, RunResult
from fastbrowse.run import connect_cdp, open_cloud_browser, run_task, stop_cloud_browser
from fastbrowse.telemetry import traced

__all__ = [
    "Citation",
    "CloudBrowser",
    "CloudBrowserError",
    "CloudBrowserUnavailable",
    "RunResult",
    "connect_cdp",
    "open_cloud_browser",
    "run_task",
    "stop_cloud_browser",
    "traced",
]
