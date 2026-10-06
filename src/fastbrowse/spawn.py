"""The environment for a program this one starts: Chrome, ffmpeg, the Bitwarden CLI."""

import os
import sys


def system_environment() -> dict[str, str] | None:
    """What to pass as `env` when starting a program installed on the machine. None means inherit.

    On Linux a PyInstaller build starts with `LD_LIBRARY_PATH` pointing into its own bundle, and keeps the
    value it was given in `LD_LIBRARY_PATH_ORIG`. A child that inherited the bundle's path would load the
    bundle's libssl and libz in place of the system's, which Chrome was not built against. This puts the
    caller's value back. It goes when the binary is no longer frozen by PyInstaller.
    """
    if not getattr(sys, "frozen", False):
        return None
    environment = dict(os.environ)
    given = environment.pop("LD_LIBRARY_PATH_ORIG", None)
    if given is None:
        environment.pop("LD_LIBRARY_PATH", None)
    else:
        environment["LD_LIBRARY_PATH"] = given
    return environment
