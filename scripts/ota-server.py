#!/usr/bin/env python3
"""Serve OTA artifacts over plain HTTP, optionally throttled or stalled.

Usage: ota-server.py DIR PORT [BYTES_PER_SECOND] [STALL_AFTER_BYTES]

A throttle of 0 means full speed; STALL_AFTER_BYTES stops sending (connection kept open)
after that many body bytes, which exercises the device's per-read timeout and total
download deadline (sub-project A, expected status `failed{download_timeout}`).
"""
import os
import sys
import time
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer

CHUNK = 1024


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    directory, port = sys.argv[1], int(sys.argv[2])
    rate = int(sys.argv[3]) if len(sys.argv) > 3 else 0
    stall_after = int(sys.argv[4]) if len(sys.argv) > 4 else 0

    class Handler(SimpleHTTPRequestHandler):
        def __init__(self, *args, **kwargs):
            super().__init__(*args, directory=directory, **kwargs)

        def copyfile(self, source, outputfile):
            sent = 0
            while True:
                chunk = source.read(CHUNK)
                if not chunk:
                    break
                if stall_after and sent + len(chunk) > stall_after:
                    chunk = chunk[: max(0, stall_after - sent)]
                    outputfile.write(chunk)
                    outputfile.flush()
                    print(f"[ota-serve] stalling after {stall_after} bytes", file=sys.stderr)
                    while True:
                        time.sleep(3600)
                outputfile.write(chunk)
                sent += len(chunk)
                if rate:
                    time.sleep(len(chunk) / rate)

    server = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    server.daemon_threads = True
    mode = f"{rate} B/s" if rate else "full speed"
    if stall_after:
        mode += f", stall after {stall_after} bytes"
    print(f"[ota-serve] {os.path.abspath(directory)} on port {port} ({mode})", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
