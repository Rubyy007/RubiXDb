"""Black-box harness for the real `rubixdb` product (release binary): starts an
instance exactly like an operator would (`rubixdb gui --no-browser`), talks to
it only over HTTP, kills it (TerminateProcess) or stops it gracefully (a real
Ctrl+C delivered to its console), and reads process resources from the OS.

Nothing in here links against the database code: expected state is always
computed by the test driver itself.
"""
import ctypes
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request

import psutil

EXE = os.environ.get("RBX_EXE", r"E:\RubiXDb\target\release\rubixdb.exe")
FRONTEND = os.environ.get("RBX_FRONTEND", r"E:\RubiXDb\frontend\dist")
CREATE_NO_WINDOW = 0x08000000


class ApiError(Exception):
    def __init__(self, status, body):
        self.status = status
        self.body = body
        err = (body or {}).get("error", {}) if isinstance(body, dict) else {}
        self.code = err.get("code")
        self.message = err.get("message")
        super().__init__(f"HTTP {status} {self.code}: {self.message}")


def _send_ctrl_c(pid):
    """Deliver CTRL_C_EVENT to the console of `pid` from a short-lived helper
    process (so this process is never attached to, or signalled by, it)."""
    helper = (
        "import ctypes,sys,time;k=ctypes.windll.kernel32;pid=int(sys.argv[1]);"
        "k.FreeConsole();ok=k.AttachConsole(pid);"
        "k.SetConsoleCtrlHandler(None,True);"
        "k.GenerateConsoleCtrlEvent(0,0) if ok else None;time.sleep(1.0);k.FreeConsole()"
    )
    subprocess.run([sys.executable, "-c", helper, str(pid)], timeout=30,
                   creationflags=CREATE_NO_WINDOW)


