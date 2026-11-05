#!/usr/bin/env python3
"""Native-messaging host for the #793 spike: bridges Chrome's stdio framing to a Unix socket that runs dial."""

import json
import os
import socket
import struct
import sys
import threading

SOCK = os.environ.get("SPIKE_SOCK") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "host.sock")
LOG = open(SOCK + ".log", "a", buffering=1)  # noqa: SIM115 - the host logs for its whole life
clients = []
lock = threading.Lock()


def to_chrome(msg):
    data = json.dumps(msg).encode()
    with lock:
        sys.stdout.buffer.write(struct.pack("<I", len(data)) + data)
        sys.stdout.buffer.flush()


def from_chrome():
    while True:
        head = sys.stdin.buffer.read(4)
        if len(head) < 4:
            LOG.write("chrome closed the port\n")
            os._exit(0)
        msg = json.loads(sys.stdin.buffer.read(struct.unpack("<I", head)[0]))
        line = (json.dumps(msg) + "\n").encode()
        for c in list(clients):
            try:
                c.sendall(line)
            except OSError:
                clients.remove(c)


def serve_client(conn):
    clients.append(conn)
    for line in conn.makefile("rb"):
        to_chrome(json.loads(line))


LOG.write(f"host started argv={sys.argv[1:]} sock={SOCK}\n")
if os.path.exists(SOCK):
    os.unlink(SOCK)
srv = socket.socket(socket.AF_UNIX)
srv.bind(SOCK)
os.chmod(SOCK, 0o600)
srv.listen()
threading.Thread(target=from_chrome, daemon=True).start()
while True:
    conn, _ = srv.accept()
    threading.Thread(target=serve_client, args=(conn,), daemon=True).start()
