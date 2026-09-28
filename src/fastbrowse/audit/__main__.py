"""Entry point for `python -m fastbrowse.audit`."""

import sys

from fastbrowse.audit.runner import main

if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
