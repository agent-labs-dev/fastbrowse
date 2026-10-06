"""The real server, except that it answers `shutdown` and then never exits, and shrugs off a polite signal."""

import asyncio
import signal
import sys

from fastbrowse import serve

answer_until_shutdown = serve.Server.serve


async def linger(self: serve.Server) -> int:
    await answer_until_shutdown(self)
    await asyncio.Event().wait()
    return 0


serve.Server.serve = linger
signal.signal(signal.SIGTERM, signal.SIG_IGN)

# Started as `<this file> serve --stdio`, the way the SDK starts the binary.
sys.exit(serve.main(sys.argv[2:]))
