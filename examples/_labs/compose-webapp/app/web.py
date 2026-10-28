"""HTTP front of the sample: /healthz, and / reporting what it was given."""
import json
import os
import socket
from http.server import BaseHTTPRequestHandler, HTTPServer


def reachable(host, port):
    try:
        with socket.create_connection((host, port), timeout=2):
            return True
    except OSError:
        return False


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/healthz":
            body = b"ok\n"
        else:
            body = json.dumps(
                {
                    "app_env": os.environ.get("APP_ENV"),
                    "db": reachable(os.environ["DATABASE_HOST"], 5432),
                    "redis": reachable(os.environ["REDIS_HOST"], 6379),
                }
            ).encode() + b"\n"
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


HTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
