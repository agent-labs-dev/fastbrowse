"""The PDF helper process: `fastbrowse._native` serving one request on stdin and stdout, then exiting.

The core starts this with `python -I -S`, on the interpreter's own executable rather than a venv's, so it can kill a
parse that runs too long. Nothing here needs `site` or the venv's packages: the extension is loaded from this file's
directory by path, which also keeps the package's own imports, and anything they print, out of a process started for
every PDF a run reads.
"""

import sys
from importlib.machinery import EXTENSION_SUFFIXES, ExtensionFileLoader
from importlib.util import module_from_spec, spec_from_loader
from pathlib import Path


def main() -> int:
    here = Path(__file__).parent
    for suffix in EXTENSION_SUFFIXES:
        path = here / f"_native{suffix}"
        if path.is_file():
            loader = ExtensionFileLoader("fastbrowse._native", str(path))
            spec = spec_from_loader("fastbrowse._native", loader)
            if spec is None:
                break
            native = module_from_spec(spec)
            loader.exec_module(native)
            return native.cli_main("fastbrowse", sys.argv[1:], None, None)
    print(f"fastbrowse: no fastbrowse._native beside {here}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
