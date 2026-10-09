"""Keep one Flux MCP session (with its window) alive and drive it from short-lived commands.

    python daw/bridge/live.py serve [PROJECT] [--port 47811] start flux mcp --gui (optionally opening PROJECT)
    python daw/bridge/live.py call TOOL [JSON | @file.json]  one tool call; prints text, saves images
    python daw/bridge/live.py stop

Lets an agent whose own MCP client isn't connected to Flux work in a session the human is watching.
"""
import base64
import json
import os
import socket
import socketserver
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(os.path.dirname(HERE))
sys.path.insert(0, os.path.join(ROOT, "daw", "tests"))
# run a copy of the exe so `cargo build` can replace target/debug/flux.exe while the session is live (with a copy of
# the plugin host next to it, where Flux looks for it)
LIVE_EXE = os.path.join(ROOT, "daw", "target", "live", "flux.exe")
LIVE_FILES = ("flux.exe", "flux-plugin-host.exe")
os.environ.setdefault("FLUX_EXE", LIVE_EXE)
from mcp_client import Client  # noqa: E402

PORT = 47811
SHOTS = os.path.join(ROOT, "daw", "tests", "out", "live")


def serve(port, project=None):
    import shutil
    os.makedirs(SHOTS, exist_ok=True)
    os.makedirs(os.path.dirname(LIVE_EXE), exist_ok=True)
    for name in LIVE_FILES:
        src = os.path.join(ROOT, "daw", "target", "debug", name)
        if not os.path.isfile(src):
            sys.exit(f"{src} is missing: build it with `cargo build` in daw/")
        for attempt in range(20):  # a session that was just stopped can hold the old copy for a moment
            try:
                shutil.copy2(src, os.path.join(os.path.dirname(LIVE_EXE), name))
                break
            except PermissionError:
                if attempt == 19:
                    raise
                time.sleep(0.5)
    c = Client("--gui", *(["--project", project] if project else []))
    c.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "Claude", "version": "1"}})
    c.notify("notifications/initialized")
    lock = threading.Lock()
    stop = threading.Event()

    class H(socketserver.StreamRequestHandler):
        def handle(self):
            req = json.loads(self.rfile.readline())
            if req.get("tool") == "__stop__":
                self.wfile.write(b'{"ok": true}\n')
                stop.set()
                return
            with lock:
                try:
                    r = c.request("tools/call", {"name": req["tool"], "arguments": req.get("args") or {}})
                except Exception as e:  # server-level error: report, keep serving
                    r = {"isError": True, "content": [{"type": "text", "text": f"bridge: {e}"}]}
            out = {"isError": bool(r.get("isError")), "text": [], "images": []}
            for item in r.get("content", []):
                if item["type"] == "text":
                    out["text"].append(item["text"])
                elif item["type"] == "image":
                    path = os.path.join(SHOTS, f"shot_{int(time.time() * 1000)}.png")
                    with open(path, "wb") as f:
                        f.write(base64.b64decode(item["data"]))
                    out["images"].append(path)
            self.wfile.write((json.dumps(out) + "\n").encode())

    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.ThreadingTCPServer(("127.0.0.1", port), H) as srv:
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        print(f"flux live session on 127.0.0.1:{port}", flush=True)
        while not stop.is_set() and c.p.poll() is None:
            time.sleep(0.2)
        srv.shutdown()
    c.close()


def send(port, req):
    with socket.create_connection(("127.0.0.1", port)) as s:
        s.sendall((json.dumps(req) + "\n").encode())
        f = s.makefile("rb")
        return json.loads(f.readline())


def main():
    a = sys.argv[1:]
    port = PORT
    if "--port" in a:
        i = a.index("--port")
        port = int(a[i + 1])
        del a[i:i + 2]
    if not a or a[0] == "serve":
        serve(port, a[1] if len(a) > 1 else None)
    elif a[0] == "stop":
        print(send(port, {"tool": "__stop__"}))
    elif a[0] == "call":
        args = {}
        if len(a) > 2:
            raw = a[2]
            args = json.load(open(raw[1:], encoding="utf-8")) if raw.startswith("@") else json.loads(raw)
        r = send(port, {"tool": a[1], "args": args})
        for t in r["text"]:
            print(t)
        for p in r["images"]:
            print(f"[image -> {p}]")
        sys.exit(1 if r["isError"] else 0)


if __name__ == "__main__":
    main()
