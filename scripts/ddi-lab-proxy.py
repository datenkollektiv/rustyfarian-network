#!/usr/bin/env python3
"""Lab reverse proxy between a DDI device and the lab DDI server (plain HTTP, lab only).

Usage: ddi-lab-proxy.py PORT UPSTREAM_ORIGIN [BYTES_PER_SECOND]

The device's DDI_BASE_URL names this proxy (http://<this host>:PORT/DEFAULT), and every request
is forwarded to UPSTREAM_ORIGIN (http://host:port) with the client's Host header kept, so a server
that builds its links from the request host (hawkBit) answers with links back to the proxy; in a
JSON body the upstream origin text is rewritten to the proxy's, for a server that builds links from
a configured base. The device's origin check then passes, because every link names its base.

Artifact bodies (a path with /artifacts/) are sent at BYTES_PER_SECOND (default 20480, 0 = full
speed), so the bench's cancel-mid-download and reboot-mid-download rows have a window to act in.
An artifact whose file name ends in -tampered.bin has one byte flipped at TAMPER_OFFSET: the image
header stays intact and the size is unchanged, so the device reports 1002 checksum_mismatch after the
download (`just ddi-upload <example> <version> --tampered` uploads such a copy; the server itself
verifies every upload, so a wrong digest cannot be declared through its API).

Only DDI paths (/<tenant>/controller/v1/...) without a `.` or `..` segment (percent-encoded ones included) are
forwarded, so a path cannot climb out of the DDI tree on an upstream that normalises it; everything else, the Management API and
a UI included, is answered 403, so the port the router opens to the IoT subnet exposes nothing
beyond what the device needs. The one exception is GET /ddi-lab-proxy/health, answered 200 with the
fixed body "ddi-lab-proxy": the bench asks it before it shares a listener, so another HTTP server on
the port is never taken for this proxy. The Authorization header passes through untouched and is never
logged; log lines carry only the method, the path, the status and the byte count. Runs with
python3 -u, so each line is flushed.
"""
import http.client
import io
import re
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit

CHUNK = 4096
TAMPER_OFFSET = 65536
TAMPER_SUFFIX = "-tampered.bin"
HEALTH_PATH = "/ddi-lab-proxy/health"
HEALTH_BODY = b"ddi-lab-proxy\n"
DDI_PATH = re.compile(r"^/[^/]+/controller/v1/")
HOP_BY_HOP = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
}


def error_detail(e):
    """A log-safe description of `e`: its class and, for an OSError, the system message, never str(e), which may
    quote the request target and its query (http.client.InvalidURL does)."""
    if isinstance(e, OSError) and e.strerror:
        return f"{type(e).__name__}: {e.strerror}"
    return type(e).__name__


