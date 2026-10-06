"""The real server, which first writes one known line to stderr."""

import sys

from fastbrowse import serve

sys.stderr.write("a line the server wrote to stderr\n")
sys.stderr.flush()

# Started as `<this file> serve --stdio`, the way the SDK starts the binary.
sys.exit(serve.main(sys.argv[2:]))
