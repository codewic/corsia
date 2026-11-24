#!/usr/bin/env python3
"""Round-trip probe for Corsia's remote experts (docs/remote-experts.md).

Measures how long it takes to send a hidden state to another machine and get one back,
the way a remote expert call would: a request of REQ bytes, a reply of the same size,
one at a time, in Corsia's message framing (8-byte header "CORS", version, type, then a
u32 payload length).

    integrations/remote-experts/rtt.py testbox --user USER

It needs only ssh access and python3 on the other machine. It sends itself over ssh and
runs a small echo server there for the length of the run. Two paths are measured:

  ssh   the echo runs on the ssh session's own stdin and stdout (encrypted twice when
        the host is reached over Tailscale; an upper bound)
  tcp   a direct TCP connection to the host's Tailscale address (what remote experts
        would use). The server binds that address only, accepts one connection and
        exits when it closes, or after 60 s with nothing to do. No port stays open.

For each path it reports the round trip for several message sizes, a fitted fixed cost
plus bandwidth (the same model Corsia uses to pick a drive), and the network time of one
generated token of Qwen3-30B-A3B if every layer made one call (48 calls in a row).
"""

import argparse
import json
import os
import shlex
import socket
import statistics
import struct
import subprocess
import sys
import time

MAGIC = b"CORS"
VERSION = 1
T_PING = 0x10
T_PONG = 0x90
HEAD = struct.Struct("<4sHHI")  # magic, version, type, payload length
LAYERS = 48  # Qwen3-30B-A3B


# ----------------------------------------------------------------- framing


def read_exact(read, n):
    buf = bytearray()
    while len(buf) < n:
        chunk = read(n - len(buf))
        if not chunk:
            raise EOFError("connection closed")
        buf += chunk
    return bytes(buf)


def read_msg(read):
    magic, ver, typ, n = HEAD.unpack(read_exact(read, HEAD.size))
    if magic != MAGIC or ver != VERSION:
        raise ValueError("not a Corsia message")
    return typ, read_exact(read, n)


def frame(typ, payload):
    return HEAD.pack(MAGIC, VERSION, typ, len(payload)) + payload


# ----------------------------------------------------------------- server side


def echo(read, write, flush):
    while True:
        try:
            typ, payload = read_msg(read)
        except EOFError:
            return
        if typ != T_PING:
            return
        # The reply is as big as the request: a hidden state in, a hidden state out.
        write(frame(T_PONG, payload))
        flush()


def serve_stdio():
    inp = sys.stdin.buffer.raw if hasattr(sys.stdin.buffer, "raw") else sys.stdin.buffer
    out = sys.stdout.buffer
    out.write(b"READY\n")
    out.flush()
    echo(inp.read, out.write, out.flush)


