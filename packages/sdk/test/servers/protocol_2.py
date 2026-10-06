"""The real server, answering `initialize` with a protocol version the SDK does not speak."""

import sys

from fastbrowse import serve

serve.PROTOCOL_VERSION = 2

# Started as `<this file> serve --stdio`, the way the SDK starts the binary.
sys.exit(serve.main(sys.argv[2:]))
