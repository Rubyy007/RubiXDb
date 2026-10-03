"""Phase L/M — API reliability and resource-exhaustion campaign against the real
release binary. Every scenario records server RSS / threads / handles / sockets
before and after, error classes, and latency percentiles, and ends by proving
the server still answers a correct query. Findings are reported as measured.

Usage: python api_chaos.py OUT_JSON
"""
import json
import os
import random
import shutil
import socket
import sys
import tempfile
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))
from lib import Instance, summarize  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(tempfile.gettempdir(), "api_chaos.json")
R = {"scenarios": {}}


def snap(inst):
    m = inst.proc_metrics()
    return {"rss_mb": round(m["rss"] / 1e6, 1), "threads": m["threads"], "handles": m["handles"], "sockets": m["conns"]}


def record(name, inst, before, **extra):
    time.sleep(1.0)
    after = snap(inst)
    ok_query = False
    try:
        ok_query = inst.rows("SELECT COUNT(*) FROM t")[0][0] is not None
    except Exception as e:
        extra["post_error"] = str(e)
    R["scenarios"][name] = {"before": before, "after": after, "server_still_answers": ok_query, **extra}
    print(f"{name:34s} rss {before['rss_mb']}->{after['rss_mb']} MB  threads {before['threads']}->{after['threads']}  handles {before['handles']}->{after['handles']}  sockets {before['sockets']}->{after['sockets']}  alive={ok_query}  {json.dumps(extra)[:160]}", flush=True)


def raw_http(port, method, path, headers, body=b"", timeout=15, key=None):
    s = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    h = {"Host": "127.0.0.1", "Content-Length": str(len(body)), "Connection": "close"}
    if key:
        h["Authorization"] = f"Bearer {key}"
    h.update(headers)
    req = f"{method} {path} HTTP/1.1\r\n" + "".join(f"{k}: {v}\r\n" for k, v in h.items()) + "\r\n"
    s.sendall(req.encode() + body)
    data = b""
    try:
        while True:
            c = s.recv(65536)
            if not c:
                break
            data += c
    except Exception:
        pass
    s.close()
    try:
        return int(data.split(b" ", 2)[1])
    except Exception:
        return 0