def serve_tcp(bind, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((bind, port))
    s.listen(1)
    s.settimeout(60)
    sys.stdout.write("READY %d\n" % s.getsockname()[1])
    sys.stdout.flush()
    try:
        c, _ = s.accept()
    except socket.timeout:
        return
    s.close()  # one client only; nothing else can connect from here on
    c.settimeout(60)
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    f = c.makefile("rb", buffering=0)
    try:
        echo(f.read, c.sendall, lambda: None)
    except (socket.timeout, OSError):
        pass
    c.close()


# ----------------------------------------------------------------- client side


def remote_cmd(args, mode):
    script = os.path.abspath(__file__)
    with open(script, "rb") as f:
        body = f.read()
    extra = ["--serve", mode]
    if mode == "tcp":
        extra += ["--bind", args.bind, "--port", str(args.port)]
    # Run the script from stdin on the far side: nothing is installed there.
    loader = "import sys; exec(compile(sys.stdin.buffer.read(%d), 'rtt.py', 'exec'))" % len(body)
    cmd = "python3 -u -c %s %s" % (shlex.quote(loader), " ".join(shlex.quote(x) for x in extra))
    ssh = ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-T", "%s@%s" % (args.user, args.host), cmd]
    p = subprocess.Popen(ssh, stdin=subprocess.PIPE, stdout=subprocess.PIPE, bufsize=0)
    p.stdin.write(body)
    p.stdin.flush()
    return p


def read_line(f):
    line = bytearray()
    while not line.endswith(b"\n"):
        c = f.read(1)
        if not c:
            raise EOFError("the remote side stopped before it was ready")
        line += c
    return line.decode().strip()


def measure(send, recv, size, count, warmup):
    payload = os.urandom(size)
    msg = frame(T_PING, payload)
    times = []
    for i in range(warmup + count):
        t0 = time.perf_counter()
        send(msg)
        typ, back = recv()
        dt = time.perf_counter() - t0
        if typ != T_PONG or len(back) != size:
            raise ValueError("bad reply")
        if i >= warmup:
            times.append(dt * 1e6)
    times.sort()
    return {
        "bytes": size,
        "count": count,
        "min_us": times[0],
        "p50_us": statistics.median(times),
        "p90_us": times[int(0.9 * (len(times) - 1))],
        "p99_us": times[int(0.99 * (len(times) - 1))],
        "max_us": times[-1],
    }


def token(send, recv, size, tokens):
    """LAYERS calls in a row, like one generated token with every layer remote."""
    msg = frame(T_PING, os.urandom(size))
    per = []
    for _ in range(tokens):
        t0 = time.perf_counter()
        for _ in range(LAYERS):
            send(msg)
            recv()
        per.append((time.perf_counter() - t0) * 1e3)
    per.sort()
    return {"tokens": tokens, "layers": LAYERS, "p50_ms": statistics.median(per), "max_ms": per[-1]}


def fit(rows):
    """Least squares: round trip = fixed + 2 x bytes / bandwidth (request and reply)."""
    xs = [2 * r["bytes"] for r in rows]
    ys = [r["p50_us"] for r in rows]
    n = len(xs)
    mx, my = sum(xs) / n, sum(ys) / n
    sxx = sum((x - mx) ** 2 for x in xs)
    slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / sxx if sxx else 0.0
    fixed = my - slope * mx
    mbps = (1.0 / slope) if slope > 0 else float("inf")  # bytes per microsecond = MB/s
    return {"fixed_us": fixed, "mb_per_s": mbps}


def run_path(name, send, recv, args):
    rows = [measure(send, recv, s, args.count, args.warmup) for s in args.sizes]
    tok = token(send, recv, args.size, args.tokens)
    return {"path": name, "sizes": rows, "fit": fit(rows), "token": tok}


def probe_ssh(args):
    p = remote_cmd(args, "stdio")
    try:
        if read_line(p.stdout) != "READY":
            raise RuntimeError("the remote side did not start")
        w, r = p.stdin, p.stdout

        def send(m):
            w.write(m)
            w.flush()

        return run_path("ssh", send, lambda: read_msg(r.read), args)
    finally:
        p.stdin.close()
        p.wait(timeout=30)


def probe_tcp(args):
    p = remote_cmd(args, "tcp")
    try:
        line = read_line(p.stdout)
        if not line.startswith("READY"):
            raise RuntimeError("the remote side did not start")
        port = int(line.split()[1])
        c = socket.create_connection((args.bind, port), timeout=30)
        c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        f = c.makefile("rb", buffering=0)
        try:
            return run_path("tcp", c.sendall, lambda: read_msg(f.read), args)
        finally:
            f.close()  # the socket stays open while its file object does
            c.close()
    finally:
        p.stdin.close()
        p.wait(timeout=30)


def tailscale_ip(host):
    try:
        out = subprocess.run(["tailscale", "ip", "-4", host], capture_output=True, text=True, timeout=10)
        ip = out.stdout.strip().splitlines()
        return ip[0] if out.returncode == 0 and ip else None
    except (OSError, subprocess.TimeoutExpired):
        return None


def report(res):
    print("\n%s" % res["path"])
    print("  %8s  %9s  %9s  %9s  %9s" % ("bytes", "min", "median", "p90", "p99"))
    for r in res["sizes"]:
        print("  %8d  %7.2fms  %7.2fms  %7.2fms  %7.2fms" % (
            r["bytes"], r["min_us"] / 1e3, r["p50_us"] / 1e3, r["p90_us"] / 1e3, r["p99_us"] / 1e3))
    f = res["fit"]
    bw = "fast enough not to measure" if f["mb_per_s"] == float("inf") else "%.1f MB/s" % f["mb_per_s"]
    print("  fixed cost %.2f ms per round trip, %s each way" % (f["fixed_us"] / 1e3, bw))
    t = res["token"]
    print("  one token, %d calls in a row: %.0f ms median, %.0f ms worst of %d" % (
        t["layers"], t["p50_ms"], t["max_ms"], t["tokens"]))


def main():
    ap = argparse.ArgumentParser(description="Round trip of hidden-state sized messages to another machine")
    ap.add_argument("host", nargs="?")
    ap.add_argument("--user", default=os.environ.get("USER", ""))
    ap.add_argument("--size", type=int, default=8192, help="hidden state bytes (Qwen3-30B-A3B: 2048 floats)")
    ap.add_argument("--sizes", type=lambda s: [int(x) for x in s.split(",")], default=[64, 8192, 65536])
    ap.add_argument("--count", type=int, default=200)
    ap.add_argument("--warmup", type=int, default=20)
    ap.add_argument("--tokens", type=int, default=10)
    ap.add_argument("--path", choices=["both", "ssh", "tcp"], default="both")
    ap.add_argument("--json", help="also write the results here")
    ap.add_argument("--serve", choices=["stdio", "tcp"], help=argparse.SUPPRESS)
    ap.add_argument("--bind", help=argparse.SUPPRESS)
    ap.add_argument("--port", type=int, default=0, help=argparse.SUPPRESS)
    args = ap.parse_args()

    if args.serve == "stdio":
        return serve_stdio()
    if args.serve == "tcp":
        return serve_tcp(args.bind, args.port)
    if not args.host:
        ap.error("name the host to measure")
    if args.size not in args.sizes:
        args.sizes = sorted(set(args.sizes + [args.size]))

    results = []
    if args.path in ("both", "ssh"):
        results.append(probe_ssh(args))
        report(results[-1])
    if args.path in ("both", "tcp"):
        args.bind = tailscale_ip(args.host)
        if args.bind is None or not args.bind.startswith("100."):
            print("\ntcp: skipped, %s has no Tailscale address (the server binds only that)" % args.host)
        else:
            results.append(probe_tcp(args))
            report(results[-1])
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"host": args.host, "when": time.strftime("%Y-%m-%dT%H:%M:%S"), "results": results}, f, indent=2)


if __name__ == "__main__":
    sys.exit(main())
