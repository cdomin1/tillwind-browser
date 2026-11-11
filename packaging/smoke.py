"""Launch the packaged browser and require JavaScript to call the test server."""

import http.server
import os
import subprocess
import sys
import threading


class TestPage(http.server.BaseHTTPRequestHandler):
    javascript_ran = threading.Event()

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        if self.path == "/javascript-ran":
            self.javascript_ran.set()
        elif self.path == "/":
            self.wfile.write(b"<!doctype html><script>fetch('/javascript-ran')</script>")


with http.server.ThreadingHTTPServer(("127.0.0.1", 0), TestPage) as server:
    threading.Thread(target=server.serve_forever, daemon=True).start()
    env = dict(os.environ, GDK_BACKEND="x11", GTK_A11Y="none")
    with subprocess.Popen(sys.argv[1:], env=env) as browser:
        try:
            window = subprocess.check_output(
                ["timeout", "30", "xdotool", "search", "--sync", "--onlyvisible", "--name", "^Tillwind$"],
                text=True,
            ).splitlines()[0]
            subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
            subprocess.run(["xdotool", "key", "--clearmodifiers", "ctrl+l"], check=True)
            url = f"http://127.0.0.1:{server.server_port}/"
            subprocess.run(["xdotool", "type", "--clearmodifiers", url], check=True)
            subprocess.run(["xdotool", "key", "Return"], check=True)
            if not TestPage.javascript_ran.wait(30):
                raise SystemExit("The packaged browser did not execute the test page's JavaScript")
        finally:
            browser.terminate()
            try:
                browser.wait(timeout=5)
            except subprocess.TimeoutExpired:
                browser.kill()
