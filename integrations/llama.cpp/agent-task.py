#!/usr/bin/env python3
"""A real coding task for a local model, through the same OpenAI-style endpoint an editor
assistant (Continue, Cline) uses: a small project with a bug and failing tests, three tools
(read a file, write a file, run the tests), and a loop until the tests pass or the turns run
out. Prints what the model did, whether the tests pass, the turns and the time.

Usage:  agent-task.py [--url http://127.0.0.1:7720/v1] [--turns 12]
Needs only Python 3. The project is written to a fresh temporary folder each run.
"""
import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.request

PROJECT = {
    "intervals.py": '''def merge_intervals(intervals):
    """Merge overlapping [start, end] intervals; touching ones ([1, 2] and [2, 3]) merge too.
    Returns a new list sorted by start."""
    result = []
    for start, end in intervals:
        if result and start < result[-1][1]:
            result[-1][1] = max(result[-1][1], end)
        else:
            result.append([start, end])
    return result
''',
    "test_intervals.py": '''from intervals import merge_intervals

def test_overlap():
    assert merge_intervals([[1, 3], [2, 6], [8, 10]]) == [[1, 6], [8, 10]]

def test_touching():
    assert merge_intervals([[1, 2], [2, 3]]) == [[1, 3]]

def test_unsorted():
    assert merge_intervals([[8, 10], [1, 3], [2, 6]]) == [[1, 6], [8, 10]]

def test_inputs_untouched():
    data = [[1, 3], [2, 4]]
    merge_intervals(data)
    assert data == [[1, 3], [2, 4]]

if __name__ == "__main__":
    import sys
    failed = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            try:
                fn()
                print("PASS", name)
            except AssertionError:
                failed += 1
                print("FAIL", name)
    sys.exit(1 if failed else 0)
''',
}

TOOLS = [
    {"type": "function", "function": {"name": "read_file", "description": "Read a file of the project.",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}},
    {"type": "function", "function": {"name": "write_file", "description": "Replace a file of the project with new content.",
        "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}}},
    {"type": "function", "function": {"name": "run_tests", "description": "Run the project's tests and return their output.",
        "parameters": {"type": "object", "properties": {}}}},
]


def run_tests(root):
    p = subprocess.run([sys.executable, "-I", "test_intervals.py"], cwd=root, capture_output=True, text=True, timeout=60)
    return p.returncode == 0, (p.stdout + p.stderr)[-2000:]


def tool(root, name, args):
    if name == "run_tests":
        return run_tests(root)[1]
    path = os.path.normpath(os.path.join(root, args.get("path", "")))
    if not path.startswith(root + os.sep):
        return "error: path outside the project"
    if name == "read_file":
        try:
            return open(path).read()
        except OSError as e:
            return f"error: {e}"
    if name == "write_file":
        with open(path, "w") as f:
            f.write(args.get("content", ""))
        return "written"
    return f"error: no tool {name}"


def chat(url, messages):
    body = json.dumps({"model": "local", "messages": messages, "tools": TOOLS, "temperature": 0, "max_tokens": 2048}).encode()
    req = urllib.request.Request(url.rstrip("/") + "/chat/completions", body, {"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=1800) as r:
        return json.load(r)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:7720/v1")
    ap.add_argument("--turns", type=int, default=12)
    a = ap.parse_args()
    root = tempfile.mkdtemp(prefix="corsia-task-")
    for name, text in PROJECT.items():
        with open(os.path.join(root, name), "w") as f:
            f.write(text)
    ok, _ = run_tests(root)
    print(f"project in {root}; tests before: {'pass' if ok else 'fail'}")
    messages = [
        {"role": "system", "content": "You are a coding assistant working in a small Python project. Use the tools to read files, fix the code and run the tests. Change intervals.py only. Stop when the tests pass."},
        {"role": "user", "content": "The tests in test_intervals.py fail. Find the bug in intervals.py and fix it."},
    ]
    t0 = time.time()
    tokens = 0
    turn = 0
    for turn in range(1, a.turns + 1):
        r = chat(a.url, messages)
        tokens += r.get("usage", {}).get("completion_tokens", 0)
        msg = r["choices"][0]["message"]
        messages.append({k: v for k, v in msg.items() if k in ("role", "content", "tool_calls")})
        calls = msg.get("tool_calls") or []
        if not calls:
            print(f"turn {turn}: answered without a tool: {(msg.get('content') or '').strip()[:200]}")
            break
        for c in calls:
            name = c["function"]["name"]
            try:
                args = json.loads(c["function"].get("arguments") or "{}")
            except json.JSONDecodeError:
                args = {}
            result = tool(root, name, args)
            shown = ", ".join(f"{k}={v!r}"[:60] for k, v in args.items() if k != "content")
            extra = f" with {len(args.get('content', ''))} characters" if name == "write_file" else ""
            print(f"turn {turn}: {name}({shown}){extra}")
            messages.append({"role": "tool", "tool_call_id": c.get("id", name), "content": result})
        if run_tests(root)[0] and any(c["function"]["name"] == "run_tests" for c in calls):
            break
    ok, out = run_tests(root)
    secs = time.time() - t0
    print(f"result: tests {'PASS' if ok else 'FAIL'} after {turn} turns, {secs:.0f} s, {tokens} tokens written ({tokens / max(secs, 1):.1f} tok/s overall)")
    print(out.strip())
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
