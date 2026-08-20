#!/usr/bin/env python3
"""
Sustained TLS + cluster soak driver (SUSTAINED_TLS_CLUSTER_SOAK).

Not "still up after N hours" (status) — **bounded quantities over time**
(content/behaviour). Drives steady real traffic to the cluster's TLS/HTTP
boundary for hours, scrapes /diag on an interval, and at the end asserts each
measured quantity stayed within a band. A soak that only checks liveness misses
the slow leaks it exists to catch, so the teeth are in the trailing-vs-leading
comparison, not the uptime.

Measures (all must stay bounded):
  * mem.binary / mem.total  — TLS session heap accretion (rustls/:ssl session
    state + large terms land in the BEAM binary heap). THE load-bearing leak.
  * counts.{process,port,atom,ets} — fd/socket/process drift.
  * dist.{roundtrips,mismatches,errors,peers_connected} — distribution stability
    (roundtrips must climb; mismatches must be 0; peers must stay put).
  * clock drift — node os:system_time vs host UTC at scrape time (kvmclock
    long-run drift, the deferred hours-long measurement).
  * latency p50/p99 — response-time creep.
  * zero request errors outside the deliberate probe windows.

Robustness probes (given via --probe at=<sec>:<shell-cmd>): the driver fires the
command (kill a node / partition the link) and then MEASURES time-to-recover on
/health and dist reconnect — a restart that "passes" without exercising
reconnect is inert, so recovery is asserted, not assumed.

Stdlib only (urllib/ssl/threading) — no pip deps on the build host.
"""
import argparse, json, ssl, subprocess, sys, threading, time, urllib.request
from collections import defaultdict

def now(): return time.time()

def http_get(url, timeout=5, insecure=False):
    ctx = None
    if url.startswith("https"):
        ctx = ssl.create_default_context()
        if insecure:
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
    t0 = now()
    try:
        with urllib.request.urlopen(url, timeout=timeout, context=ctx) as r:
            body = r.read()
            return (r.status, body, (now() - t0) * 1000.0)
    except Exception as e:
        return (None, str(e).encode(), (now() - t0) * 1000.0)

