"""Minimal MCP stdio client for exercising the Flux server.

    python daw/tests/mcp_client.py SCRIPT.json      run a list of [tool, args] calls, print each result
    python daw/tests/mcp_client.py --smoke          initialize, list tools, open + render + analyze makima_boss
"""
import base64
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
EXE = os.environ.get("FLUX_EXE") or os.path.join(ROOT, "daw", "target", "debug", "flux.exe")


class Client:
    def __init__(self, *args):
        self.p = subprocess.Popen([EXE, "mcp", *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=sys.stderr, cwd=ROOT)
        self.next = 1

    def request(self, method, params=None):
        rid = self.next
        self.next += 1
        msg = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params or {}}
        self.p.stdin.write((json.dumps(msg) + "\n").encode())
        self.p.stdin.flush()
        while True:
            line = self.p.stdout.readline()
            if not line:
                raise RuntimeError("server closed stdout")
            try:
                r = json.loads(line)
            except json.JSONDecodeError:
                raise RuntimeError(f"non-JSON on stdout: {line[:200]!r}")
            if r.get("id") == rid:
                if "error" in r:
                    raise RuntimeError(r["error"])
                return r["result"]

    def notify(self, method, params=None):
        self.p.stdin.write((json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}}) + "\n").encode())
        self.p.stdin.flush()

    def init(self):
        r = self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                        "clientInfo": {"name": "smoke-test", "version": "0"}})
        self.notify("notifications/initialized")
        return r

    def call(self, name, args=None, show=True, image_dir=None):
        t = time.time()
        r = self.request("tools/call", {"name": name, "arguments": args or {}})
        dt = time.time() - t
        if show:
            flag = " ERROR" if r.get("isError") else ""
            print(f"--- {name} {json.dumps(args or {})[:160]} ({dt:.2f} s){flag}")
            for c in r["content"]:
                if c["type"] == "text":
                    print(c["text"])
                elif c["type"] == "image":
                    data = base64.b64decode(c["data"])
                    if image_dir:
                        path = os.path.join(image_dir, f"shot_{int(time.time() * 1000)}.png")
                        open(path, "wb").write(data)
                        print(f"[image {len(data)} bytes -> {path}]")
                    else:
                        print(f"[image {len(data)} bytes]")
        return r

    def close(self):
        self.p.stdin.close()
        try:
            self.p.wait(10)
        except subprocess.TimeoutExpired:
            self.p.kill()


def main():
    if len(sys.argv) > 1 and sys.argv[1] != "--smoke":
        script = json.load(open(sys.argv[1]))
        c = Client(*script.get("args", []))
        c.init()
        for name, args in script["calls"]:
            c.call(name, args, image_dir=script.get("image_dir"))
        c.close()
        return
    c = Client()
    info = c.init()
    print("server:", info["serverInfo"], "protocol", info["protocolVersion"])
    tools = c.request("tools/list")["tools"]
    print(len(tools), "tools:", ", ".join(t["name"] for t in tools))
    c.call("open", {"path": "output/makima_boss/makima_boss.json"})
    c.call("get_project")
    c.call("render")
    c.call("analyze")
    c.close()


if __name__ == "__main__":
    main()
