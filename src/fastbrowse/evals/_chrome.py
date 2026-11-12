"""Keep a harness-owned Chrome tied to its owner's open stdin pipe."""

import shutil
import subprocess
import sys
import threading


def run() -> int:
    process = subprocess.Popen(sys.argv[2:], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)

    def stop_on_eof() -> None:
        # A native abort bypasses the owner's finally block, but still closes its pipe.
        sys.stdin.buffer.read()
        try:
            process.terminate()
        except ProcessLookupError:
            return
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

    threading.Thread(target=stop_on_eof, daemon=True).start()
    return process.wait()


if __name__ == "__main__":
    try:
        sys.exit(run())
    finally:
        # An abort also bypasses TemporaryDirectory cleanup in the owner.
        shutil.rmtree(sys.argv[1], ignore_errors=True)