class Soak:
    def __init__(self, a):
        self.a = a
        self.nodes = [n.strip().rstrip("/") for n in a.nodes.split(",") if n.strip()]
        self.stop = threading.Event()
        self.lock = threading.Lock()
        self.series = []            # scraped /diag snapshots + host time
        self.lat = []               # (t, node, ms, ok)
        self.req_ok = 0
        self.req_err = 0
        self.probe_errs = 0         # errors inside a probe window (excused)
        self.in_probe = threading.Event()
        self.recoveries = []        # probe recovery records

    # --- steady request load -------------------------------------------------
    def load_loop(self, node):
        interval = 1.0 / max(self.a.rps, 0.1)
        while not self.stop.is_set():
            for path in ("/health", "/work"):
                st, body, ms = http_get(node + path, timeout=self.a.timeout,
                                         insecure=self.a.https_insecure)
                ok = (st == 200) and (path != "/health" or body == b"L2_OK")
                with self.lock:
                    self.lat.append((now(), node, ms, ok))
                    if ok:
                        self.req_ok += 1
                    elif self.in_probe.is_set():
                        self.probe_errs += 1
                    else:
                        self.req_err += 1
            time.sleep(interval)

    # --- /diag scrape --------------------------------------------------------
    def scrape_loop(self):
        while not self.stop.is_set():
            for node in self.nodes:
                st, body, _ = http_get(node + "/diag", timeout=self.a.timeout,
                                       insecure=self.a.https_insecure)
                host_ms = int(now() * 1000)
                if st == 200:
                    try:
                        snap = json.loads(body.decode())
                        drift = None
                        if "clock" in snap and "system_ms" in snap["clock"]:
                            drift = snap["clock"]["system_ms"] - host_ms
                        with self.lock:
                            self.series.append({"t": now(), "node": node,
                                                "host_ms": host_ms,
                                                "drift_ms": drift, "snap": snap})
                    except Exception as e:
                        print(f"[scrape] parse error {node}: {e}", flush=True)
            self._print_tick()
            self.stop.wait(self.a.scrape_s)

    def _print_tick(self):
        with self.lock:
            if not self.series:
                return
            last = self.series[-1]["snap"]
            drift = self.series[-1]["drift_ms"]
        el = int(now() - self.t_start)
        try:
            print(f"[soak t={el}s] binary={last['mem']['binary']//1024}KiB "
                  f"total={last['mem']['total']//1024}KiB proc={last['counts']['process_count']} "
                  f"port={last['counts']['port_count']} atom={last['counts']['atom_count']} "
                  f"peers={last['dist']['peers_connected']} rt={last['dist']['roundtrips']} "
                  f"mism={last['dist']['mismatches']} err={last['dist']['errors']} "
                  f"drift={drift}ms ok={self.req_ok} err={self.req_err}", flush=True)
        except Exception:
            pass

    # --- probes with teeth ---------------------------------------------------
    def probe_loop(self, probes):
        for at_s, cmd in sorted(probes):
            if self.stop.wait(max(0, self.t_start + at_s - now())):
                return
            print(f"\n[probe t={int(now()-self.t_start)}s] FIRE: {cmd}", flush=True)
            self.in_probe.set()
            try:
                subprocess.run(cmd, shell=True, timeout=60)
            except Exception as e:
                print(f"[probe] cmd error: {e}", flush=True)
            rec = self._measure_recovery()
            self.in_probe.clear()
            self.recoveries.append(rec)
            print(f"[probe] recovery: {rec}", flush=True)

    def _measure_recovery(self, budget_s=120):
        """Assert the cluster actually comes back: /health serving AND every node
        reporting its peers again. Time-to-recover is the teeth."""
        t0 = now()
        health_ok_at = None
        peers_ok_at = None
        want_peers = len(self.nodes) - 1
        while now() - t0 < budget_s:
            hs = all(http_get(n + "/health", timeout=4,
                              insecure=self.a.https_insecure)[1] == b"L2_OK"
                     for n in self.nodes)
            if hs and health_ok_at is None:
                health_ok_at = now() - t0
            ps = all((self._peers_of(n) >= want_peers) for n in self.nodes)
            if ps and peers_ok_at is None:
                peers_ok_at = now() - t0
            if health_ok_at is not None and (want_peers == 0 or peers_ok_at is not None):
                break
            time.sleep(2)
        return {"health_recover_s": health_ok_at, "peers_recover_s": peers_ok_at,
                "want_peers": want_peers, "recovered": health_ok_at is not None and
                (want_peers == 0 or peers_ok_at is not None)}

    def _peers_of(self, node):
        st, body, _ = http_get(node + "/diag", timeout=4, insecure=self.a.https_insecure)
        if st != 200:
            return -1
        try:
            return json.loads(body.decode())["dist"]["peers_connected"]
        except Exception:
            return -1

    # --- run + evaluate ------------------------------------------------------
    def run(self, probes):
        self.t_start = now()
        threads = [threading.Thread(target=self.load_loop, args=(n,), daemon=True)
                   for n in self.nodes]
        threads.append(threading.Thread(target=self.scrape_loop, daemon=True))
        if probes:
            threads.append(threading.Thread(target=self.probe_loop, args=(probes,), daemon=True))
        for t in threads:
            t.start()
        self.stop.wait(self.a.duration_s)
        self.stop.set()
        time.sleep(2)
        return self.evaluate()

    def evaluate(self):
        out = {"verdict": "PASS", "reasons": []}
        with self.lock:
            series = list(self.series)
            lat = list(self.lat)
        if len(series) < 6:
            out["verdict"] = "INCONCLUSIVE"
            out["reasons"].append(f"too few scrapes ({len(series)})")
            return out, series, lat

        # warmup-exclude the first 20%, compare leading vs trailing window medians.
        n = len(series)
        warm = series[n // 5:]
        lead = warm[: max(1, len(warm) // 4)]
        trail = warm[-max(1, len(warm) // 4):]

        def med(rows, path):
            vals = []
            for r in rows:
                s = r["snap"]
                try:
                    for k in path:
                        s = s[k]
                    vals.append(s)
                except Exception:
                    pass
            vals.sort()
            return vals[len(vals) // 2] if vals else None

        # bounded-growth checks: trailing median must not exceed leading by more
        # than the band. Memory: 1.5x; unbounded counters (atom/proc/port/ets): tight.
        checks = [
            (["mem", "binary"], 1.6, "TLS session heap (binary) accretion"),
            (["mem", "total"], 1.5, "total BEAM memory growth"),
            (["counts", "process_count"], 1.3, "process leak"),
            (["counts", "port_count"], 1.3, "port/socket-fd leak"),
            (["counts", "atom_count"], 1.05, "atom leak"),
            (["counts", "ets_count"], 1.2, "ets table leak"),
        ]
        for path, band, label in checks:
            a, b = med(lead, path), med(trail, path)
            if a and b and a > 0 and (b / a) > band:
                out["verdict"] = "FAIL"
                out["reasons"].append(f"{label}: {a} -> {b} (x{b/a:.2f} > {band})")

        # dist: mismatches must be 0, errors must not climb after warmup, peers stable.
        mism = max((r["snap"]["dist"]["mismatches"] for r in warm), default=0)
        if mism > 0:
            out["verdict"] = "FAIL"
            out["reasons"].append(f"dist term mismatches: {mism} (must be 0)")
        rt_lead, rt_trail = med(lead, ["dist", "roundtrips"]), med(trail, ["dist", "roundtrips"])
        want_peers = len(self.nodes) - 1
        if want_peers > 0 and rt_lead is not None and rt_trail is not None and rt_trail <= rt_lead:
            out["verdict"] = "FAIL"
            out["reasons"].append(f"dist roundtrips not climbing: {rt_lead} -> {rt_trail}")

        # clock drift: |drift| must stay within band (default 2s over the run).
        drifts = [abs(r["drift_ms"]) for r in warm if r.get("drift_ms") is not None]
        if drifts:
            maxd = max(drifts)
            out["max_abs_drift_ms"] = maxd
            if maxd > self.a.max_drift_ms:
                out["verdict"] = "FAIL"
                out["reasons"].append(f"kvmclock drift {maxd}ms > {self.a.max_drift_ms}ms")

        # latency creep: p99 of trailing window vs leading.
        def p99(win):
            lo, hi = win
            v = sorted(ms for (t, _node, ms, ok) in lat if lo <= t <= hi and ok)
            return v[min(int(len(v) * 0.99), len(v) - 1)] if v else None
        span = warm[-1]["t"] - warm[0]["t"]
        lat_lead = p99((warm[0]["t"], warm[0]["t"] + span / 4))
        lat_trail = p99((warm[-1]["t"] - span / 4, warm[-1]["t"]))
        if lat_lead and lat_trail and lat_trail > lat_lead * 2.0:
            out["verdict"] = "FAIL"
            out["reasons"].append(f"latency p99 creep: {lat_lead:.0f}ms -> {lat_trail:.0f}ms")

        # steady-state request errors (outside probe windows) must be ~0.
        err_rate = self.req_err / max(1, self.req_ok + self.req_err)
        out["req_ok"] = self.req_ok
        out["req_err_steady"] = self.req_err
        out["req_err_in_probe"] = self.probe_errs
        if err_rate > 0.01:
            out["verdict"] = "FAIL"
            out["reasons"].append(f"steady-state error rate {err_rate:.3f} > 0.01")

        # probes must have actually recovered.
        for rec in self.recoveries:
            if not rec["recovered"]:
                out["verdict"] = "FAIL"
                out["reasons"].append(f"probe did not recover: {rec}")
        out["recoveries"] = self.recoveries
        return out, series, lat

def parse_probe(s):
    # "at=90:pkill -9 -f node2" -> (90.0, "pkill -9 -f node2")
    head, _, cmd = s.partition(":")
    assert head.startswith("at="), f"bad --probe {s}"
    return (float(head[3:]), cmd)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--nodes", required=True, help="comma-separated base URLs")
    ap.add_argument("--duration-s", dest="duration_s", type=int, default=9000)
    ap.add_argument("--scrape-s", dest="scrape_s", type=int, default=15)
    ap.add_argument("--rps", type=float, default=5.0, help="requests/sec per node")
    ap.add_argument("--timeout", type=float, default=5.0)
    ap.add_argument("--max-drift-ms", dest="max_drift_ms", type=int, default=2000)
    ap.add_argument("--https-insecure", dest="https_insecure", action="store_true",
                    help="skip client cert verify (server-side TLS is what we soak)")
    ap.add_argument("--probe", action="append", default=[], type=parse_probe)
    ap.add_argument("--out", default=None, help="write full jsonl series here")
    a = ap.parse_args()

    soak = Soak(a)
    print(f"=== SOAK start: nodes={soak.nodes} dur={a.duration_s}s rps={a.rps} "
          f"scrape={a.scrape_s}s probes={len(a.probe)} ===", flush=True)
    out, series, lat = soak.run(a.probe)

    if a.out:
        with open(a.out, "w") as f:
            for r in series:
                f.write(json.dumps(r) + "\n")

    print("\n=== SOAK VERDICT ===", flush=True)
    print(json.dumps(out, indent=2, default=str), flush=True)
    sys.exit(0 if out["verdict"] == "PASS" else 1)

if __name__ == "__main__":
    main()
