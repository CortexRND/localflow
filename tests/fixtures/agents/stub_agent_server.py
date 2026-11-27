"""Scripted HTTP stub for the devin and http providers (loopback only).

    stub = StubAgentServer()
    stub.script("POST", "/v1/sessions", (200, {"session_id": "d-1", "url": "https://x"}))
    stub.script("GET", "/v1/sessions/d-1", (200, {...}), (200, {...}))  # consumed in order
    ...
    stub.requests  # [{"method", "path", "headers", "json", "body"}]

The last scripted response of a route repeats. Unscripted routes answer 404.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class StubAgentServer:
    def __init__(self):
        self.requests: list[dict] = []
        self._routes: dict[tuple[str, str], list] = {}
        self._lock = threading.Lock()
        stub = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):  # quiet
                pass

            def _handle(self):
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b""
                try:
                    body = json.loads(raw) if raw else None
                except ValueError:
                    body = None
                stub._record(self.command, self.path, dict(self.headers), body, raw)
                status, payload, headers = stub._next(self.command, self.path)
                data = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                for key, value in (headers or {}).items():
                    self.send_header(key, value)
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST = do_PUT = do_PATCH = do_DELETE = _handle

        self._server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    @property
    def url(self) -> str:
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}"

    def script(self, method: str, path: str, *responses) -> None:
        normalized = [r if len(r) == 3 else (*r, None) for r in responses]
        with self._lock:
            self._routes[(method, path)] = list(normalized)

    def _record(self, method, path, headers, body, raw) -> None:
        with self._lock:
            self.requests.append({"method": method, "path": path, "headers": headers,
                                  "json": body, "body": raw})

    def _next(self, method, path):
        with self._lock:
            queue = self._routes.get((method, path))
            if not queue:
                return 404, {"detail": "not scripted"}, None
            return queue.pop(0) if len(queue) > 1 else queue[0]

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