def has_dot_segment(path):
    """Whether the path part of `path` has a `.` or `..` segment, percent-encoded or not."""
    segments = unquote(path.split("?", 1)[0]).split("/")
    return any(segment in (".", "..") for segment in segments)


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    port = int(sys.argv[1])
    upstream = sys.argv[2].rstrip("/")
    rate = int(sys.argv[3]) if len(sys.argv) > 3 else 20480
    parts = urlsplit(upstream)
    if parts.scheme != "http" or not parts.hostname:
        sys.exit(f"UPSTREAM_ORIGIN must be http://host[:port], got {upstream!r}")
    up_host, up_port = parts.hostname, parts.port or 80
    upstream_bytes = upstream.encode()

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *args):
            print(f"[ddi-proxy] {fmt % args}", file=sys.stderr)

        def log_request(self, code="-", size="-"):
            pass

        def log_error(self, fmt, *args):
            # The built-in messages can quote the raw request line, query included; log the status code only.
            self.log_message("answered %s", args[0] if args else "an error")

        def forward(self):
            # Logged without the query or fragment: either may carry a secret.
            log_path = self.path.split("?", 1)[0].split("#", 1)[0]
            if self.command == "GET" and self.path == HEALTH_PATH:
                self.send_response(200)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(HEALTH_BODY)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(HEALTH_BODY)
                return
            if not DDI_PATH.match(self.path) or has_dot_segment(self.path):
                self.log_message("%s %s -> 403 not a DDI path", self.command, log_path)
                self.send_error(403, "only DDI paths are proxied")
                return
            length = int(self.headers.get("Content-Length") or 0)
            body = self.rfile.read(length) if length else None
            headers = {}
            for key, value in self.headers.items():
                low = key.lower()
                if low in HOP_BY_HOP or low == "accept-encoding":
                    continue
                headers[key] = value
            # ESP-IDF can omit a non-default listener port from Host, but hawkBit builds DDI links from it.
            # Restore the public listener port so those links stay on the device's configured origin.
            host = self.headers.get("Host", f"127.0.0.1:{port}")
            if urlsplit("//" + host).port is None:
                host = f"{host}:{port}"
            headers["Host"] = host
            headers["Connection"] = "close"
            conn = http.client.HTTPConnection(up_host, up_port, timeout=60)
            try:
                try:
                    conn.request(self.command, self.path, body=body, headers=headers)
                    resp = conn.getresponse()
                except (OSError, http.client.HTTPException) as e:
                    self.log_message("%s %s -> upstream error: %s", self.command, log_path, error_detail(e))
                    self.send_error(502, "upstream unreachable")
                    return
                self.relay(resp, log_path, host)
            finally:
                conn.close()

        def relay(self, resp, log_path, host):
            status, reason = resp.status, resp.reason
            resp_headers = resp.getheaders()
            content_type = ""
            upstream_length = None
            for key, value in resp_headers:
                low = key.lower()
                if low == "content-type":
                    content_type = value
                elif low == "content-length":
                    upstream_length = value
            artifact = "/artifacts/" in log_path
            notes = []
            source = resp
            length = upstream_length
            if self.command != "HEAD" and "json" in content_type:
                # A DDI document is small (the device caps it at 8 KiB): it is read whole so the upstream origin in
                # its links can be rewritten.
                try:
                    data = resp.read()
                except (OSError, http.client.HTTPException) as e:
                    self.log_message("%s %s -> upstream error: %s", self.command, log_path, error_detail(e))
                    self.send_error(502, "upstream unreachable")
                    return
                if upstream_bytes in data:
                    public = ("http://" + host).encode()
                    data = data.replace(upstream_bytes, public)
                    notes.append("links rewritten")
                source = io.BytesIO(data)
                length = str(len(data))

            # Any other body, an artifact above all, is streamed as it arrives: the device gets the headers at
            # once instead of after the whole image, and the proxy never holds an image in memory. A body the
            # upstream sent without a length is passed on chunked rather than read whole to count it.
            chunked = self.command != "HEAD" and length is None
            self.send_response(status, reason)
            for key, value in resp_headers:
                low = key.lower()
                if low in HOP_BY_HOP or low == "content-length":
                    continue
                self.send_header(key, value)
            if chunked:
                self.send_header("Transfer-Encoding", "chunked")
            else:
                # A HEAD response has no body, so its length is the upstream's.
                self.send_header("Content-Length", length or "0")
            self.send_header("Connection", "close")
            self.end_headers()
            if self.command == "HEAD":
                return

            tamper = artifact and status == 200 and log_path.endswith(TAMPER_SUFFIX)
            throttle = artifact and status == 200 and rate > 0
            if throttle:
                notes.append(f"throttled to {rate} B/s")
                self.log_message("%s %s -> %s, sending %s bytes", self.command, log_path, status, length or "unknown")
            sent = 0
            started = time.monotonic()
            try:
                while True:
                    chunk = source.read(CHUNK)
                    if not chunk:
                        break
                    if tamper and sent <= TAMPER_OFFSET < sent + len(chunk):
                        i = TAMPER_OFFSET - sent
                        chunk = chunk[:i] + bytes([chunk[i] ^ 0xFF]) + chunk[i + 1 :]
                        notes.append("tampered")
                    if chunked:
                        self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                    else:
                        self.wfile.write(chunk)
                    self.wfile.flush()
                    sent += len(chunk)
                    if throttle:
                        pause = started + sent / rate - time.monotonic()
                        if pause > 0:
                            time.sleep(pause)
                if chunked:
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
            except (OSError, http.client.HTTPException) as e:
                self.log_message(
                    "%s %s -> transfer stopped after %d bytes: %s", self.command, log_path, sent, error_detail(e)
                )
                return
            note = "".join(" " + n for n in notes)
            self.log_message("%s %s -> %s %d bytes%s", self.command, log_path, status, sent, note)

        do_GET = forward
        do_HEAD = forward
        do_POST = forward
        do_PUT = forward
        do_DELETE = forward

    class Server(ThreadingHTTPServer):
        allow_reuse_address = True
        daemon_threads = True

    with Server(("0.0.0.0", port), Handler) as server:
        print(
            f"[ddi-proxy] listening on 0.0.0.0:{port}, forwarding to {upstream}, artifacts at "
            f"{rate or 'full speed'}{' B/s' if rate else ''}, {TAMPER_SUFFIX} corrupted at byte {TAMPER_OFFSET}",
            file=sys.stderr,
        )
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