def main():
    root = tempfile.mkdtemp(prefix="rbx_chaos_")
    inst = Instance(root, "chaos")
    inst.start()
    inst.sql("CREATE TABLE t (id INTEGER PRIMARY KEY, v BIGINT, s TEXT)")
    inst.sql("CREATE INDEX t_v ON t (v)")
    for s in range(0, 20000, 250):
        inst.sql("INSERT INTO t (id, v, s) VALUES " + ", ".join(f"({i}, {i % 101}, 'row-{i}-" + "p" * 40 + "')" for i in range(s, s + 250)))
    base = snap(inst)
    R["baseline"] = base
    port, key = inst.port, inst.key
    print("baseline", base, flush=True)

    # 1. normal load: 8 workers x 1500 mixed requests
    before = snap(inst)
    lat, errs = [], [0]
    lock = threading.Lock()

    def work(w):
        r = random.Random(w)
        for _ in range(1500):
            t = time.perf_counter()
            try:
                if r.random() < 0.8:
                    inst.sql(f"SELECT * FROM t WHERE id = {r.randrange(20000)}")
                else:
                    inst.sql(f"UPDATE t SET v = {r.randrange(1000)} WHERE id = {r.randrange(20000)}")
            except Exception:
                with lock:
                    errs[0] += 1
            with lock:
                lat.append((time.perf_counter() - t) * 1000)

    t0 = time.perf_counter()
    with ThreadPoolExecutor(8) as ex:
        list(ex.map(work, range(8)))
    wall = time.perf_counter() - t0
    s = summarize(lat)
    record("normal_load_8x1500", inst, before, latency_ms={k: round(v, 2) for k, v in s.items()}, errors=errs[0], throughput=round(len(lat) / wall))

    # 2. malformed JSON / malformed SQL / hostile bodies
    before = snap(inst)
    codes = {}
    rr = random.Random(5)
    for i in range(600):
        kind = i % 6
        if kind == 0:
            body = bytes(rr.randrange(256) for _ in range(rr.randrange(1, 300)))
        elif kind == 1:
            body = b'{"sql": ' + bytes(rr.randrange(32, 127) for _ in range(40))
        elif kind == 2:
            body = json.dumps({"sql": "".join(chr(rr.randrange(32, 0x2FFF)) for _ in range(rr.randrange(1, 200)))}).encode()
        elif kind == 3:
            body = json.dumps({"sql": "SELECT " + "(" * rr.randrange(1, 400)}).encode()
        elif kind == 4:
            body = json.dumps({"sql": "SELECT * FROM t WHERE id = $9", "params": [1]}).encode()
        else:
            body = b'{"sql": null, "params": "x", "session_id": 7}'
        c = raw_http(port, "POST", "/v1/sql", {"Content-Type": "application/json"}, body, key=key)
        codes[c] = codes.get(c, 0) + 1
    record("malformed_json_and_sql_600", inst, before, status_codes=codes, server_5xx=sum(v for k, v in codes.items() if k >= 500))

    # 3. oversized input
    before = snap(inst)
    big_codes = {}
    for size in (1_000_000, 5_000_000, 40_000_000):
        body = b'{"sql": "SELECT 1", "pad": "' + b"a" * size + b'"}'
        big_codes[size] = raw_http(port, "POST", "/v1/sql", {"Content-Type": "application/json"}, body, timeout=60, key=key)
    huge_sql = json.dumps({"sql": "SELECT " + ", ".join(["1"] * 200000)}).encode()
    big_codes["200k_select_items"] = raw_http(port, "POST", "/v1/sql", {"Content-Type": "application/json"}, huge_sql, timeout=60, key=key)
    record("oversized_input", inst, before, status_codes={str(k): v for k, v in big_codes.items()})

    # 4. large result set
    before = snap(inst)
    t = time.perf_counter()
    try:
        res = inst.sql("SELECT * FROM t")["result"]
        info = {"rows_returned": res.get("row_count"), "truncated": res.get("truncated"), "seconds": round(time.perf_counter() - t, 2)}
    except Exception as e:
        info = {"error": str(e)[:200]}
    record("large_result_set_20k_rows", inst, before, **info)

    # 5. abrupt disconnects: send a heavy request and close immediately
    before = snap(inst)
    for _ in range(300):
        s = socket.create_connection(("127.0.0.1", port))
        body = json.dumps({"sql": "SELECT a.id, b.id FROM t a JOIN t b ON a.v = b.v LIMIT 100000"}).encode()
        s.sendall((f"POST /v1/sql HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n").encode() + body)
        s.close()
    time.sleep(3)
    st = inst.status()
    record("disconnect_mid_request_300", inst, before, cancellations=st["queries"]["cancellations"], active_http=st["queries"]["active_http_requests"])

    # 6. slow clients (slowloris style): half-sent requests held open
    before = snap(inst)
    socks = []
    for i in range(400):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=5)
            s.sendall(b"POST /v1/sql HTTP/1.1\r\nHost: x\r\nContent-Length: 1000\r\nAuthorization: Bearer " + key.encode() + b"\r\n")
            socks.append(s)
        except Exception:
            break
    mid = snap(inst)
    lat = []
    for _ in range(100):
        t = time.perf_counter()
        inst.sql("SELECT 1")
        lat.append((time.perf_counter() - t) * 1000)
    held = len(socks)
    time.sleep(35)  # does the server reap them?
    still_open = 0
    for s in socks:
        try:
            s.settimeout(0.2)
            if s.recv(1) == b"":
                continue
            still_open += 1
        except socket.timeout:
            still_open += 1
        except Exception:
            pass
    for s in socks:
        try:
            s.close()
        except Exception:
            pass
    record("slow_clients_400_held", inst, before, during=mid, held=held, still_open_after_35s=still_open, latency_during_ms={k: round(v, 2) for k, v in summarize(lat).items()})

    # 7. many concurrent connections / query flood
    before = snap(inst)
    codes = {}
    lat = []
    lock = threading.Lock()

    def flood(w):
        for _ in range(60):
            t = time.perf_counter()
            try:
                inst.sql(f"SELECT COUNT(*) FROM t WHERE v = {w % 101}", timeout=120)
                c = 200
            except Exception as e:
                c = getattr(e, "status", 0)
            with lock:
                codes[c] = codes.get(c, 0) + 1
                lat.append((time.perf_counter() - t) * 1000)

    with ThreadPoolExecutor(200) as ex:
        list(ex.map(flood, range(200)))
    peak = snap(inst)
    record("query_flood_200_clients_x60", inst, before, status_codes=codes, latency_ms={k: round(v, 2) for k, v in summarize(lat).items()}, peak_after_flood=peak)

    # 8. many sessions / transactions (limit per principal expected)
    before = snap(inst)
    sess, codes = [], {}
    for i in range(120):
        try:
            sess.append(inst.sql("BEGIN")["session_id"])
            codes[200] = codes.get(200, 0) + 1
        except Exception as e:
            c = getattr(e, "status", 0)
            codes[c] = codes.get(c, 0) + 1
    mid_sessions = inst.status()["sessions"]["active_transactions"]
    for s in sess:
        try:
            inst.sql("ROLLBACK", session_id=s)
        except Exception:
            pass
    record("many_transactions_120_begin", inst, before, status_codes=codes, active_when_full=mid_sessions, active_after_rollback=inst.status()["sessions"]["active_transactions"])

    # 9. many cancellations: 300 requests aborted after 5 ms
    before = snap(inst)
    for i in range(300):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=2)
            body = json.dumps({"sql": f"SELECT * FROM t WHERE v = {i % 101}"}).encode()
            s.sendall((f"POST /v1/sql HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n").encode() + body)
            time.sleep(0.005)
            s.close()
        except Exception:
            pass
    record("many_cancellations_300", inst, before)

    # 10. repeated identical request
    before = snap(inst)
    lat = []
    for _ in range(5000):
        t = time.perf_counter()
        inst.sql("SELECT id FROM t WHERE id = 7")
        lat.append((time.perf_counter() - t) * 1000)
    record("repeated_request_5000", inst, before, latency_ms={k: round(v, 2) for k, v in summarize(lat).items()})

    R["final"] = snap(inst)
    R["final_status_counters"] = inst.status()["queries"]
    inst.stop()
    with open(OUT, "w", encoding="utf-8") as f:
        json.dump(R, f, indent=1)
    shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