class Instance:
    def __init__(self, root, name="default", exe=EXE, env=None, args=None):
        self.root = root
        self.name = name
        self.exe = exe
        self.extra_env = env or {}
        self.args = args or []
        self.proc = None
        self.port = None
        self.key = None
        self.log = None

    # ---- paths ----
    @property
    def dir(self):
        return os.path.join(self.root, self.name)

    @property
    def data_dir(self):
        return os.path.join(self.dir, "data")

    @property
    def backups_dir(self):
        return os.path.join(self.dir, "backups")

    def env(self):
        e = dict(os.environ)
        e["RUBIXDB_INSTANCES_ROOT"] = self.root
        e["RUBIXDB_FRONTEND_DIST"] = FRONTEND
        e.update(self.extra_env)
        return e

    # ---- lifecycle ----
    def start(self, timeout=180.0):
        """Starts the instance; returns seconds from spawn to a successful
        authenticated first query (the RTO's 'time to first successful query')."""
        os.makedirs(self.root, exist_ok=True)
        self.log = open(os.path.join(self.root, f"{self.name}.log"), "ab")
        t0 = time.perf_counter()
        self.proc = subprocess.Popen(
            [self.exe, "gui", "--no-browser", "--instance", self.name] + self.args,
            env=self.env(), stdout=self.log, stderr=self.log, stdin=subprocess.DEVNULL,
            creationflags=CREATE_NO_WINDOW)
        deadline = time.time() + timeout
        while time.time() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"instance exited during startup with code {self.proc.returncode}")
            try:
                self._load_identity()
                st, _ = self._raw("GET", "/healthz", None, timeout=2)
                if st == 200:
                    # first authenticated query = engine fully open and serving
                    self.api("GET", "/v1/whoami", timeout=10)
                    return time.perf_counter() - t0
            except Exception:
                pass
            time.sleep(0.05)
        raise RuntimeError("instance did not become ready")

    def _load_identity(self):
        with open(os.path.join(self.dir, "instance.json"), encoding="utf-8") as f:
            self.port = json.load(f)["api_port"]
        with open(os.path.join(self.dir, "credentials.json"), encoding="utf-8") as f:
            self.key = json.load(f)["admin_key"]

    def kill(self):
        """TerminateProcess: no cooperation from the process at all."""
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=60)
        self._close_log()

    def stop(self, timeout=90.0):
        """Graceful stop through the supported admin operation
        (POST /v1/admin/shutdown with the exact instance name). Returns the exit code."""
        if self.proc is None or self.proc.poll() is not None:
            return self.proc.returncode if self.proc else None
        self.api("POST", "/v1/admin/shutdown", {"confirm": self.name}, timeout=30)
        try:
            self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=60)
            self._close_log()
            raise RuntimeError("graceful stop timed out; killed")
        self._close_log()
        return self.proc.returncode

    def _close_log(self):
        if self.log:
            try:
                self.log.close()
            except Exception:
                pass
            self.log = None

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    # ---- HTTP ----
    def _conn(self, timeout):
        import http.client
        import socket
        import threading
        tl = self.__dict__.setdefault("_tl", threading.local())
        c = getattr(tl, "conn", None)
        if c is None or getattr(tl, "port", None) != self.port:
            c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
            c.connect()
            c.sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            tl.conn, tl.port = c, self.port
        c.sock.settimeout(timeout)
        return c

    def _raw(self, method, path, body, timeout=60, key=True):
        """Keep-alive HTTP (one persistent connection per thread, TCP_NODELAY,
        headers+body in one send) — what a real client (browser, CLI pool) does."""
        data = None
        headers = {}
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        if key and self.key:
            headers["Authorization"] = f"Bearer {self.key}"
        for attempt in (0, 1):
            c = self._conn(timeout)
            try:
                c.request(method, path, body=data, headers=headers)
                r = c.getresponse()
                raw = r.read()
                if r.getheader("Connection", "").lower() == "close":
                    c.close()
                    self._tl.conn = None
                try:
                    return r.status, (json.loads(raw) if raw else None)
                except ValueError:
                    return r.status, {"raw": raw.decode("utf-8", "replace")}
            except (ConnectionError, OSError, __import__("http.client").client.HTTPException):
                try:
                    c.close()
                except Exception:
                    pass
                self._tl.conn = None
                if attempt == 1:
                    raise

    def api(self, method, path, body=None, timeout=600):
        st, js = self._raw(method, path, body, timeout=timeout)
        if st >= 400:
            raise ApiError(st, js)
        return js

    def sql(self, stmt, session_id=None, timeout=600):
        body = {"sql": stmt}
        if session_id:
            body["session_id"] = session_id
        return self.api("POST", "/v1/sql", body, timeout=timeout)

    def rows(self, stmt):
        r = self.sql(stmt)["result"]
        return [[c.get("value") if isinstance(c, dict) else c for c in row] for row in r["rows"]]

    def status(self):
        return self.api("GET", "/v1/admin/status")

    # ---- OS-level process metrics ----
    def proc_metrics(self):
        p = psutil.Process(self.proc.pid)
        with p.oneshot():
            mi = p.memory_info()
            return {
                "rss": mi.rss, "threads": p.num_threads(),
                "handles": p.num_handles() if hasattr(p, "num_handles") else 0,
                "cpu": sum(p.cpu_times()[:2]),
                "conns": len([c for c in p.net_connections(kind="inet")]),
            }


def dir_size(path):
    total = 0
    for base, _, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(base, f))
            except OSError:
                pass
    return total


def rm_tree(path):
    shutil.rmtree(path, ignore_errors=True)


def percentile(sorted_vals, p):
    if not sorted_vals:
        return 0.0
    i = min(len(sorted_vals) - 1, int(len(sorted_vals) * p))
    return sorted_vals[i]


def summarize(lat_ms):
    s = sorted(lat_ms)
    return {"n": len(s), "p50": percentile(s, .5), "p95": percentile(s, .95),
            "p99": percentile(s, .99), "max": s[-1] if s else 0.0,
            "mean": (sum(s) / len(s)) if s else 0.0}


def slope(xs, ys):
    """Least-squares slope of ys over xs (units of y per unit of x)."""
    n = len(xs)
    if n < 2:
        return 0.0
    mx, my = sum(xs) / n, sum(ys) / n
    den = sum((x - mx) ** 2 for x in xs)
    return (sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den) if den else 0.0
