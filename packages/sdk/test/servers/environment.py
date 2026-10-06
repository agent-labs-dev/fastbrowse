"""The real server, which first writes the environment it was started with to a file the test names."""

import json
import os
import sys
from pathlib import Path

from fastbrowse import serve

Path(os.environ["FASTBROWSE_TEST_ENVIRONMENT_FILE"]).write_text(json.dumps(dict(os.environ)))

# Started as `<this file> serve --stdio`, the way the SDK starts the binary.
sys.exit(serve.main(sys.argv[2:]))
